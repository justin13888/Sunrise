#!/usr/bin/env python3
"""The exit-code contract of `file-size-gate.py`, as assertions.

Why this file exists
--------------------

The gate has three failure modes — a file grew past its threshold
(`new_violations`), a baseline entry dropped under its threshold (`stale`),
and a baseline entry names a file that is gone (`missing`) — and until this
file existed not one of them was asserted by anything. The gate's own commit
message records them being checked by hand, once, against the tree that
happened to be on disk. That is not a check: the next person to touch the
script has nothing to run, and the tree it measures changes under it every
week.

It is the only gate here whose subject is the repository itself rather than
a string, which is exactly why it needs fixtures. `core-filesystem-gate.py`
can carry its cases as literals; this one cannot, because what it reads is
file lengths under package roots. So every case below builds a package tree
in a temp directory and runs the gate inside it, with `BASELINE` and
`THRESHOLDS` overridden through a generated copy of the script. Nothing here
reads the repository's own crates — the `file-size` job does that, and a
contract test that also did would go red for whatever the tree happens to be
rather than for a change to the contract.

The distinction the exit codes carry
------------------------------------

1 means a file grew and the remedy is a code change. 2 means the tree is
fine and `BASELINE` needs a line deleted. Both are red, and they ask for
opposite things: a reader who sees "file-size failed" and assumes 1 will go
looking for a file to split when somebody has just finished splitting one.
Two baseline entries currently sit three lines above their threshold, so
code 2 is not a theoretical branch — deleting four lines from
`sunrise-domain/src/constraint.rs` reaches it.

Run it with `mise run file-size-gate-test`, or directly.
"""

from __future__ import annotations

import pathlib
import re
import subprocess
import sys
import tempfile
import unittest

GATE = pathlib.Path(__file__).resolve().parent / "file-size-gate.py"

OK = 0
GREW = 1
BASELINE_STALE = 2


def rust_file(path: pathlib.Path, lines: int) -> pathlib.Path:
    """A `.rs` file of exactly `lines` lines. Only the count is ever read."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("\n".join(f"// line {n}" for n in range(lines)) + "\n")
    return path


class GateCase(unittest.TestCase):
    """One synthesised repository per test; the gate always runs inside it."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = pathlib.Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)
        (self.tmp / "crates").mkdir()

    def source(self, thresholds: dict, baseline: dict) -> pathlib.Path:
        """The gate, with its two tables replaced by this case's.

        Rewriting the literals rather than importing and patching keeps the
        subprocess boundary — which is what CI runs, and where the exit code
        lives — while letting a case describe a tree of three files instead
        of the twenty packages the real tables name.
        """
        text = GATE.read_text()
        for name, value in (("THRESHOLDS", thresholds), ("BASELINE", baseline)):
            pattern = re.compile(
                rf"^{name}: dict\[str, \w+\] = \{{.*?^\}}$", re.S | re.M
            )
            self.assertRegex(text, pattern, f"{name} table not found in the gate")
            literal = f"{name}: dict = {value!r}"
            text = pattern.sub(lambda _m, lit=literal: lit, text, count=1)
        copy = self.tmp / "gate.py"
        copy.write_text(text)
        return copy

    def run_gate(self, thresholds: dict, baseline: dict) -> subprocess.CompletedProcess:
        return subprocess.run(
            [sys.executable, str(self.source(thresholds, baseline))],
            cwd=self.tmp,
            capture_output=True,
            text=True,
        )

    def assert_code(self, result, expected: int, *expected_output: str) -> None:
        self.assertEqual(
            result.returncode,
            expected,
            f"expected exit {expected}, got {result.returncode}:\n"
            f"{result.stdout}{result.stderr}",
        )
        for fragment in expected_output:
            self.assertIn(fragment, result.stdout)


class CleanTree(GateCase):
    def test_a_tree_under_its_thresholds_passes(self):
        rust_file(self.tmp / "crates/a/src/lib.rs", 50)
        result = self.run_gate({"crates/a": 100}, {})
        self.assert_code(result, OK, "file-size clean")

    def test_a_file_exactly_at_the_threshold_is_not_over_it(self):
        # `>` and not `>=`: the threshold is a package's own p90, so at least
        # one real file sits exactly on it and must not be a violation.
        rust_file(self.tmp / "crates/a/src/lib.rs", 100)
        self.assert_code(self.run_gate({"crates/a": 100}, {}), OK)

    def test_a_baseline_entry_over_its_threshold_is_tolerated(self):
        rust_file(self.tmp / "crates/a/src/big.rs", 500)
        result = self.run_gate({"crates/a": 100}, {"crates/a/src/big.rs": "debt"})
        self.assert_code(result, OK, "1 file(s) on the shrinking baseline")

    def test_files_outside_a_named_package_are_not_measured(self):
        # The gate is scoped to the packages in THRESHOLDS. A crate with no
        # threshold has no bound, which is a deliberate limit and not an
        # accident -- it is the difference between a ratchet and a style rule.
        rust_file(self.tmp / "crates/unscoped/src/huge.rs", 5000)
        rust_file(self.tmp / "crates/a/src/lib.rs", 10)
        self.assert_code(self.run_gate({"crates/a": 100}, {}), OK)

    def test_target_and_node_modules_are_skipped(self):
        # Build output is not source. Without this the gate would measure
        # whatever a local `cargo build` left behind and fail differently on
        # every machine.
        rust_file(self.tmp / "crates/a/target/debug/build/generated.rs", 9000)
        rust_file(self.tmp / "crates/a/node_modules/x/vendored.rs", 9000)
        rust_file(self.tmp / "crates/a/src/lib.rs", 10)
        self.assert_code(self.run_gate({"crates/a": 100}, {}), OK)


class AFileGrew(GateCase):
    """Exit 1: the tree got worse, and the remedy is a code change."""

    def test_a_new_file_over_its_threshold_fails(self):
        rust_file(self.tmp / "crates/a/src/lib.rs", 10)
        rust_file(self.tmp / "crates/a/src/grown.rs", 101)
        result = self.run_gate({"crates/a": 100}, {})
        self.assert_code(
            result,
            GREW,
            "grew past its package's threshold",
            "crates/a/src/grown.rs: 101 lines, over 100",
            "Split it along a cohesion boundary",
        )

    def test_adding_it_to_the_baseline_is_named_as_not_being_the_fix(self):
        # The one sentence that decides whether this gate is a ratchet or an
        # amnesty form. It has to be in the failure output, not only the
        # docstring nobody opens.
        rust_file(self.tmp / "crates/a/src/grown.rs", 101)
        result = self.run_gate({"crates/a": 100}, {})
        self.assert_code(result, GREW, "Adding it to BASELINE in this script is not a fix")

    def test_each_package_is_judged_against_its_own_threshold(self):
        # The whole argument for per-package p90: 500 lines is debt in one
        # package and unremarkable in another.
        rust_file(self.tmp / "crates/small/src/lib.rs", 500)
        rust_file(self.tmp / "crates/large/src/lib.rs", 500)
        result = self.run_gate({"crates/small": 100, "crates/large": 1000}, {})
        self.assert_code(result, GREW, "crates/small/src/lib.rs: 500 lines, over 100")
        self.assertNotIn("crates/large", result.stdout)

    def test_a_regression_outranks_a_stale_entry(self):
        # Both conditions at once. The code has to be 1: it is the one that
        # asks for a change to the tree, and a reader who acts on 2 first
        # deletes the entry and ships the regression.
        rust_file(self.tmp / "crates/a/src/shrunk.rs", 10)
        rust_file(self.tmp / "crates/a/src/grown.rs", 500)
        result = self.run_gate({"crates/a": 100}, {"crates/a/src/shrunk.rs": "debt"})
        self.assert_code(result, GREW, "grew past its package's threshold")
        self.assertIn("the baseline is out of date", result.stdout)


class TheBaselineDrifted(GateCase):
    """Exit 2: nothing is too long; the list needs a line deleted."""

    def test_an_entry_that_dropped_under_its_threshold(self):
        rust_file(self.tmp / "crates/a/src/shrunk.rs", 90)
        result = self.run_gate({"crates/a": 100}, {"crates/a/src/shrunk.rs": "debt"})
        self.assert_code(
            result,
            BASELINE_STALE,
            "the baseline is out of date",
            "bookkeeping, not a regression",
            "crates/a/src/shrunk.rs: now 90 lines, under 100",
            "Delete each of those entries from BASELINE",
        )

    def test_an_entry_that_landed_exactly_on_its_threshold(self):
        # The boundary the `<=` picks. A file at exactly p90 is not over it,
        # so its entry has stopped earning its place.
        rust_file(self.tmp / "crates/a/src/shrunk.rs", 100)
        result = self.run_gate({"crates/a": 100}, {"crates/a/src/shrunk.rs": "debt"})
        self.assert_code(result, BASELINE_STALE, "now 100 lines, under 100")

    def test_four_lines_deleted_from_a_three_line_margin(self):
        # The case the real baseline is three lines away from, twice over
        # (constraint.rs at 766/763, relay_log.rs at 860/857). Somebody
        # improves a file and CI goes red; the output has to make it obvious
        # that this is bookkeeping and not a regression.
        rust_file(self.tmp / "crates/a/src/narrow.rs", 762)
        result = self.run_gate({"crates/a": 763}, {"crates/a/src/narrow.rs": "debt"})
        self.assert_code(result, BASELINE_STALE, "bookkeeping, not a regression")
        self.assertNotIn("grew past", result.stdout)
        self.assertNotIn("Split it along a cohesion boundary", result.stdout)

    def test_an_entry_whose_file_is_gone(self):
        rust_file(self.tmp / "crates/a/src/lib.rs", 10)
        result = self.run_gate({"crates/a": 100}, {"crates/a/src/deleted.rs": "debt"})
        self.assert_code(
            result,
            BASELINE_STALE,
            "names files that are not in the tree",
            "crates/a/src/deleted.rs",
            "correct the path if the file was moved",
        )

    def test_a_renamed_file_is_reported_as_missing_rather_than_ignored(self):
        # A split renames files, which is the single most common way this
        # list goes stale -- `api/sync.rs` became `api/sync/suite.rs` once
        # already. Silence here would leave the new file unbounded.
        rust_file(self.tmp / "crates/a/src/new/name.rs", 500)
        result = self.run_gate(
            {"crates/a": 100},
            {"crates/a/src/old_name.rs": "debt"},
        )
        # Both halves are reported, and the code is 1: the file under its new
        # path is an unlisted violation, which is the half that asks for a
        # decision about the code.
        self.assert_code(
            result,
            GREW,
            "crates/a/src/old_name.rs",
            "crates/a/src/new/name.rs: 500 lines, over 100",
        )


class RefusesToGuess(GateCase):
    """Neither answer, when the gate cannot measure what it was asked about."""

    def test_running_outside_the_repository_root_is_not_a_pass(self):
        # `Path("crates")` is relative. Run from anywhere else it measures
        # nothing and would otherwise print OK.
        (self.tmp / "crates").rmdir()
        rust_file(self.tmp / "elsewhere/x.rs", 10)
        result = self.run_gate({"crates/a": 100}, {})
        self.assert_code(result, GREW, "run me from the repository root")

    def test_a_package_that_does_not_exist_is_not_a_pass(self):
        # A crate rename that silently drops a package from the gate's scope
        # is the failure this is here for: nothing would be measured and the
        # gate would go green.
        rust_file(self.tmp / "crates/a/src/lib.rs", 10)
        result = self.run_gate({"crates/a": 100, "crates/renamed": 100}, {})
        self.assert_code(result, GREW, "crates/renamed does not exist")


if __name__ == "__main__":
    unittest.main(verbosity=2)
