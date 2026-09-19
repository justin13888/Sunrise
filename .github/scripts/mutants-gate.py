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
                                [--allow-partial] [--command TEXT]
    mutants-gate.py --record-revision PATH

`--update` rewrites the baseline from this run instead of judging it. That is
how the first baseline is recorded and how an intentional improvement is
banked; it is deliberately a separate, explicit invocation rather than
something the gate does on its own when the number goes up.

Provenance, and why the revision is not this command's HEAD
-----------------------------------------------------------

Every floor records the revision it was measured at, and `--update` does not
ask git for it. It reads it from a `revision.json` written beside each
`outcomes.json` while the measurement was running — by `mise.toml`'s `mutants`
task and by `ci.yml`'s `mutants` matrix, both of which call
`--record-revision` — and refuses when an outcomes file carries no stamp or
when two stamps name different revisions.

The reason is the gap between the two. A `sunrise-core` pass is about five
hours and a `sunrise-domain` pass about two; the tests that motivated the run
get committed while it is going, and `mise run mutants-baseline` may not be run
until the next day. A floor stamped with HEAD at recording time therefore names
a revision that does not produce the number beside it, which is verbatim the
defect this field was added to stop. The same stamp carries a `dirty` flag from
`git status --porcelain`, because a floor taken on a modified tree does not
reproduce at the named revision either.

`--command` is what gets banked as `provenance.command`. It exists so the field
is a recipe rather than a trace: `mise run mutants-baseline --expect-shards
sunrise-sync=1` is what a person runs, and this script's own argv is the inside
of that. It defaults to the argv for the one caller that has no friendlier
form — a person handing the gate a nightly's artifacts by hand.

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
import datetime
import json
import os
import pathlib
import shlex
import subprocess
import sys

# `summary` values cargo-mutants writes into outcomes.json.
CAUGHT = "CaughtMutant"
MISSED = "MissedMutant"
TIMEOUT = "Timeout"
UNVIABLE = "Unviable"

# What every recorded floor has to say about itself. Required by
# `malformed()` and written by `--update` — one change, not two, because
# the writer replaces each crate entry wholesale and a requirement without
# a writer refuses the file the repository's own task produces.
#
# The three have fixed meanings, and they are fixed because the field is
# only comparable across floors if they are:
#
# * `sha` — the full 40-hex revision the mutants were MEASURED at, not
#   the one the floor was recorded at. Those are different revisions
#   whenever the measurement is long enough to be worth recording, which
#   is always: a `sunrise-domain` pass is two hours and a `sunrise-core`
#   pass is five, and the tests that motivated the run get committed in
#   between. A floor stamped with the recorder's HEAD names a revision
#   that does not produce the number beside it.
# * `date` — the day the MEASUREMENT ran, for the same reason.
# * `command` — the invocation a human would run to reproduce it, not
#   this script's own argv. `mise run mutants-baseline --expect-shards
#   sunrise-sync=1` is a recipe; `mutants-gate.py out/…/outcomes.json
#   --update --expect-shards sunrise-sync=1` is the inside of one.
PROVENANCE_FIELDS = ("sha", "date", "command")

# Written beside `outcomes.json` by whoever ran the measurement, and read
# by `--update`. This file is the whole of the mechanism that makes `sha`
# and `date` mean the measurement rather than the recording: cargo-mutants
# records no revision of its own, so without it the two are not
# correlatable by anything.
REVISION_FILENAME = "revision.json"

# Optional, and typed when present. `--update` writes it for every floor
# it banks; the floors recorded before measurement-time capture existed
# carry no `dirty` at all, because whether those working trees were clean
# is not something anybody can now establish, and writing `false` would
# be the invention the provenance requirement exists to stop. Absent
# means unknown, not clean.
DIRTY_FIELD = "dirty"


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


def malformed(baseline: object, skip: object = (),
              require_provenance: bool = True) -> str | None:
    """Say how a parsed baseline fails to be one, or None if it is fine.

    `mutants/baseline.json` is hand-edited every time a floor moves, so it
    is the one input to this gate that a person types. A file that parses
    as JSON but is not shaped like a baseline used to reach
    `recorded.get(crate).get("caught_pct")` and die on an AttributeError —
    and an uncaught exception exits 1, which is this gate's code for
    "coverage regressed". A typo in the floors would have been reported as
    a test regression, with a traceback where the crate names go.

    A crate with a `caught_pct` must also carry a `provenance` object with
    `sha`, `date` and `command`, each a non-empty string. A floor is a
    standing constraint on every future run, and one that cannot say what
    measured it cannot be compared with the one it replaced, re-taken, or
    argued with. `--update` writes all three, so the requirement and the
    writer are the same change; requiring it here without writing it there
    would make `mise run mutants-baseline` emit a file this gate refuses.

    Structure only. Whether the numbers in it are the right numbers is not
    something any check here can know. That limit is worth stating twice
    now that provenance is required, because requiring it looks like more
    than it is: this checks that a sha, a date and a command are *present
    and well-formed*, and it cannot check that they are true. Whether the
    named revision carried the tests the floor beside it is worth, and
    whether a percentage quoted in prose was computed by the rule it names,
    are both decidable only by re-running the campaign at that revision —
    which is the work the floor exists to avoid. Those two defects are
    real, they have both occurred here, and this gate does not catch them.

    `skip` and `require_provenance` exist for the bootstrap, and only for
    it. A baseline written before provenance was required cannot be
    re-recorded at all if this runs in full before `--update` gets to
    write: every crate the run did not measure fails the check and the
    write never happens, so the only way out of a legacy file is to hand
    edit the one field the design says must never be hand-added — after a
    measurement that costs hours. Reachable by reverting a floor-recording
    commit, by `--update` against a baseline restored from an older ref,
    or by running this script against a release branch's baseline. So
    `--update` validates the shape of the entries it is not replacing,
    without the provenance requirement, and the full check runs again on
    the merged document before the verdict is returned.
    """
    if not isinstance(baseline, dict):
        return f"top level is {type(baseline).__name__}, expected an object"
    crates = baseline.get("crates", {})
    if not isinstance(crates, dict):
        return f'"crates" is {type(crates).__name__}, expected an object'
    for crate, entry in crates.items():
        if crate in skip:
            continue
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
        # Only a crate that carries a floor needs to say where the floor
        # came from. An entry with no `caught_pct` constrains nothing, so
        # there is nothing yet to account for.
        if floor is None or not require_provenance:
            continue
        origin = entry.get("provenance")
        if not isinstance(origin, dict):
            return (f'"crates.{crate}.provenance" is '
                    f"{type(origin).__name__}, expected an object with "
                    f"{', '.join(PROVENANCE_FIELDS)}")
        for field in PROVENANCE_FIELDS:
            value = origin.get(field)
            if not isinstance(value, str) or not value.strip():
                return (f'"crates.{crate}.provenance.{field}" is '
                        f"{type(value).__name__}, expected a non-empty "
                        "string")
        # Optional, because the floors taken before the measurement-time
        # capture existed cannot answer it. Typed when present, so a
        # string "false" — which is truthy, and would read as clean to a
        # human and as dirty to the code — cannot get in.
        if DIRTY_FIELD in origin and not isinstance(
                origin[DIRTY_FIELD], bool):
            return (f'"crates.{crate}.provenance.{DIRTY_FIELD}" is '
                    f"{type(origin[DIRTY_FIELD]).__name__}, expected a "
                    "boolean")
    return None


def working_tree_revision() -> tuple[str, bool]:
    """(full sha, dirty) for the tree this is called in.

    Deliberately without `check=True`. With it, the only way `git
    rev-parse HEAD` returns is with a sha, and the empty-stdout arm below
    is unreachable — a guarded case no test can enter, which is worse
    than no guard at all because it reads as one. Without it, every way
    git can decline to answer arrives at the same place: a repository
    with no commits exits 128, a directory that is not a repository exits
    128, and both print nothing to stdout.

    The `OSError` arm is separate and is not the same news: it means git
    is not on `PATH` at all, so the caller has no repository tooling
    rather than no repository.
    """
    try:
        head = subprocess.run(
            ["git", "rev-parse", "HEAD"], capture_output=True, text=True)
        status = subprocess.run(
            ["git", "status", "--porcelain"], capture_output=True, text=True)
    except OSError as error:
        raise CannotRun(
            f"cannot run git to read the measurement revision: {error}. "
            "A floor with blank provenance is what the placeholder rule in "
            "mutants/baseline.json rejects, so nothing was written."
        ) from error
    sha = head.stdout.strip()
    if not sha:
        detail = head.stderr.strip() or "no output"
        raise CannotRun(
            f"`git rev-parse HEAD` printed nothing ({detail}); refusing to "
            "record a measurement that cannot say which revision produced "
            "it. An empty sha has the right shape and says nothing, which "
            "is the placeholder mutants/baseline.json's own rule rejects."
        )
    # Any porcelain output at all means the measurement did not describe a
    # committed tree. Which files differ is not worth banking; that it
    # differed is, because a floor taken on a dirty tree names a revision
    # that does not reproduce it — one of the two defects issue #246 cites.
    return sha, bool(status.stdout.strip())


def write_revision(path: pathlib.Path) -> dict[str, object]:
    """Stamp a measurement with the revision it is being taken at.

    Called by `mise.toml`'s `mutants` task and by `ci.yml`'s `mutants`
    matrix, immediately around the cargo-mutants run, so the answer is
    the tree that was mutated. `--update` runs later — minutes later in
    CI, days later on a laptop, and after the commit whose tests
    motivated the run — and has no way to recover this after the fact.
    """
    sha, dirty = working_tree_revision()
    document = {
        "sha": sha,
        DIRTY_FIELD: dirty,
        "date": datetime.date.today().isoformat(),
    }
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(document, indent=4, sort_keys=True) + "\n")
    return document


def revision_beside(outcomes: pathlib.Path) -> pathlib.Path | None:
    """Find the revision stamp for one outcomes file.

    Two places, because the two producers lay out their output
    differently and neither is wrong. cargo-mutants always writes
    `mutants.out/` under the directory it is given, so the stamp sits
    either in `mutants.out/` beside `outcomes.json` or one level up in
    the run directory the task created.
    """
    for directory in (outcomes.parent, outcomes.parent.parent):
        candidate = directory / REVISION_FILENAME
        if candidate.is_file():
            return candidate
    return None


def measurement_revision(paths: list[pathlib.Path]) -> dict[str, object]:
    """The revision every one of these outcomes was measured at.

    Refuses rather than guessing, in both directions. An outcomes file
    with no stamp beside it is a measurement whose revision nobody
    recorded, and the previous answer to that — `git rev-parse HEAD` at
    recording time — is precisely the defect: run at A for five hours,
    commit the tests that motivated it, record at B, and the file claims
    B for numbers produced at A. Two stamps naming different revisions
    are a floor assembled from two different trees, which is not one
    measurement at all.

    Disagreement is judged on the sha alone. Thirteen CI shards start
    together and can finish either side of midnight UTC, so differing
    dates are ordinary; the earliest is the day the measurement began.
    `dirty` is true if any shard saw a modified tree, because one shard
    measuring something uncommitted is enough to make the whole floor
    unreproducible at the named revision.
    """
    stamps: dict[str, list[dict]] = {}
    unstamped: list[pathlib.Path] = []
    for path in paths:
        candidate = revision_beside(path)
        if candidate is None:
            unstamped.append(path)
            continue
        try:
            document = json.loads(candidate.read_text())
        except (OSError, json.JSONDecodeError) as error:
            raise CannotRun(
                f"cannot read {candidate}: {error}") from error
        sha = document.get("sha") if isinstance(document, dict) else None
        date = document.get("date") if isinstance(document, dict) else None
        if not isinstance(sha, str) or not sha.strip():
            raise CannotRun(
                f'{candidate} records no "sha"; it cannot say which '
                "revision the outcomes beside it were measured at.")
        if not isinstance(date, str) or not date.strip():
            raise CannotRun(
                f'{candidate} records no "date"; it cannot say when the '
                "outcomes beside it were measured.")
        stamps.setdefault(sha.strip(), []).append(document)

    if unstamped:
        raise CannotRun(
            "these outcomes carry no measurement revision:\n"
            + "".join(f"    {path}\n" for path in unstamped)
            + f"\nA floor is stamped with the revision it was MEASURED "
            f"at, which is written to {REVISION_FILENAME} beside "
            "outcomes.json\nwhile the run happens. Recording HEAD now "
            "would name the revision this\ncommand is being run at, "
            "which is a different tree whenever the\nmeasurement was long "
            "enough to be worth recording.\n"
            "\n"
            "Re-run the measurement with `mise run mutants <crate>`, which "
            "writes the\nstamp, or write one by hand beside each outcomes "
            "file naming the revision\nthe numbers actually came from.")

    if len(stamps) > 1:
        listing = "".join(
            f"    {sha}  ({len(documents)} outcomes file(s))\n"
            for sha, documents in sorted(stamps.items()))
        raise CannotRun(
            "the outcomes were measured at more than one revision:\n"
            + listing
            + "\nOne floor cannot be stamped with two revisions, and a "
            "crate scored across\ntwo trees is not a measurement of "
            "either. Re-run the shards that are behind,\nor record only "
            "the outcomes from one revision.")

    sha, documents = next(iter(stamps.items()))
    return {
        "sha": sha,
        "date": min(str(document["date"]).strip() for document in documents),
        DIRTY_FIELD: any(
            bool(document.get(DIRTY_FIELD)) for document in documents),
    }


def provenance(paths: list[pathlib.Path],
               command: str | None) -> dict[str, object]:
    """What produced the floors this run is about to bank.

    `command` is the human-facing invocation, supplied by whoever called
    this — `mise.toml`'s task passes the `mise run mutants-baseline …`
    line a person would type. The fallback is this process's own argv,
    which is the right answer for the one caller that has no friendlier
    form: `mutants-gate.py outcomes/*/outcomes.json --update …` run by
    hand against a nightly's artifacts is itself the reproduction recipe.
    """
    origin = measurement_revision(paths)
    return {
        "sha": origin["sha"],
        "date": origin["date"],
        DIRTY_FIELD: origin[DIRTY_FIELD],
        "command": command if command else shlex.join(sys.argv),
    }


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
    parser.add_argument("outcomes", nargs="*", type=pathlib.Path)
    parser.add_argument("--record-revision", type=pathlib.Path,
                        metavar="PATH",
                        help="write the current revision and dirty flag to "
                             "PATH and exit; run beside a measurement so "
                             "--update can stamp its floors with the "
                             "revision they were measured at")
    parser.add_argument("--command", metavar="TEXT",
                        help="with --update, the human-facing invocation to "
                             "record as each floor's provenance.command; "
                             "defaults to this process's argv")
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

    # Before anything else, and it scores nothing: this mode exists to be
    # called from inside the task that runs the measurement, where there
    # are no outcomes yet.
    if args.record_revision is not None:
        try:
            document = write_revision(args.record_revision)
        except CannotRun as error:
            print(error, file=sys.stderr)
            return 2
        state = "dirty" if document[DIRTY_FIELD] else "clean"
        print(f"{args.record_revision}: {document['sha']} ({state})")
        return 0

    if not args.outcomes:
        parser.error(
            "at least one outcomes file is required unless "
            "--record-revision is given")

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
    #
    # `--update` is held to less here and to the same thing afterwards.
    # The entries it is about to replace wholesale are not worth
    # validating — whatever is wrong with them is about to be gone — and
    # the provenance requirement is deferred to the merged document,
    # because applying it now to a baseline written before provenance
    # existed makes that file impossible to repair by re-recording, which
    # is the only way the design allows it to be repaired at all.
    problem = malformed(
        baseline,
        skip=set(counts) if args.update else (),
        require_provenance=not args.update,
    )
    if problem is not None:
        print(f"cannot use {args.baseline}: {problem}", file=sys.stderr)
        return 2

    recorded = baseline.setdefault("crates", {})

    if args.update:
        if args.allow_partial and not args.expect_shards:
            print(f"--allow-partial: recording from {len(args.outcomes)} "
                  "outcomes file(s) with no completeness check. These floors "
                  "describe what ran, not the crates.")
        # Taken once, before anything is written, so every crate banked by
        # one invocation names one revision — and so a failure to read it
        # aborts before the file is touched rather than halfway through it.
        try:
            origin = provenance(unique, args.command)
        except CannotRun as error:
            print(error, file=sys.stderr)
            return 2
        if origin[DIRTY_FIELD]:
            # Recorded, not refused. A floor measured on a dirty tree is
            # still a measurement of something, and refusing it would
            # throw away hours of work over a stray file in `out/`. What
            # it is not is reproducible at the revision beside it, and
            # that has to be visible in the file rather than only here.
            print(f"warning: measured at {origin['sha']} with a modified "
                  "working tree; recording the floor with "
                  f'"{DIRTY_FIELD}": true, which says the named revision '
                  "does not reproduce it on its own.", file=sys.stderr)
        for crate, bucket in sorted(counts.items()):
            # Wholesale replacement, as before: an entry merged into would
            # keep the previous run's counts beside this run's rate. That
            # is why `provenance` has to be written here — a key added by
            # hand does not survive the next `mise run mutants-baseline`.
            recorded[crate] = {
                "caught": bucket[CAUGHT],
                "missed": bucket[MISSED],
                "timeout": bucket[TIMEOUT],
                "unviable": bucket[UNVIABLE],
                "caught_pct": caught_pct(bucket),
                "provenance": dict(origin),
            }
        args.baseline.write_text(json.dumps(baseline, indent=4, sort_keys=True) + "\n")
        print(f"{args.baseline}: recorded {len(counts)} crate(s)")

        # The deferred half. The write went in first on purpose: a
        # legacy baseline is repaired one recorded crate at a time, and
        # discarding the crate this run measured because a crate it did
        # not measure is still unaccounted would make the repair
        # impossible by the only route the design allows.
        problem = malformed(baseline)
        if problem is not None:
            print(
                f"\n{args.baseline} was written and is not yet a usable "
                f"baseline: {problem}\n"
                "\n"
                "The crates this run measured are recorded. The rest date "
                "from before a floor\nhad to say what produced it, and a "
                "`provenance` added by hand does not survive\nthe next "
                "`mise run mutants-baseline` — the update path replaces "
                "each crate entry\nwholesale. Re-record them: measure each "
                "one and run this again. Until then the\nnightly "
                "`Mutation coverage gate` cannot read this file.",
                file=sys.stderr)
            return 2
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
