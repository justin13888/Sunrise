#!/usr/bin/env python3
"""Fail when a Rust file grows past its package's threshold.

Why this gate exists
--------------------

`crates/sunrise-core/src/engine.rs` reached 18,568 lines — six `impl` blocks,
ninety free functions and a ten-thousand-line test module in one file, 87% of
its crate. Nothing stopped it, because nothing was counting. It got there one
reasonable commit at a time, and every one of those commits was fine.

This gate is the thing that was missing. It does not judge the files that are
already large; the baseline below carries those. It stops the next one.

Where the numbers come from
---------------------------

Each threshold is that package's own **p90** at the time the gate landed,
measured over its Rust files by `tokei` and rounded to nothing. Not a number
from a style guide: four hundred lines is generous in a codebase whose median
file is sixty and punitive in one whose median is four hundred, and a threshold
imported from elsewhere fails on day one, earns an exemption, and then means
nothing.

Re-derive them if the tree's shape changes again — and note that it did once
already. These packages were measured before a large reorganisation and again
after, and the earlier numbers would have marked eight freshly-created modules
as debt on the day they were written. A threshold describes a distribution, so
when the distribution moves deliberately, the threshold moves with it.

What is counted
---------------

A file's **total length**. That is what a person means by "this file is six
hundred lines", and what their editor's gutter shows them. A file that is mostly
tests still costs a reader the scroll, and `engine/tests.rs` at 10,539 lines is
worth seeing in a report even though every line of it is a test.

The baseline is a ratchet
-------------------------

`BASELINE` lists every file over its threshold today, by path, with what it is.
Three rules make it debt rather than an amnesty:

* **It may only shrink.** A file that grows past its threshold and is not listed
  fails the build. Adding a path here is not a fix.
* **It is paths, not a count.** A count would let one file be fixed while
  another regressed, and the total would look unchanged.
* **A stale entry fails too.** If a listed file drops below its threshold, the
  gate says so and asks for the entry to be removed. An allowance nobody
  revisits is how a baseline becomes permanent.
"""

from __future__ import annotations

import sys
from pathlib import Path

# package root -> p90 of that package's Rust files when this gate landed.
THRESHOLDS: dict[str, int] = {
    "crates/sunrise-core": 1782,
    "crates/sunrise-domain": 763,
    "crates/sunrise-server": 857,
}

# Every file over its threshold today. This list may shrink; it may not grow.
BASELINE: dict[str, str] = {
    # --- sunrise-core -------------------------------------------------------
    "crates/sunrise-core/src/engine/tests.rs": (
        "The engine's whole test module, moved intact when engine.rs was split "
        "so the split's diff carried no test changes. 198 tests and 95 shared "
        "fixtures. Redistributing them across the sixteen engine modules is a "
        "change of its own."
    ),
    "crates/sunrise-core/src/sync_driver.rs": (
        "One async state machine plus its transport harness. Deliberately not "
        "split: `session` is the sole constructor of five of the types it "
        "uses, and separating a state machine from types only it builds "
        "produces two files that must be read together. See issue #207 for the "
        "testability work this file actually needs."
    ),
    "crates/sunrise-core/src/keychain/mod.rs": (
        "What remains of keychain.rs after its free-function tail was "
        "extracted. The four `impl` groups were left deliberately: moving any "
        "of them would require making Keychain's seven private fields "
        "`pub(super)`, widening the visibility of key material to buy "
        "navigation."
    ),
    # --- sunrise-domain -----------------------------------------------------
    "crates/sunrise-domain/src/stats.rs": (
        "504 implementation lines; the rest is tests. A cohesive fold, with one "
        "worthwhile extraction identified but not urgent: `routine_drift` is "
        "the only reason this file imports `routine`, `routine_gen` and `rrule`."
    ),
    "crates/sunrise-domain/src/notify.rs": (
        "481 implementation lines. Cohesive, with a clean three-way line "
        "available (device policy / reminder pipeline / the two digest views) "
        "if it is ever wanted."
    ),
    "crates/sunrise-domain/src/review.rs": (
        "537 implementation lines. One fold over one input struct; the module "
        "doc argues for keeping it in one place, since it is the module that "
        "must not re-derive numbers other folds own."
    ),
    "crates/sunrise-domain/src/export.rs": (
        "485 implementation lines. A renderer for the three folds it imports; "
        "cohesive."
    ),
    "crates/sunrise-domain/src/constraint.rs": (
        "399 implementation lines, and the only spec in the repository with a "
        "test that fails when the Rust and the CDDL disagree."
    ),
    # --- sunrise-server -----------------------------------------------------
    "crates/sunrise-server/src/api/sync/suite.rs": (
        "8 implementation lines. This is the sync test suite, moved whole when "
        "api/sync.rs was split so the split's diff carried no test changes."
    ),
    "crates/sunrise-server/src/api/devices.rs": (
        "399 implementation lines — below this package's threshold on its own. "
        "Deliberately not split: every symbol serves the five `/devices*` "
        "routes mounted together, so splitting it to satisfy a line count "
        "would be churn."
    ),
    "crates/sunrise-server/src/api/blobs.rs": (
        "498 implementation lines. Not audited for a split; the chunked upload, "
        "finalize and streaming fetch paths share the pending store."
    ),
    "crates/sunrise-server/src/relay_log.rs": (
        "570 implementation lines, and it grew when the relay DDL moved here to "
        "sit beside the only code that touches it — a net improvement that "
        "shows up here as a larger file."
    ),
}

SKIP_DIRS = {"target", "node_modules"}


def rust_files(root: Path):
    for p in sorted(root.rglob("*.rs")):
        if not any(part in SKIP_DIRS for part in p.parts):
            yield p


def length(path: Path) -> int:
    return len(path.read_text(encoding="utf-8").splitlines())


def main() -> int:
    if not Path("crates").is_dir():
        print("::error::file-size: run me from the repository root.")
        return 1

    for pkg in THRESHOLDS:
        if not Path(pkg).is_dir():
            print(f"::error::file-size: {pkg} does not exist; the gate cannot run.")
            return 1

    new_violations: list[tuple[str, int, int]] = []
    measured: dict[str, int] = {}

    for pkg, threshold in THRESHOLDS.items():
        for path in rust_files(Path(pkg)):
            rel = path.as_posix()
            n = length(path)
            measured[rel] = n
            if n > threshold and rel not in BASELINE:
                new_violations.append((rel, n, threshold))

    # A baseline entry that no longer earns its place.
    stale = [
        rel
        for rel in BASELINE
        if rel in measured
        and measured[rel] <= THRESHOLDS[next(p for p in THRESHOLDS if rel.startswith(p))]
    ]
    missing = [rel for rel in BASELINE if rel not in measured]

    failed = False

    if new_violations:
        failed = True
        print("::error::file-size: a file grew past its package's threshold.")
        for rel, n, threshold in sorted(new_violations, key=lambda x: -x[1]):
            print(f"  {rel}: {n} lines, over {threshold}")
        print()
        print(
            "Split it along a cohesion boundary — a group of symbols that share "
            "a table, a dependency, or a reason to change — rather than at a "
            "line number, which relocates the problem and adds an import."
        )
        print(
            "Adding it to BASELINE in this script is not a fix. That list is "
            "the debt this gate already tolerates, and it may only shrink."
        )

    if stale:
        failed = True
        print(
            "::error::file-size: a baseline entry is now under its threshold. "
            "Remove it — an allowance nobody revisits becomes permanent."
        )
        for rel in sorted(stale):
            print(f"  {rel}: now {measured[rel]} lines")

    if missing:
        failed = True
        print(
            "::error::file-size: a baseline entry names a file that no longer "
            "exists. Remove it — a stale entry silently widens the gate."
        )
        for rel in sorted(missing):
            print(f"  {rel}")

    if failed:
        return 1

    counts = ", ".join(
        f"{pkg.split('/')[-1]} ≤{t}" for pkg, t in sorted(THRESHOLDS.items())
    )
    print(
        f"OK: file-size clean — {counts}; "
        f"{len(BASELINE)} file(s) on the shrinking baseline."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
