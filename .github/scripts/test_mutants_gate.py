#!/usr/bin/env python3
"""The exit-code contract of `mutants-gate.py`, as assertions.

Why this file exists
--------------------

The gate's exit codes are read by two callers that cannot ask it what it
meant: `.github/workflows/ci.yml`, which turns a code into a red or green
check, and a person following the remedy the failure printed. The
distinction those callers depend on is 1 ("the run was scored and the
answer is no") versus 2 ("nothing was scored, do not read a verdict into
this"), and it is spread across a dozen early returns in one function.

It drifted twice in successive reviews — a route added to the code and
not to the docstring, then a docstring paragraph that argued for one code
while the code returned another — and both times the only check was a
person re-running the script by hand against whatever outcomes happened
to be on disk. That is not a check; the tree it reads is gitignored and
differs per machine.

So: every documented route, its own fixture, its own asserted code. The
docstring of `mutants-gate.py` names this file, and the `mutants-gate`
job in ci.yml runs it on every push and pull request rather than in the
nightly mutation run, because a contract test that only runs at 04:00 on
a schedule would not have caught either drift before it merged.

Fixtures are synthesised in a temp directory. Nothing here reads or
writes `mutants/baseline.json`, and every subprocess runs with its cwd
inside the temp directory, so the gate's default relative `--baseline`
cannot resolve to the repository's own floors even if a case forgets to
pass one. `test_default_baseline_is_relative_to_cwd` pins that.

Run it with `mise run mutants-gate-test`, or directly.
"""

from __future__ import annotations

import json
import pathlib
import subprocess
import sys
import tempfile
import unittest

GATE = pathlib.Path(__file__).resolve().parent / "mutants-gate.py"

CAUGHT = "CaughtMutant"
MISSED = "MissedMutant"
TIMEOUT = "Timeout"
UNVIABLE = "Unviable"


def mutant(crate: str, summary: str, package=None, file=None) -> dict:
    """One outcome in the shape cargo-mutants writes.

    Only the fields the gate reads are populated, and both of them are:
    `package` is what it consults first, `file` the fallback for when the
    package is absent. A fixture carrying one of them would leave whichever
    it dropped unexercised, and the two are pinned apart in
    `CrateAttribution` below.

    `package` and `file` override the defaults, which is how a mutant that
    disagrees with itself gets built.
    """
    return {
        "scenario": {
            "Mutant": {
                "package": crate if package is None else package,
                "file": f"crates/{crate}/src/lib.rs" if file is None else file,
                "name": f"replace * with + in {crate}::f",
            }
        },
        "summary": summary,
    }


def document_file(path: pathlib.Path, outcomes: list) -> pathlib.Path:
    """Write an outcomes.json holding exactly these outcomes."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"outcomes": outcomes}))
    return path


def outcomes_file(
    path: pathlib.Path,
    crate: str,
    caught: int = 0,
    missed: int = 0,
    timeout: int = 0,
    unviable: int = 0,
) -> pathlib.Path:
    """Write an outcomes.json holding exactly the tally asked for."""
    document = {
        "outcomes": [
            # The unmutated baseline scenario, which the gate must skip:
            # counted as a mutant it would shift every rate in the suite.
            {"scenario": "Baseline", "summary": "Success"},
            *(mutant(crate, CAUGHT) for _ in range(caught)),
            *(mutant(crate, MISSED) for _ in range(missed)),
            *(mutant(crate, TIMEOUT) for _ in range(timeout)),
            *(mutant(crate, UNVIABLE) for _ in range(unviable)),
        ]
    }
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(document))
    return path


def baseline_file(path: pathlib.Path, crates: dict, target=None) -> pathlib.Path:
    document: dict = {
        "crates": {c: {"caught_pct": pct} for c, pct in crates.items()}
    }
    if target is not None:
        document["target_caught_pct"] = target
    path.write_text(json.dumps(document))
    return path


class CrateAttribution(unittest.TestCase):
    """Which field of a mutant decides the crate it is counted against.

    Everything else in this file assumes a mutant lands in the right
    bucket, and nothing asserted which field puts it there. Both operands
    of `package or file` are pinned, in both directions: swapping the
    precedence, or dropping either side, has to break something here.
    """

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = pathlib.Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)

    def score(self, *outcomes) -> subprocess.CompletedProcess:
        run = document_file(self.tmp / "a.json", list(outcomes))
        base = baseline_file(self.tmp / "base.json", {})
        return subprocess.run(
            [sys.executable, str(GATE), str(run), "--baseline", str(base)],
            cwd=self.tmp, capture_output=True, text=True,
        )

    def test_package_wins_over_the_file_path(self):
        # A mutant that disagrees with itself. cargo-mutants does not emit
        # this, which is the point: it separates two fields that otherwise
        # always agree, so the precedence is observable at all.
        result = self.score(
            mutant("sunrise-sync", CAUGHT,
                   package="sunrise-sync",
                   file="crates/sunrise-domain/src/lib.rs"),
        )
        self.assertIn("sunrise-sync: measured", result.stderr)
        self.assertNotIn("sunrise-domain", result.stderr)

    def test_the_file_path_is_the_fallback(self):
        result = self.score(
            mutant("sunrise-domain", CAUGHT,
                   package="",
                   file="crates/sunrise-domain/src/lib.rs"),
        )
        self.assertIn("sunrise-domain: measured", result.stderr)

    def test_a_package_given_as_a_path_is_reduced_to_the_crate(self):
        # crates/<name>/... is the shape the reducer exists for; a bare
        # name falls through it unchanged, which the first test covers.
        result = self.score(
            mutant("sunrise-core", CAUGHT,
                   package="crates/sunrise-core/src/lib.rs"),
        )
        self.assertIn("sunrise-core: measured", result.stderr)
        self.assertNotIn("crates/sunrise-core", result.stderr)

    def test_a_mutant_naming_no_crate_is_skipped(self):
        # Not attributable to anything, so it cannot be counted anywhere —
        # and a run of nothing but these is a run with no mutants in it.
        result = self.score(mutant("sunrise-sync", CAUGHT, package="", file=""))
        self.assertEqual(result.returncode, 2)
        self.assertIn("no mutants found", result.stderr)


class GateContract(unittest.TestCase):
    """One temp directory per test; the gate always runs inside it."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = pathlib.Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)

    def run_gate(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [sys.executable, str(GATE), *args],
            cwd=self.tmp,
            capture_output=True,
            text=True,
        )

    def assert_code(self, result, expected: int, *fragments: str) -> None:
        output = result.stdout + result.stderr
        self.assertEqual(
            result.returncode,
            expected,
            f"expected exit {expected}, got {result.returncode}\n{output}",
        )
        for fragment in fragments:
            self.assertIn(fragment, output)

    # --- 2: the gate could not run, and nothing was scored ----------------

    def test_missing_file_is_2(self):
        self.assert_code(
            self.run_gate("nope.json"),
            2, "not a readable file", "a missing file or an empty argument",
        )

    def test_unexpanded_glob_is_2(self):
        # A quoted glob that matched nothing reaches argv verbatim. It is
        # distinguished from a plain missing file because the remedy is
        # different, and the hint is the only thing that says which.
        self.assert_code(
            self.run_gate("outcomes/*/outcomes.json"),
            2, "not a readable file", "an unexpanded glob",
        )

    def test_empty_argument_is_2(self):
        self.assert_code(
            self.run_gate(""),
            2, "not a readable file", "a missing file or an empty argument",
        )

    def test_no_mutants_in_outcomes_is_2(self):
        empty = self.tmp / "outcomes.json"
        empty.write_text(json.dumps({"outcomes": []}))
        self.assert_code(
            self.run_gate(str(empty)),
            2, "no mutants found in the supplied outcomes",
        )

    def test_unparseable_outcomes_is_2(self):
        broken = self.tmp / "outcomes.json"
        broken.write_text("{not json")
        self.assert_code(self.run_gate(str(broken)), 2, "cannot read")

    def test_undeclared_crate_is_2(self):
        run = outcomes_file(self.tmp / "a.json", "sunrise-core", caught=1)
        base = baseline_file(self.tmp / "base.json", {"sunrise-core": 50.0})
        result = self.run_gate(
            str(run), "--baseline", str(base),
            "--expect-shards", "sunrise-sync=1",
        )
        self.assert_code(
            result, 2,
            "undeclared crate: sunrise-core",
            # Both readings of the failure, and the file that caused it.
            ".github/workflows/ci.yml",
            "out/mutants/",
            "a.json",
        )

    def test_unreadable_baseline_is_2(self):
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        self.assert_code(
            self.run_gate(str(run), "--baseline", str(self.tmp / "gone.json")),
            2, "cannot read",
        )

    def test_bad_expect_shards_spec_is_2(self):
        # argparse's own exit for a type error, which is 2 and happens to
        # agree with the gate's "could not run" — asserted so a future
        # remap of the gate's codes has to confront the collision.
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        self.assert_code(
            self.run_gate(str(run), "--expect-shards", "sunrise-sync"),
            2, "cannot parse",
        )

    def test_no_arguments_is_2(self):
        self.assert_code(self.run_gate(), 2, "usage")

    def test_default_baseline_is_relative_to_cwd(self):
        # Pins the property this suite relies on to stay clear of the real
        # floors: with no --baseline the gate reads mutants/baseline.json
        # relative to the working directory, which here is a temp dir.
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        self.assert_code(
            self.run_gate(str(run)), 2, "mutants/baseline.json",
        )

    # --- 1: the run was scored, or the invocation was wrong ---------------

    def test_duplicate_artifacts_is_1(self):
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = baseline_file(self.tmp / "base.json", {"sunrise-sync": 50.0})
        # Two spellings of one path: the case that used to be counted as
        # two shards, doubling every mutant in it.
        result = self.run_gate(
            str(run), f"./{run.name}", "--baseline", str(base),
        )
        self.assert_code(result, 1, "duplicate artifacts")

    def test_shard_mismatch_with_update_is_1(self):
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = baseline_file(self.tmp / "base.json", {})
        result = self.run_gate(
            str(run), "--update", "--baseline", str(base),
            "--expect-shards", "sunrise-sync=2",
        )
        self.assert_code(
            result, 1, "refusing to record a floor from a mismatched run",
        )
        self.assertEqual(json.loads(base.read_text())["crates"], {})

    def test_shard_mismatch_without_update_is_1(self):
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = baseline_file(self.tmp / "base.json", {"sunrise-sync": 50.0})
        result = self.run_gate(
            str(run), "--baseline", str(base),
            "--expect-shards", "sunrise-sync=2",
        )
        self.assert_code(result, 1, "shards missing", "1/2 shards")

    def test_unscorable_with_update_is_1(self):
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", unviable=3)
        base = baseline_file(self.tmp / "base.json", {})
        result = self.run_gate(
            str(run), "--update", "--baseline", str(base),
            "--expect-shards", "sunrise-sync=1",
        )
        self.assert_code(
            result, 1, "refusing to record a floor with no scorable mutants",
        )

    def test_unscorable_without_update_is_1(self):
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", unviable=3)
        base = baseline_file(self.tmp / "base.json", {"sunrise-sync": 50.0})
        result = self.run_gate(str(run), "--baseline", str(base))
        self.assert_code(result, 1, "no scorable mutants")

    def test_update_without_shard_flags_is_1(self):
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = baseline_file(self.tmp / "base.json", {})
        result = self.run_gate(str(run), "--update", "--baseline", str(base))
        self.assert_code(
            result, 1, "refusing to record a floor from an unverified set",
        )
        self.assertEqual(json.loads(base.read_text())["crates"], {})

    # The three below pin the *order*, not just the code. The refusal is
    # judged on argv before the outcomes are read, so it outranks every
    # exit 2 that a further-along check would have produced: a caller who
    # cannot record a floor whatever happens is told which flag is missing,
    # rather than being sent to fix a file that would not have helped.

    def test_update_refusal_outranks_an_unreadable_baseline(self):
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        result = self.run_gate(
            str(run), "--update", "--baseline", str(self.tmp / "gone.json"),
        )
        self.assert_code(
            result, 1, "refusing to record a floor from an unverified set",
        )
        self.assertNotIn("cannot read", result.stderr)

    def test_update_refusal_outranks_an_unscorable_run(self):
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", unviable=3)
        base = baseline_file(self.tmp / "base.json", {})
        result = self.run_gate(str(run), "--update", "--baseline", str(base))
        self.assert_code(
            result, 1, "refusing to record a floor from an unverified set",
        )
        self.assertNotIn("no scorable mutants", result.stderr)

    def test_update_refusal_outranks_unreadable_outcomes(self):
        base = baseline_file(self.tmp / "base.json", {})
        result = self.run_gate(
            "nope.json", "--update", "--baseline", str(base),
        )
        self.assert_code(
            result, 1, "refusing to record a floor from an unverified set",
        )
        self.assertNotIn("not a readable file", result.stderr)

    def test_regression_is_1(self):
        run = outcomes_file(
            self.tmp / "a.json", "sunrise-sync", caught=1, missed=1)
        base = baseline_file(self.tmp / "base.json", {"sunrise-sync": 90.0})
        result = self.run_gate(str(run), "--baseline", str(base))
        self.assert_code(result, 1, "mutation coverage regressed", "50.0%")

    def test_no_recorded_floor_is_1(self):
        run = outcomes_file(
            self.tmp / "a.json", "sunrise-sync", caught=1, missed=1)
        base = baseline_file(self.tmp / "base.json", {})
        result = self.run_gate(str(run), "--baseline", str(base))
        self.assert_code(result, 1, "no floor recorded")

    def test_default_tolerance_is_half_a_point(self):
        # The default exists to absorb one thing — a timeout that depends on
        # how loaded the machine was — and it is sized to that and no more.
        # An unpinned default is a silent licence: at 5.0 a real 4.9-point
        # loss of coverage passes this gate without a word. So both sides of
        # the boundary are asserted, which fixes the number at 0.5 from
        # above and below. Measured here is 50.0%.
        run = outcomes_file(
            self.tmp / "a.json", "sunrise-sync", caught=1, missed=1)

        base = baseline_file(self.tmp / "base.json", {"sunrise-sync": 50.5})
        self.assert_code(
            self.run_gate(str(run), "--baseline", str(base)),
            0, "50.0% vs floor 50.5% — ok",
        )

        base = baseline_file(self.tmp / "base.json", {"sunrise-sync": 50.6})
        self.assert_code(
            self.run_gate(str(run), "--baseline", str(base)),
            1, "mutation coverage regressed",
        )

    def test_tolerance_flag_overrides_the_default(self):
        run = outcomes_file(
            self.tmp / "a.json", "sunrise-sync", caught=1, missed=1)
        base = baseline_file(self.tmp / "base.json", {"sunrise-sync": 50.6})
        self.assert_code(
            self.run_gate(
                str(run), "--baseline", str(base), "--tolerance", "1.0"),
            0, "50.0% vs floor 50.6% — ok",
        )

    # --- a lost shard is an infrastructure failure, not a regression ------
    #
    # The reason --expect-shards exists. A crate that arrived short has a
    # numerator that never ran, so every number computed from it is wrong in
    # the direction of "the tests got worse" — and the two exclusions that
    # keep it out of the comparison are one `continue` and one clause, both
    # of which can be deleted without any other test noticing.

    def test_a_broken_crate_is_never_called_a_regression(self):
        # Two of six shards, 50% measured, floor 90. Scored, this is a
        # textbook regression; excluded, it is a dead runner. The gate has
        # to say the second, or --expect-shards is decoration.
        shards = [
            outcomes_file(
                self.tmp / f"shard{n}" / "outcomes.json", "sunrise-domain",
                caught=1, missed=1)
            for n in range(2)
        ]
        base = baseline_file(self.tmp / "base.json", {"sunrise-domain": 90.0})
        result = self.run_gate(
            *(str(shard) for shard in shards), "--baseline", str(base),
            "--expect-shards", "sunrise-domain=6",
        )
        self.assert_code(
            result, 1,
            "2/6 shards",
            "shards missing",
            "these crates were not scored",
        )
        output = result.stdout + result.stderr
        self.assertNotIn("REGRESSED", output)
        self.assertNotIn("mutation coverage regressed", output)
        self.assertNotIn("vs floor", output)

    def test_a_broken_crate_is_never_called_unscorable(self):
        # The same exclusion by the other route: a crate that is both short
        # of shards and entirely unviable is a broken run, and reporting it
        # as a crate with nothing to score would send the reader to
        # exclude_re over a runner that died.
        run = outcomes_file(self.tmp / "a.json", "sunrise-domain", unviable=3)
        base = baseline_file(self.tmp / "base.json", {"sunrise-domain": 90.0})
        result = self.run_gate(
            str(run), "--baseline", str(base),
            "--expect-shards", "sunrise-domain=6",
        )
        self.assert_code(result, 1, "1/6 shards", "shards missing")
        output = result.stdout + result.stderr
        self.assertNotIn("no scorable mutants", output)
        self.assertNotIn("REGRESSED", output)

    def test_a_broken_crate_does_not_stop_the_others_being_scored(self):
        # Not fail-fast: the shards that did arrive for other crates are
        # still judged, which is why the gate job runs on always().
        broken = outcomes_file(
            self.tmp / "one" / "outcomes.json", "sunrise-domain", caught=1)
        fine = outcomes_file(
            self.tmp / "two" / "outcomes.json", "sunrise-sync",
            caught=1, missed=1)
        base = baseline_file(
            self.tmp / "base.json",
            {"sunrise-domain": 90.0, "sunrise-sync": 90.0},
        )
        result = self.run_gate(
            str(broken), str(fine), "--baseline", str(base),
            "--expect-shards", "sunrise-domain=6,sunrise-sync=1",
        )
        self.assert_code(
            result, 1,
            "sunrise-sync: 50.0% vs floor 90.0% — REGRESSED",
            "sunrise-domain: 1/6 shards",
        )
        self.assertNotIn("sunrise-domain: 100.0%", result.stdout)

    # --- timeouts: in the denominator, never in the numerator -------------
    #
    # The gate's headline design claim, and the one a plausible "fix" would
    # undo in either direction. A mutant that hung is a mutant no test
    # refuted, so scoring it as caught would let an infinite loop improve the
    # number; dropping it from the denominator instead would let one hide.
    # Both fixtures below are chosen so that the correct rate, the
    # numerator-mutation rate and the denominator-mutation rate are three
    # different numbers.

    def test_timeouts_are_in_the_denominator_not_the_numerator(self):
        # 3 caught, 1 missed, 2 timeout. Correct: 3 / 6 = 50%. Counting
        # timeouts as caught: 5 / 6 = 83.33%. Dropping them from the
        # denominator: 3 / 4 = 75%.
        run = outcomes_file(
            self.tmp / "a.json", "sunrise-sync",
            caught=3, missed=1, timeout=2)
        base = baseline_file(self.tmp / "base.json", {})
        result = self.run_gate(
            str(run), "--update", "--baseline", str(base),
            "--expect-shards", "sunrise-sync=1",
        )
        self.assert_code(result, 0, "recorded 1 crate(s)")
        recorded = json.loads(base.read_text())["crates"]["sunrise-sync"]
        self.assertEqual(recorded["caught_pct"], 50.0)
        self.assertEqual(recorded["timeout"], 2)

    def test_a_timeout_cannot_hold_a_crate_above_its_floor(self):
        # The same tally judged rather than recorded, against a floor of 60.
        # At the true 50% this is a regression; at either mutation's rate
        # (83.33% or 75%) it passes, and a hung mutant has bought coverage.
        run = outcomes_file(
            self.tmp / "a.json", "sunrise-sync",
            caught=3, missed=1, timeout=2)
        base = baseline_file(self.tmp / "base.json", {"sunrise-sync": 60.0})
        result = self.run_gate(str(run), "--baseline", str(base))
        self.assert_code(
            result, 1,
            "mutation coverage regressed",
            "sunrise-sync: 50.0% < 60.0%",
            "(3 caught, 1 missed, 2 timeout, 0 unviable)",
        )

    def test_a_run_of_only_timeouts_scores_zero(self):
        # Nothing was refuted, so the rate is 0% and the floor bites.
        # Counting timeouts as caught makes this 100%; dropping them from
        # the denominator makes it unscorable, which is a different exit
        # path and a different message.
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", timeout=2)
        base = baseline_file(self.tmp / "base.json", {"sunrise-sync": 50.0})
        result = self.run_gate(str(run), "--baseline", str(base))
        self.assert_code(
            result, 1, "sunrise-sync: 0.0% < 50.0%", "0 caught, 0 missed, "
            "2 timeout",
        )
        self.assertNotIn("no scorable mutants", result.stderr)

    # --- the remedy a failure prints has to be runnable -------------------

    def test_unfloored_remedy_is_a_pasteable_command(self):
        # Six shards, all present, so the crate is scored and reaches the
        # unfloored branch rather than being excluded as a broken run.
        shards = [
            outcomes_file(
                self.tmp / f"shard{n}" / "outcomes.json", "sunrise-domain",
                caught=1, missed=1)
            for n in range(6)
        ]
        base = baseline_file(self.tmp / "base.json", {})
        result = self.run_gate(
            *(str(shard) for shard in shards), "--baseline", str(base),
            "--expect-shards", "sunrise-domain=6",
        )
        self.assert_code(
            result, 1,
            "mise run mutants-baseline --expect-shards sunrise-domain=6",
        )
        # The bug this pins: `<crate>=<N>` is redirection syntax, so a
        # remedy containing it is not a command anyone can run.
        for line in result.stderr.splitlines():
            if "mise run mutants-baseline" in line:
                self.assertNotIn("<", line)

    def test_unfloored_remedy_counts_the_files_when_not_told(self):
        # No --expect-shards to copy, so the count is the number of
        # outcomes files that actually carried the crate.
        first = outcomes_file(
            self.tmp / "one" / "outcomes.json", "sunrise-domain",
            caught=1, missed=1)
        second = outcomes_file(
            self.tmp / "two" / "outcomes.json", "sunrise-domain", caught=1)
        base = baseline_file(self.tmp / "base.json", {})
        result = self.run_gate(
            str(first), str(second), "--baseline", str(base))
        self.assert_code(
            result, 1,
            "mise run mutants-baseline --expect-shards sunrise-domain=2",
        )

    def test_unfloored_remedy_names_every_crate_in_the_run(self):
        # The local task scores every directory under out/mutants/, so a
        # remedy naming only the crate that failed comes straight back as
        # an undeclared crate for the one that passed.
        floored = outcomes_file(
            self.tmp / "one" / "outcomes.json", "sunrise-sync", caught=1)
        unfloored = outcomes_file(
            self.tmp / "two" / "outcomes.json", "sunrise-domain", caught=1)
        base = baseline_file(self.tmp / "base.json", {"sunrise-sync": 50.0})
        result = self.run_gate(
            str(floored), str(unfloored), "--baseline", str(base))
        self.assert_code(
            result, 1,
            "--expect-shards sunrise-domain=1,sunrise-sync=1",
        )

    def test_mismatched_run_is_not_offered_a_floor_command(self):
        # One crate short of its shards, another with no floor. `--update`
        # refuses a mismatched run, so printing a mutants-baseline command
        # here would be handing the reader something that cannot work.
        broken = outcomes_file(
            self.tmp / "one" / "outcomes.json", "sunrise-domain", caught=1)
        unfloored = outcomes_file(
            self.tmp / "two" / "outcomes.json", "sunrise-sync",
            caught=1, missed=1)
        base = baseline_file(self.tmp / "base.json", {})
        result = self.run_gate(
            str(broken), str(unfloored), "--baseline", str(base),
            "--expect-shards", "sunrise-domain=6,sunrise-sync=1",
        )
        self.assert_code(
            result, 1,
            "no floor recorded for",
            "No floor can be recorded from this run",
            "sunrise-domain did not arrive",
            "Repeat the run",
        )
        self.assertNotIn("mise run mutants-baseline", result.stderr)

    # --- 0: scored and accepted -------------------------------------------

    def test_clean_run_is_0(self):
        run = outcomes_file(
            self.tmp / "a.json", "sunrise-sync", caught=1, missed=1)
        base = baseline_file(self.tmp / "base.json", {"sunrise-sync": 50.0})
        result = self.run_gate(
            str(run), "--baseline", str(base),
            "--expect-shards", "sunrise-sync=1",
        )
        self.assert_code(result, 0, "50.0% vs floor 50.0% — ok")

    def test_below_release_target_is_still_0(self):
        # The target is the destination, not the floor. A run under it is
        # reported and passes; a gate that failed from day one would have
        # been switched off long before the number arrived.
        run = outcomes_file(
            self.tmp / "a.json", "sunrise-sync", caught=1, missed=1)
        base = baseline_file(
            self.tmp / "base.json", {"sunrise-sync": 50.0}, target=90.0)
        result = self.run_gate(str(run), "--baseline", str(base))
        self.assert_code(result, 0, "Below the 90.0% release target")

    def test_update_with_expect_shards_is_0_and_records(self):
        run = outcomes_file(
            self.tmp / "a.json", "sunrise-sync",
            caught=3, missed=1, unviable=2)
        base = baseline_file(self.tmp / "base.json", {})
        result = self.run_gate(
            str(run), "--update", "--baseline", str(base),
            "--expect-shards", "sunrise-sync=1",
        )
        self.assert_code(result, 0, "recorded 1 crate(s)")
        recorded = json.loads(base.read_text())["crates"]["sunrise-sync"]
        # Unviable mutants are excluded from both sides: the 2 here are in
        # neither half of 3 / (3 + 1) = 75%, which is caught over caught plus
        # missed. What timeouts do to that fraction is a separate claim, and
        # this fixture has none — see the timeout tests below.
        self.assertEqual(recorded["caught_pct"], 75.0)
        self.assertEqual(recorded["unviable"], 2)

    def test_update_with_allow_partial_is_0(self):
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = baseline_file(self.tmp / "base.json", {})
        result = self.run_gate(
            str(run), "--update", "--allow-partial", "--baseline", str(base),
        )
        self.assert_code(result, 0, "no completeness check")

    def test_shards_of_one_crate_aggregate_before_scoring(self):
        # The property the whole sharding design rests on: six shards and
        # one process must produce the same verdict. 1+1 caught over
        # 1+1 caught and 1 missed is 66.67%, not two separate rates.
        first = outcomes_file(
            self.tmp / "one" / "outcomes.json", "sunrise-sync", caught=1)
        second = outcomes_file(
            self.tmp / "two" / "outcomes.json", "sunrise-sync",
            caught=1, missed=1)
        base = baseline_file(self.tmp / "base.json", {"sunrise-sync": 66.67})
        result = self.run_gate(
            str(first), str(second), "--baseline", str(base),
            "--expect-shards", "sunrise-sync=2",
        )
        self.assert_code(result, 0, "66.67% vs floor 66.67% — ok", "2/2 shards")


if __name__ == "__main__":
    unittest.main(verbosity=2)
