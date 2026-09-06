#!/usr/bin/env python3
"""Fail when mutation coverage on the Rust core drops below its recorded floor.

Why this gate exists
--------------------

`cargo test` answers "do the tests pass". It cannot answer "would the tests
notice if the code were wrong", and those come apart quietly. The first
`cargo-mutants` run against this workspace made the point in six mutants:

    MISSED crates/sunrise-sync/src/backoff.rs:61:37:
           replace * with + in Backoff::next_delay

`Backoff::next_delay` could add where it multiplies and the whole suite stayed
green. Nothing in `sunrise-sync` pins the multiplier.

`docs/10-cross-cutting/testing.md` requires >= 90% caught mutations for release
sign-off. That number is a destination, not a starting point, and a gate that
fails from day one gets disabled in a week. So this compares against a
*committed baseline* instead: the score may rise and may hold, and may not
fall. The baseline ratchets, and the 90% target is what it ratchets toward.

What counts as caught
---------------------

    caught_pct = caught / (caught + missed + timeout)

Timeouts sit in the denominator and not the numerator on purpose. A mutant that
hung is a mutant no test refuted; scoring it as caught would let an infinite
loop improve the number.

Unviable mutants — ones that do not compile — are excluded from both sides.
They are an artifact of mutating typed code, not a statement about the tests,
and their count varies with rustc rather than with anything a person did.

Which leaves a crate whose mutants are *all* unviable with a zero denominator
and no rate at all. That is not a pass: it means a crate in scope produced
nothing anyone could score, and the cause is upstream — an `exclude_re` that
matched too much, or a crate that stopped compiling under mutation. It is
reported and it fails.

Sharding
--------

A full pass over the four scoped crates is several thousand mutants, so CI
runs it in shards and each shard writes its own `outcomes.json`. This accepts
any number of them and aggregates by crate before comparing, so a sharded run
and a single-process run produce the same verdict.

That aggregation has one failure mode worth naming. A shard whose runner died
contributes no outcomes, and the mutants it would have caught are simply absent
from the numerator — a lost shard and a deleted test look identical in the
arithmetic, and the gate would blame the tests for an infrastructure failure.
`--expect-shards crate=N,...` closes that: it says how many files each crate's
outcomes should arrive in, and a crate that arrives short is reported as a
broken run and excluded from the floor comparison rather than scored on a
partial denominator. CI passes the counts from its own matrix.

Because the flag and the matrix are two copies of one fact, they can drift. So
when the flag is supplied it must account for every crate the run produced: a
crate with outcomes and no expectation is a configuration error, not a silently
unchecked crate.

Tolerance
---------

Mutation results are deterministic given the same tree, with one exception:
a timeout depends on how loaded the machine was. `--tolerance` (default 0.5
percentage points) absorbs exactly that much and no more. It is not a licence
to lose coverage; it is the width of the timeout flake.

Usage
-----

    mutants-gate.py OUTCOMES... [--baseline mutants/baseline.json]
                                [--tolerance 0.5] [--update]
                                [--expect-shards crate=N,...]
                                [--allow-partial]

`--update` rewrites the baseline from this run instead of judging it. That is
how the first baseline is recorded and how an intentional improvement is
banked; it is deliberately a separate, explicit invocation rather than
something the gate does on its own when the number goes up.

It writes whatever it is handed, which is the whole risk: one shard of a
six-shard crate would be banked as that crate's floor, measured on a sixth of
its mutants and permanently too low to catch anything. So `--update` requires
`--expect-shards`, or an explicit `--allow-partial` that says on the way past
that the floors describe only what ran.

Every crate that appears in a run must carry a floor. A measured crate with no
recorded floor is a crate this gate is not protecting, so it fails rather than
noting it in passing — an unenforced crate that reads as a warning is how most
of a workspace's mutants end up scored and then ignored.

Exit 0 clean, 1 on a regression, on a crate with no recorded floor, on a crate
with nothing scorable, on a run whose shards did not arrive as expected, on an
outcomes file supplied more than once, or on an `--update` that names neither
--expect-shards nor --allow-partial, 2 if the gate could not run at all (which
is a failure, not a pass).

The last two are deliberately 1 and not 2, by the same argument: the run itself
is fine and the gate read it, the invocation is what is wrong, and a caller that
treats 2 as "infrastructure" should not be told to re-run shards over a bad
command line. The `--update` refusal is judged on argv alone, before the
outcomes are read at all, so it is the one exit 1 that can be reached by a run
with nothing wrong with it at all — and the one that outranks every exit 2 the
gate itself returns: a caller who cannot record a floor is told which flag is
missing rather than which file, because fixing the file would not let the
command succeed. argparse gets there first with its own 2, so an `--update`
with no outcomes named at all is a usage error and not this.

Every route named above is asserted in `test_mutants_gate.py` beside this file,
which synthesises its own outcomes and runs in about a second — `mise run
mutants-gate-test`, and the `Mutation gate contract` job on every CI run,
nightly or not, unlike the mutation jobs it guards. This
paragraph and those assertions are two statements of one contract, and the
tests are the half that cannot quietly stop being true.
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import sys

# `summary` values cargo-mutants writes into outcomes.json.
CAUGHT = "CaughtMutant"
MISSED = "MissedMutant"
TIMEOUT = "Timeout"
UNVIABLE = "Unviable"


class CannotRun(Exception):
    """The gate could not evaluate the run at all.

    Distinct from a verdict. Every path that raises this exits 2, because the
    alternative — reporting a run nobody scored as a run that passed — is the
    one outcome a gate must never produce.
    """


def crate_of(path: str) -> str | None:
    """`crates/sunrise-sync/src/backoff.rs` -> `sunrise-sync`.

    Grouping by crate rather than by file is what makes the baseline readable:
    a per-file floor would churn on every rename, and a single workspace-wide
    number would let a well-tested crate hide a bare one.
    """
    parts = pathlib.PurePosixPath(path).parts
    if len(parts) >= 2 and parts[0] == "crates":
        return parts[1]
    return None


def malformed(baseline: object) -> str | None:
    """Say how a parsed baseline fails to be one, or None if it is fine.

    `mutants/baseline.json` is hand-edited every time a floor moves, so it
    is the one input to this gate that a person types. A file that parses
    as JSON but is not shaped like a baseline used to reach
    `recorded.get(crate).get("caught_pct")` and die on an AttributeError —
    and an uncaught exception exits 1, which is this gate's code for
    "coverage regressed". A typo in the floors would have been reported as
    a test regression, with a traceback where the crate names go.

    Structure only. Whether the numbers in it are the right numbers is not
    something any check here can know.
    """
    if not isinstance(baseline, dict):
        return f"top level is {type(baseline).__name__}, expected an object"
    crates = baseline.get("crates", {})
    if not isinstance(crates, dict):
        return f'"crates" is {type(crates).__name__}, expected an object'
    for crate, entry in crates.items():
        if not isinstance(entry, dict):
            return (f'"crates.{crate}" is {type(entry).__name__}, '
                    "expected an object")
        floor = entry.get("caught_pct")
        # bool is an int as far as isinstance is concerned, and `true` is
        # not a floor.
        if floor is not None and (isinstance(floor, bool)
                                  or not isinstance(floor, (int, float))):
            return (f'"crates.{crate}.caught_pct" is '
                    f"{type(floor).__name__}, expected a number")
    return None


def parse_expect_shards(spec: str) -> dict[str, int]:
    """`sunrise-core=4,sunrise-sync=1` -> {"sunrise-core": 4, "sunrise-sync": 1}."""
    expected: dict[str, int] = {}
    for item in spec.split(","):
        item = item.strip()
        if not item:
            continue
        crate, sep, count = item.partition("=")
        if not crate or not sep or not count.isdigit() or int(count) < 1:
            raise argparse.ArgumentTypeError(
                f"cannot parse {item!r}; expected crate=N with N >= 1"
            )
        expected[crate] = int(count)
    if not expected:
        raise argparse.ArgumentTypeError("expected at least one crate=N")
    return expected


def tally(
    paths: list[pathlib.Path],
) -> tuple[dict[str, dict[str, int]], dict[str, set[str]]]:
    """Aggregate every outcomes.json into per-crate counts.

    Also returns, per crate, the set of input files that carried any of its
    mutants. That is the only evidence available that all of a crate's shards
    arrived: a shard that never ran leaves no file and no trace in the counts,
    so without counting the files a lost shard is indistinguishable from a
    crate whose tests got worse.
    """
    counts: dict[str, dict[str, int]] = {}
    sources: dict[str, set[str]] = {}
    for path in paths:
        if not path.is_file():
            # The common shape of this is a quoted glob that matched nothing
            # and reached argv verbatim. A shell with nullglob set instead
            # drops it, leaving an empty argument. Both mean the outcomes
            # never arrived, which is a broken run and not an empty one.
            hint = ("an unexpanded glob — it matched no files"
                    if any(c in str(path) for c in "*?[")
                    else "a missing file or an empty argument")
            raise CannotRun(f"not a readable file: {str(path)!r} — {hint}")
        try:
            document = json.loads(path.read_text())
        except (OSError, json.JSONDecodeError) as error:
            raise CannotRun(f"cannot read {path}: {error}") from error

        for outcome in document.get("outcomes", []):
            scenario = outcome.get("scenario")
            # The baseline scenario is a dict-shaped `{"Mutant": {...}}` for
            # mutants and the bare string "Baseline" for the unmutated run.
            if not isinstance(scenario, dict):
                continue
            mutant = scenario.get("Mutant")
            if not isinstance(mutant, dict):
                continue
            crate = crate_of(mutant.get("package", "") or mutant.get("file", ""))
            if crate is None:
                crate = mutant.get("package")
            if not crate:
                continue

            bucket = counts.setdefault(
                crate, {CAUGHT: 0, MISSED: 0, TIMEOUT: 0, UNVIABLE: 0}
            )
            # Keyed on the resolved path: `a/outcomes.json` and
            # `./a/outcomes.json` are one shard, and counting them as two
            # would report a crate complete that is missing a real one.
            sources.setdefault(crate, set()).add(os.path.realpath(path))
            summary = outcome.get("summary")
            if summary in bucket:
                bucket[summary] += 1
    return counts, sources


def caught_pct(bucket: dict[str, int]) -> float | None:
    denominator = bucket[CAUGHT] + bucket[MISSED] + bucket[TIMEOUT]
    if denominator == 0:
        return None
    return round(100.0 * bucket[CAUGHT] / denominator, 2)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("outcomes", nargs="+", type=pathlib.Path)
    parser.add_argument("--baseline", type=pathlib.Path,
                        default=pathlib.Path("mutants/baseline.json"))
    parser.add_argument("--tolerance", type=float, default=0.5)
    parser.add_argument("--update", action="store_true")
    parser.add_argument("--expect-shards", type=parse_expect_shards, default={},
                        metavar="crate=N,...",
                        help="how many outcomes files each crate should arrive "
                             "in; a crate that arrives short is a broken run, "
                             "not a coverage regression")
    parser.add_argument("--allow-partial", action="store_true",
                        help="with --update, record floors without a "
                             "completeness check; the floors then describe "
                             "exactly what ran and nothing more")
    args = parser.parse_args()

    # First, and on argv alone. This asks nothing about the run: it is the
    # question of whether the caller is in a position to record a floor at all,
    # and the answer does not depend on a single outcome. Judged here so a
    # caller who forgot the counts is told about the counts — not about a
    # baseline they were never going to write, or the scorability of a run they
    # were never going to bank, which is what they got while this sat four
    # checks further down. The message below says why the counts matter.
    if args.update and not args.expect_shards and not args.allow_partial:
        print(
            "refusing to record a floor from an unverified set of runs.\n"
            "\n"
            "--update banks whatever it is handed. Hand it one shard of a "
            "six-shard\ncrate and that shard becomes the crate's floor: a "
            "rate measured over a\nsixth of its mutants, recorded as "
            "though it covered all of them, and\nthereafter too low to "
            "fail on anything.\n"
            "\n"
            "Say what should be here:\n"
            "  --expect-shards sunrise-domain=6,...  checked against what "
            "arrived\n"
            "  --allow-partial                       unchecked, for a "
            "deliberately partial floor",
            file=sys.stderr,
        )
        return 1

    # One file named twice is a duplicated argument, not a duplicated shard —
    # overlapping globs, or a path listed twice by hand. It doubles every
    # mutant in that file, and the completeness check counts distinct files, so
    # it would see one file where two were passed and call the crate complete.
    # Deduplicated here so nothing is counted twice, then reported below:
    # accepting it quietly is how `--update` banks doubled counts as a floor.
    unique: list[pathlib.Path] = []
    repeated: dict[str, int] = {}
    resolved: set[str] = set()
    for path in args.outcomes:
        real = os.path.realpath(path)
        if real in resolved:
            repeated[real] = repeated.get(real, 1) + 1
        else:
            resolved.add(real)
            unique.append(path)

    try:
        counts, sources = tally(unique)
    except CannotRun as error:
        print(error, file=sys.stderr)
        return 2
    if not counts:
        print("no mutants found in the supplied outcomes; refusing to pass",
              file=sys.stderr)
        return 2

    if repeated:
        print("\nduplicate artifacts — the same outcomes file was supplied "
              "more than once:", file=sys.stderr)
        for real, times in sorted(repeated.items()):
            print(f"  {real} (x{times})", file=sys.stderr)
        print(
            "\nEvery mutant in it would be counted that many times, and the "
            "shard check\ncounts distinct files, so the crate would still "
            "look complete. Overlapping\nglobs are the usual cause. Narrow "
            "the inputs and re-run the gate.",
            file=sys.stderr,
        )
        return 1

    # The flag and ci.yml's matrix are two copies of one fact, and the copy
    # that silently stops covering a crate is the dangerous one: a crate added
    # to the matrix and not to the flag would be scored with no completeness
    # check for as long as nobody noticed. So if the flag is supplied at all,
    # it has to account for everything the run produced.
    if args.expect_shards:
        undeclared = sorted(set(counts) - set(args.expect_shards))
        if undeclared:
            for crate in undeclared:
                print(f"undeclared crate: {crate} has outcomes but no "
                      "expectation in --expect-shards", file=sys.stderr)
                for path in sorted(sources.get(crate, ())):
                    print(f"    from {path}", file=sys.stderr)
            # Two callers reach this, and the advice is not the same for both.
            # In CI the flag and the matrix are the only two things in play. On
            # a laptop the third is out/mutants/, which `mise run
            # mutants-baseline` globs whole: a crate mutated last week is still
            # sitting there and still gets scored, so the crate the gate is
            # complaining about may be one nobody meant to run at all. Naming
            # only the matrix sent that reader to edit a workflow file that has
            # nothing to do with what they are looking at.
            print(
                "\nEvery crate the run produces needs a shard count, or it is "
                "scored with no\ncheck that all of it arrived. Two ways to be "
                "here:\n"
                "\n"
                "  in CI, --expect-shards and ci.yml's matrix have drifted "
                "apart. Add the\n  crate to --expect-shards in "
                ".github/workflows/ci.yml, or drop it from\n  the matrix.\n"
                "\n"
                "  locally, out/mutants/ holds a run for a crate this command "
                "did not\n  declare — often a stale one, since "
                "`mise run mutants-baseline` scores\n  every directory under "
                "it together. Declare the crate, or delete the\n  directory "
                "listed above.",
                file=sys.stderr,
            )
            return 2

    # Judged before anything else: a crate missing a shard has a numerator that
    # never ran, and every number computed from it is a lie in the direction of
    # "the tests got worse". Reported, excluded from the comparison, never
    # written to the baseline.
    mismatched = []
    for crate, expected in sorted(args.expect_shards.items()):
        seen = len(sources.get(crate, ()))
        if seen != expected:
            cause = "shards missing" if seen < expected else "duplicate artifacts"
            mismatched.append((crate, seen, expected, cause))

    broken = {crate for crate, _, _, _ in mismatched}

    # A crate whose every mutant was unviable has a zero denominator: no rate
    # to compare and no floor to record. Skipping it quietly is the same
    # silent-unscored failure --expect-shards exists to close, reached by
    # another route — an exclude_re that swallowed the crate, or a crate that
    # stopped compiling under mutation, would both read as a clean pass.
    unscorable = [
        (crate, bucket[UNVIABLE])
        for crate, bucket in sorted(counts.items())
        if crate not in broken and caught_pct(bucket) is None
    ]

    if mismatched:
        print("\nmutation run does not match the expected shards — an "
              "infrastructure failure,\nnot a test regression:",
              file=sys.stderr)
        for crate, seen, expected, cause in mismatched:
            mutants = sum(counts.get(crate, {}).values())
            print(f"  {crate}: {seen}/{expected} shards, {mutants} mutants "
                  f"— {cause}", file=sys.stderr)
        if any(cause == "shards missing" for *_, cause in mismatched):
            print(
                "\nA shard whose runner died contributes no outcomes, so the "
                "crate scores low\nfor a reason no test change would explain. "
                "Re-run the failed shards rather\nthan touching the tests or "
                "the floor.",
                file=sys.stderr,
            )
        if any(cause == "duplicate artifacts" for *_, cause in mismatched):
            print(
                "\nMore files than shards means one shard's outcomes arrived "
                "twice — an artifact\ndownloaded into two directories, or a "
                "stale run left beside a fresh one. Every\nmutant in it is "
                "then counted twice. Narrow the inputs and re-run the gate.",
                file=sys.stderr,
            )
        print("\nEither way these crates were not scored.", file=sys.stderr)
        if args.update:
            print("\nrefusing to record a floor from a mismatched run",
                  file=sys.stderr)
            return 1

    if unscorable and args.update:
        for crate, unviable in unscorable:
            print(f"{crate}: no scorable mutants ({unviable} unviable)",
                  file=sys.stderr)
        print("refusing to record a floor with no scorable mutants",
              file=sys.stderr)
        return 1

    try:
        baseline = json.loads(args.baseline.read_text())
    except (OSError, json.JSONDecodeError) as error:
        print(f"cannot read {args.baseline}: {error}", file=sys.stderr)
        return 2

    # Same exit as an unreadable one, for the same reason: a baseline the
    # gate cannot use is a run nobody scored, and the one thing it must not
    # do with that is return a verdict.
    problem = malformed(baseline)
    if problem is not None:
        print(f"cannot use {args.baseline}: {problem}", file=sys.stderr)
        return 2

    recorded = baseline.setdefault("crates", {})

    if args.update:
        if args.allow_partial and not args.expect_shards:
            print(f"--allow-partial: recording from {len(args.outcomes)} "
                  "outcomes file(s) with no completeness check. These floors "
                  "describe what ran, not the crates.")
        for crate, bucket in sorted(counts.items()):
            recorded[crate] = {
                "caught": bucket[CAUGHT],
                "missed": bucket[MISSED],
                "timeout": bucket[TIMEOUT],
                "unviable": bucket[UNVIABLE],
                "caught_pct": caught_pct(bucket),
            }
        args.baseline.write_text(json.dumps(baseline, indent=4, sort_keys=True) + "\n")
        print(f"{args.baseline}: recorded {len(counts)} crate(s)")
        return 0

    failures = []
    unfloored = []
    for crate, bucket in sorted(counts.items()):
        measured = caught_pct(bucket)
        if crate in broken:
            continue
        if measured is None:
            print(f"  {crate}: no scorable mutants "
                  f"({bucket[UNVIABLE]} unviable)")
            continue
        shards = len(sources.get(crate, ()))
        expected = args.expect_shards.get(crate)
        tally_line = (
            f"      {shards}/{expected} shards" if expected is not None
            else f"      {shards} shard(s)"
        ) + (
            f", {sum(bucket.values())} mutants "
            f"({bucket[CAUGHT]} caught, {bucket[MISSED]} missed, "
            f"{bucket[TIMEOUT]} timeout, {bucket[UNVIABLE]} unviable)"
        )
        floor = (recorded.get(crate) or {}).get("caught_pct")
        if floor is None:
            # Deliberately not a note. This crate was mutated, scored, and is
            # compared against nothing; treating that as informational is how a
            # gate reports on thousands of mutants while enforcing none of them.
            print(f"  {crate}: {measured}% — NO FLOOR RECORDED")
            print(tally_line)
            unfloored.append((crate, measured))
            continue
        verdict = "ok" if measured >= floor - args.tolerance else "REGRESSED"
        print(f"  {crate}: {measured}% vs floor {floor}% — {verdict}")
        print(tally_line)
        if verdict == "REGRESSED":
            failures.append((crate, measured, floor))

    # The per-crate listing above is the evidence for the summaries below, and
    # in CI both streams land in one log. Flush so they land in that order.
    sys.stdout.flush()

    if failures:
        print("\nmutation coverage regressed:", file=sys.stderr)
        for crate, measured, floor in failures:
            print(f"  {crate}: {measured}% < {floor}%", file=sys.stderr)
        print(
            "\nEither add a test that kills the surviving mutants "
            "(mutants.out/missed.txt lists them), or — if the floor was wrong — "
            "lower it deliberately in a commit that says why.",
            file=sys.stderr,
        )

    if unscorable:
        print("\nno scorable mutants:", file=sys.stderr)
        for crate, unviable in unscorable:
            print(f"  {crate}: no scorable mutants ({unviable} unviable)",
                  file=sys.stderr)
        print(
            "\nEvery mutant in these crates failed to compile, so the rate has "
            "a zero\ndenominator and there is nothing to compare. That is a "
            "question about the\nbuild or about .cargo/mutants.toml's "
            "exclude_re, not about the tests.",
            file=sys.stderr,
        )

    if unfloored:
        print("\nno floor recorded for:", file=sys.stderr)
        for crate, measured in unfloored:
            print(f"  {crate}: measured {measured}%", file=sys.stderr)
        if mismatched:
            # No command here on purpose. `--update` refuses a mismatched run,
            # so any invocation printed at this point is one the reader would
            # paste and watch fail — and the failure they would then be trying
            # to fix is not the one they were sent here for. The floor has to
            # wait for a run the gate can score.
            #
            # Split by cause, because the two remedies are opposites: one run
            # is missing files and wants repeating, the other has too many and
            # wants narrowing. Telling someone whose artifacts arrived twice to
            # re-run the missing shards describes a run that did not happen and
            # prescribes the one thing that reproduces theirs.
            missing = [c for c, _, _, cause in mismatched
                       if cause == "shards missing"]
            duplicated = [c for c, _, _, cause in mismatched
                          if cause == "duplicate artifacts"]
            print("\nNo floor can be recorded from this run: --update refuses "
                  "a mismatched run\nrather than bank a rate measured over "
                  "the wrong population.", file=sys.stderr)
            if missing:
                print(
                    f"\n  {', '.join(missing)} arrived short. Repeat the run "
                    "— or just the shards\n  that went missing — and record "
                    "the floor from a complete one.",
                    file=sys.stderr,
                )
            if duplicated:
                print(
                    f"\n  {', '.join(duplicated)} arrived more than once, so "
                    "every mutant in the\n  repeated file counts twice. "
                    "Narrow the inputs to one file per shard\n  and record "
                    "from that; the run itself is fine.",
                    file=sys.stderr,
                )
        else:
            # Assembled from this run rather than printed as `<crate>=<N>`: a
            # remedy with angle brackets in it is four redirections when
            # pasted into a shell, so the reader gets `No such file or
            # directory` from bash and never reaches the task at all.
            #
            # Every crate the run produced, not only the unfloored ones. The
            # mise task scores every directory under out/mutants/ together, so
            # a spec naming just the crate that failed comes straight back as
            # an undeclared crate for the ones that passed.
            spec = ",".join(
                f"{crate}="
                f"{args.expect_shards.get(crate) or len(sources.get(crate, ()))}"
                for crate in sorted(counts)
            )
            print(
                "\nEvery crate in scope carries a floor or it is not "
                "enforced. Record one from this run:\n"
                "\n"
                "  locally, from out/:\n"
                f"    mise run mutants-baseline --expect-shards {spec}\n"
                "\n"
                "  from a nightly run's artifacts:\n"
                "    gh run download <run-id> --pattern 'mutants-*' "
                "--dir outcomes\n"
                "    .github/scripts/mutants-gate.py outcomes/*/outcomes.json "
                "\\\n"
                "        --update --expect-shards <the counts in ci.yml's "
                "matrix>\n"
                "\n"
                "and commit mutants/baseline.json saying what the number is.",
                file=sys.stderr,
            )

    if failures or unfloored or mismatched or unscorable:
        return 1

    target = baseline.get("target_caught_pct")
    if target is not None:
        below = [c for c, b in counts.items()
                 if (caught_pct(b) or 0) < target]
        if below:
            print(f"\nBelow the {target}% release target: {', '.join(sorted(below))}")
            print("Not a failure — the floor ratchets toward the target.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
