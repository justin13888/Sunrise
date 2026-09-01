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

Sharding
--------

A full pass over the four scoped crates is several thousand mutants, so CI
runs it in shards and each shard writes its own `outcomes.json`. This accepts
any number of them and aggregates by crate before comparing, so a sharded run
and a single-process run produce the same verdict.

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

`--update` rewrites the baseline from this run instead of judging it. That is
how the first baseline is recorded and how an intentional improvement is
banked; it is deliberately a separate, explicit invocation rather than
something the gate does on its own when the number goes up.

Every crate that appears in a run must carry a floor. A measured crate with no
recorded floor is a crate this gate is not protecting, so it fails rather than
noting it in passing — an unenforced crate that reads as a warning is how most
of a workspace's mutants end up scored and then ignored.

Exit 0 clean, 1 on a regression or on a crate with no recorded floor, 2 if the
gate could not run at all (which is a failure, not a pass).
"""

from __future__ import annotations

import argparse
import json
import pathlib
import sys

# `summary` values cargo-mutants writes into outcomes.json.
CAUGHT = "CaughtMutant"
MISSED = "MissedMutant"
TIMEOUT = "Timeout"
UNVIABLE = "Unviable"
# The unmutated baseline scenario cargo-mutants runs first. Not a mutant.
BASELINE_SCENARIO = "Baseline"


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


def tally(paths: list[pathlib.Path]) -> dict[str, dict[str, int]]:
    """Aggregate every outcomes.json into per-crate counts."""
    counts: dict[str, dict[str, int]] = {}
    for path in paths:
        try:
            document = json.loads(path.read_text())
        except (OSError, json.JSONDecodeError) as error:
            raise SystemExit(f"cannot read {path}: {error}") from error

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
            summary = outcome.get("summary")
            if summary in bucket:
                bucket[summary] += 1
    return counts


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
    args = parser.parse_args()

    counts = tally(args.outcomes)
    if not counts:
        print("no mutants found in the supplied outcomes; refusing to pass",
              file=sys.stderr)
        return 2

    try:
        baseline = json.loads(args.baseline.read_text())
    except (OSError, json.JSONDecodeError) as error:
        print(f"cannot read {args.baseline}: {error}", file=sys.stderr)
        return 2

    recorded = baseline.setdefault("crates", {})

    if args.update:
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
        if measured is None:
            continue
        counted = (
            f"({bucket[CAUGHT]} caught, {bucket[MISSED]} missed, "
            f"{bucket[TIMEOUT]} timeout, {bucket[UNVIABLE]} unviable)"
        )
        floor = (recorded.get(crate) or {}).get("caught_pct")
        if floor is None:
            # Deliberately not a note. This crate was mutated, scored, and is
            # compared against nothing; treating that as informational is how a
            # gate reports on thousands of mutants while enforcing none of them.
            print(f"  {crate}: {measured}% {counted} — NO FLOOR RECORDED")
            unfloored.append((crate, measured))
            continue
        verdict = "ok" if measured >= floor - args.tolerance else "REGRESSED"
        print(f"  {crate}: {measured}% vs floor {floor}% {counted} — {verdict}")
        if verdict == "REGRESSED":
            failures.append((crate, measured, floor))

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

    if unfloored:
        print("\nno floor recorded for:", file=sys.stderr)
        for crate, measured in unfloored:
            print(f"  {crate}: measured {measured}%", file=sys.stderr)
        print(
            "\nEvery crate in scope carries a floor or it is not enforced. "
            "Record one from this run:\n"
            "\n"
            "  locally, from out/:\n"
            "    mise run mutants-baseline\n"
            "\n"
            "  from a nightly run's artifacts:\n"
            "    gh run download <run-id> --pattern 'mutants-*' --dir outcomes\n"
            "    .github/scripts/mutants-gate.py outcomes/*/outcomes.json --update\n"
            "\n"
            "and commit mutants/baseline.json saying what the number is.",
            file=sys.stderr,
        )

    if failures or unfloored:
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
