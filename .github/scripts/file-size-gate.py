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

A package with fewer than ten Rust files has no p90 of its own: the
nearest-rank p90 of eight files is the largest of them, so a threshold derived
that way is whatever its biggest file happens to be and bounds nothing.
`sunrise-sync` was such a package when it entered -- eight files, one of them
`sse.rs` -- so its threshold is instead the pooled p90 of every Rust file in the packages still in
`UNGATED` when it entered the gate: 708, nearest-rank over their 169 files. That is still this
codebase's own distribution, drawn from the packages most like it, rather than a
number from a style guide.

The twenty-one packages still in `UNGATED` then entered together, emptying it,
so "the packages still in `UNGATED`" was all of them: their pooled p90 is 802,
nearest-rank over their 189 files, and that is the threshold of each of them
with fewer than ten files. `sunrise-sync` and `sunrise-telemetry` keep 708,
the pool they were measured against when they entered. The six of the
twenty-one with ten or more files carry their own p90 as everyone else does;
for `sunrise-cli`, `sunrise-core-bindings` and `sunrise-pairing`, with ten to
thirteen files, that is their second-largest file, which is what a nearest-rank
p90 of so few files is.

Which packages are measured
---------------------------

Every directory under `crates/` is in exactly one of two tables. `THRESHOLDS`
holds the packages the gate measures. `UNGATED` holds the rest, each with the
reason nothing measures it. A crate in neither, or in both, fails the gate with
exit 1 before anything is counted.

That partition is the point. The gate first shipped with three packages and no
record that the other twenty-two were outside it, so `sunrise-sync/src/sse.rs`
grew past five thousand lines while the gate printed "file-size clean" -- not
because the file was within a bound but because it had none, and nobody had
decided that it should not. A new crate now has to be classified on the day it
is added, and an ungated one is ungated on the record.

`UNGATED` is debt in the same sense `BASELINE` is, and it is empty: every
crate in the tree has a threshold and a `BASELINE` entry for each of its files
over it. A crate added later goes into `THRESHOLDS`, at its own nearest-rank
p90 if it has ten or more Rust files and at 802 if it has fewer: with
`UNGATED` empty there is no pool of unmeasured packages left to draw from, so
a small crate takes the last pool's number rather than one re-derived from
packages already bounded by thresholds of their own. One listed in `UNGATED`
instead needs a reason of its own for staying unbounded, not "not measured
yet". An entry whose crate is gone is reported as bookkeeping, exit 2, like a
stale baseline entry.

What is counted
---------------

A file's **total length**. That is what a person means by "this file is six
hundred lines", and what their editor's gutter shows them. A file that is mostly
tests still costs a reader the scroll, and a ten-thousand-line test module is
worth seeing in a report even though every line of it is a test.

The baseline is a ratchet
-------------------------

`BASELINE` lists every file over its threshold today, by path, with what it is.
Three rules make it debt rather than an amnesty:

* **It may only shrink.** A file that grows past its threshold and is not listed
  fails the build. Adding a path here is not a fix.
* **It is paths, not a count.** A count would let one file be fixed while
  another regressed, and the total would look unchanged.
* **A stale entry is reported too.** If a listed file drops below its threshold,
  the gate says so and asks for the entry to be removed. An allowance nobody
  revisits is how a baseline becomes permanent.

Two exit codes, because they are two different pieces of news
-------------------------------------------------------------

* **1 — a file grew past its threshold.** Something in the tree got worse. The
  remedy is a change to the code.
* **2 — the baseline no longer matches the tree.** Nothing got worse; a listed
  file shrank below its threshold or stopped existing, which means somebody did
  the work this list was tracking. The remedy is a one-line deletion from
  `BASELINE` (or from `UNGATED`, for a crate that is gone), and it is spelled
  out in the output.

Exit 1 is also what the gate returns when it refuses to measure at all -- run
outside the repository root, a `THRESHOLDS` package that does not exist, or a
crate its two tables do not classify -- because each of those would otherwise
report a pass over a scope nobody chose.

Both are red in CI, which is the point -- an unrevisited allowance is how the
list rots. But they are not the same event, and a gate that reports them in one
voice teaches people to read "file-size failed" as "a file is too long" and act
on the wrong thing. This mirrors `mutants-gate.py`, which separates "scored, and
the answer is no" from "nothing was scored" for the same reason.

That distinction does real work, because a baseline entry can sit only a few
lines over its threshold, and then deleting those lines turns CI red. The margins
are not padded to hide that -- a threshold of "p90 plus a bit of slack" is exactly
the imported, meaningless number this gate's own thresholds were derived to avoid,
and padding would only move the cliff rather than remove it. What the padding was
tempting for is fixed instead: the report says what happened and what to type.

A file can also sit exactly *at* its package's threshold -- unavoidably on the day
the package enters, since each threshold is its package's p90 and therefore one of
its own files. Adding a line to such a file is a genuine exit 1, which is the gate
working: it is the largest file the distribution calls ordinary, and the next line
really is the one worth arguing about.

What checks this gate
---------------------

`.github/scripts/test_file_size_gate.py`, run by the `file-size-gate-contract`
job ("File size gate contract") on every trigger this workflow has, and by
`mise run file-size-gate-test`. It synthesises packages in a temp directory and
asserts each exit code above: `new_violations`, `stale` and `missing`, which
until that file existed nothing asserted, and the scope partition -- a crate in
neither table, a crate in both, and an `UNGATED` entry whose crate is gone.
"""

from __future__ import annotations

import sys
from pathlib import Path

# package root -> p90 of that package's Rust files when it entered the gate.
# `sunrise-sync` had eight files then, too few to have a p90 of its own, so its
# number is the pooled p90 of the packages in UNGATED: see "Where the numbers
# come from" above.
THRESHOLDS: dict[str, int] = {
    "crates/sunrise-core": 1782,
    "crates/sunrise-domain": 763,
    "crates/sunrise-server": 857,
    "crates/sunrise-sync": 708,
    # Four files on the day it was added, so the pooled number as for
    # `sunrise-sync`.
    "crates/sunrise-telemetry": 708,
    # The twenty-one packages below entered together, when UNGATED was
    # emptied. Those with ten or more Rust files carry their own nearest-rank
    # p90; the rest carry 802, the pooled p90 of all 189 files of the
    # twenty-one: see "Where the numbers come from" above.
    "crates/sunrise-auth": 802,
    "crates/sunrise-bench": 802,
    "crates/sunrise-cbor": 802,
    "crates/sunrise-cli": 1831,  # 13 files
    "crates/sunrise-client-core": 802,
    "crates/sunrise-core-bindings": 2454,  # 13 files
    "crates/sunrise-core-wasm": 802,
    "crates/sunrise-crypto": 895,  # 22 files
    "crates/sunrise-crypto-test-vectors": 802,
    "crates/sunrise-e2e": 517,  # 38 files
    "crates/sunrise-error": 802,
    "crates/sunrise-http-sig": 802,
    "crates/sunrise-id": 802,
    "crates/sunrise-integrations": 802,
    "crates/sunrise-log": 448,  # 16 files
    "crates/sunrise-pairing": 835,  # 10 files
    "crates/sunrise-private-file": 802,
    "crates/sunrise-relay-client": 802,
    "crates/sunrise-storage": 802,
    "crates/sunrise-test-seed": 802,
    "crates/sunrise-wire-protocol": 802,
}

# package root -> why nothing measures it. Every directory under `crates/` is
# in exactly one of THRESHOLDS and this table, and the gate fails on one that
# is in neither: see "Which packages are measured" above. A new crate joins it
# only on purpose, with a reason of its own.
UNGATED: dict[str, str] = {
    # Empty: every crate in the tree is measured.
}

# Every file over its threshold today. This list may shrink; it may not grow.
BASELINE: dict[str, str] = {
    # --- sunrise-domain -----------------------------------------------------
    "crates/sunrise-domain/src/stats.rs": (
        "A cohesive fold above its tests, with one "
        "worthwhile extraction identified but not urgent: `routine_drift` is "
        "the only reason this file imports `routine`, `routine_gen` and `rrule`."
    ),
    "crates/sunrise-domain/src/notify.rs": (
        "Cohesive, with a clean three-way line "
        "available (device policy / reminder pipeline / the two digest views) "
        "if it is ever wanted."
    ),
    "crates/sunrise-domain/src/review.rs": (
        "One fold over one input struct; the module "
        "doc argues for keeping it in one place, since it is the module that "
        "must not re-derive numbers other folds own."
    ),
    "crates/sunrise-domain/src/constraint.rs": (
        "The only spec in the repository with a "
        "test that fails when the Rust and the CDDL disagree."
    ),
    # --- sunrise-server -----------------------------------------------------
    "crates/sunrise-server/src/api/sync/suite.rs": (
        "The sync test suite, moved whole when api/sync.rs was split so the "
        "split's diff carried no test changes. Everything below its module doc "
        "is one `#[cfg(test)] mod tests`: it is the whole suite in one file."
    ),
    "crates/sunrise-server/src/api/devices.rs": (
        "Its implementation alone is below this package's threshold. "
        "Deliberately not split: every symbol serves the five `/devices*` "
        "routes mounted together, so splitting it to satisfy a line count "
        "would be churn."
    ),
    # --- sunrise-sync -------------------------------------------------------
    "crates/sunrise-sync/src/sse/tests.rs": (
        "All test code: the SSE transport's `#[cfg(test)] mod tests`, moved out "
        "of `sse.rs` unchanged, with the loopback relay it runs against. What "
        "those tests pin -- `take_event`, `frame_for`, `binding_bytes`, the "
        "stream buffer -- is private, which rules out `tests/` but not a child "
        "module: splitting them along their own section markers keeps that "
        "access and is a change of its own."
    ),
    # --- sunrise-client-core ------------------------------------------------
    "crates/sunrise-client-core/src/undo.rs": (
        "Its implementation alone is below this package's threshold; the rest "
        "is tests. One concern: the inverse command for each "
        "command a client submits, which the module doc argues is the only "
        "undo a client can build without a protocol change. Not audited for "
        "a split."
    ),
    # --- sunrise-core-bindings ----------------------------------------------
    "crates/sunrise-core-bindings/src/dto.rs": (
        "The UniFFI mirror of the domain vocabulary: the records and enums and "
        "the exhaustively destructuring `From` impls that make a new domain "
        "field fail this crate's build. No test module. Its length tracks the "
        "domain's; splitting it by the domain module each mirror comes from "
        "is a change of its own."
    ),
    # --- sunrise-crypto -----------------------------------------------------
    "crates/sunrise-crypto/src/op_envelope.rs": (
        "Its implementation alone is below this package's threshold; the rest "
        "is tests. The byte-exact CBOR "
        "codec, signing, encryption, verification and decryption of one wire "
        "type. Not audited for a split."
    ),
    "crates/sunrise-crypto/src/device_cert.rs": (
        "Its implementation alone is below this package's threshold; the rest "
        "is tests. The codec, signing and verification of one "
        "certificate type."
    ),
    # --- sunrise-e2e --------------------------------------------------------
    "crates/sunrise-e2e/src/lib.rs": (
        "The crate's whole helper surface for booting a relay, opening wired "
        "`Core`s and waiting on convergence. Test-harness code; not audited "
        "for a split."
    ),
    # --- sunrise-integrations -----------------------------------------------
    "crates/sunrise-integrations/src/ical.rs": (
        "Its implementation alone is below this package's threshold; the rest "
        "is tests. The iCalendar syntax layer only; mapping and "
        "the vault driver are already separate modules."
    ),
    "crates/sunrise-integrations/src/gcal.rs": (
        "Its implementation alone is below this package's threshold; the rest "
        "is tests. Google Calendar OAuth and read-only import "
        "request building."
    ),
    # --- sunrise-log --------------------------------------------------------
    "crates/sunrise-log/tests/event_catalog.rs": (
        "An integration test: the event catalogue and log-field vocabulary "
        "enforced against the files rustc compiled, read from dep-info: every "
        "test over one shared reading of the build. Not audited for a split."
    ),
    # --- sunrise-storage ----------------------------------------------------
    "crates/sunrise-storage/src/db.rs": (
        "Its implementation alone is over this package's threshold, so moving "
        "the tests out would not clear it. The SQLCipher wrapper: keying, pragmas, "
        "integrity check, pre-migration backups and the migration runner."
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

    # Scope is a declaration, not an accident of which packages were typed in
    # first: every crate is measured or says why it is not.
    crates = {
        p.as_posix()
        for p in Path("crates").iterdir()
        if p.is_dir() and not p.name.startswith(".")
    }
    in_both = sorted(set(THRESHOLDS) & set(UNGATED))
    if in_both:
        print(
            "::error::file-size: these packages are in both THRESHOLDS and "
            "UNGATED; a package is measured or it is not."
        )
        for pkg in in_both:
            print(f"  {pkg}")
        return 1
    unclassified = sorted(crates - set(THRESHOLDS) - set(UNGATED))
    if unclassified:
        print(
            "::error::file-size: these packages are in neither THRESHOLDS nor "
            "UNGATED, so the gate cannot say whether anything bounds them."
        )
        for pkg in unclassified:
            print(f"  {pkg}")
        print()
        print(
            "Give each one a threshold in THRESHOLDS -- its own nearest-rank "
            "p90, or 802 if it has fewer than ten Rust files (the pooled p90 "
            "of the last packages to leave UNGATED) -- or list it in UNGATED "
            "with the reason nothing measures it, in "
            ".github/scripts/file-size-gate.py."
        )
        return 1

    new_violations: list[tuple[str, int, int]] = []
    measured: dict[str, int] = {}
    # The threshold each file was measured against, recorded by the package
    # walk that found it. Recovering it from the path by prefix would judge
    # `crates/sunrise-core-bindings/...` against `crates/sunrise-core`.
    limit: dict[str, int] = {}

    for pkg, threshold in THRESHOLDS.items():
        for path in rust_files(Path(pkg)):
            rel = path.as_posix()
            n = length(path)
            measured[rel] = n
            limit[rel] = threshold
            if n > threshold and rel not in BASELINE:
                new_violations.append((rel, n, threshold))

    # A baseline entry that no longer earns its place.
    stale = [rel for rel in BASELINE if rel in measured and measured[rel] <= limit[rel]]
    missing = [rel for rel in BASELINE if rel not in measured]
    ungated_gone = sorted(set(UNGATED) - crates)

    regressed = False
    baseline_drifted = False

    if new_violations:
        regressed = True
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
        baseline_drifted = True
        print(
            "::error::file-size: the baseline is out of date. These files are "
            "under their threshold now, so somebody did the work this list was "
            "tracking — bookkeeping, not a regression."
        )
        for rel in sorted(stale):
            print(f"  {rel}: now {measured[rel]} lines, under {limit[rel]}")
        print()
        print(
            "Delete each of those entries from BASELINE in "
            ".github/scripts/file-size-gate.py. That is the whole remedy; no "
            "code change is wanted or implied."
        )

    if missing:
        baseline_drifted = True
        print(
            "::error::file-size: the baseline names files that are not in the "
            "tree — bookkeeping, not a regression. A stale entry silently "
            "widens the gate."
        )
        for rel in sorted(missing):
            print(f"  {rel}")
        print()
        print(
            "Delete each of those entries from BASELINE in "
            ".github/scripts/file-size-gate.py, or correct the path if the "
            "file was moved rather than removed."
        )

    if ungated_gone:
        baseline_drifted = True
        print(
            "::error::file-size: UNGATED names packages that are not in the "
            "tree — bookkeeping, not a regression."
        )
        for pkg in ungated_gone:
            print(f"  {pkg}")
        print()
        print(
            "Delete each of those entries from UNGATED in "
            ".github/scripts/file-size-gate.py, or correct the path if the "
            "crate was renamed rather than removed."
        )

    # 1 and 2 are different news: see the module docstring. A regression wins
    # when both are true, because it is the one that asks for a code change.
    if regressed:
        return 1
    if baseline_drifted:
        return 2

    counts = ", ".join(
        f"{pkg.split('/')[-1]} ≤{t}" for pkg, t in sorted(THRESHOLDS.items())
    )
    print(
        f"OK: file-size clean — {counts}; "
        f"{len(BASELINE)} file(s) on the shrinking baseline; "
        f"{len(UNGATED)} package(s) listed as not measured."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
