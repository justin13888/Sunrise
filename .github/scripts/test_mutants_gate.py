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
docstring of `mutants-gate.py` names this file, and the
`mutants-gate-contract` job ("Mutation gate contract") in ci.yml runs it
on every trigger the workflow has — push, pull request, the nightly
schedule and a manual dispatch. It carries no `if:`, which is the point:
the `mutants` and `mutants-gate` jobs are the ones held to the nightly,
and a contract test that only ran at 04:00 would have reported both of
those drifts the morning after they merged.

Fixtures are synthesised in a temp directory. Nothing here reads or
writes `mutants/baseline.json`, and every subprocess runs with its cwd
inside the temp directory, so the gate's default relative `--baseline`
cannot resolve to the repository's own floors even if a case forgets to
pass one. `test_default_baseline_is_relative_to_cwd` pins that.

Run it with `mise run mutants-gate-test`, or directly.
"""

from __future__ import annotations

import importlib.util
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest

GATE = pathlib.Path(__file__).resolve().parent / "mutants-gate.py"
# `.github/scripts/` -> the repository root, for the one test that reads the
# committed baseline rather than a fixture it made up.
REPO = GATE.parent.parent.parent

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


# The stamp `mise run mutants` and ci.yml's matrix write beside every
# outcomes.json while the measurement runs. Every fixture below gets one,
# because an outcomes file without one is a measurement whose revision
# nobody recorded and `--update` refuses it. Tests that are *about* the
# stamp pass `revision=` to drop it, contradict it, or corrupt it.
MEASURED_AT = {
    "sha": "9f8e7d6c5b4a39281706f5e4d3c2b1a098765432",
    "dirty": False,
    "date": "2026-09-15",
}


def stamp_file(directory: pathlib.Path, revision) -> None:
    """Write the measurement stamp beside an outcomes file."""
    if revision is None:
        return
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "revision.json").write_text(json.dumps(revision))


def document_file(path: pathlib.Path, outcomes: list,
                  revision=MEASURED_AT) -> pathlib.Path:
    """Write an outcomes.json holding exactly these outcomes."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"outcomes": outcomes}))
    stamp_file(path.parent, revision)
    return path


def outcomes_file(
    path: pathlib.Path,
    crate: str,
    caught: int = 0,
    missed: int = 0,
    timeout: int = 0,
    unviable: int = 0,
    revision=MEASURED_AT,
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
    stamp_file(path.parent, revision)
    return path


# A floored crate must carry one of these or the gate refuses the file, so
# every fixture below gets one by default. Tests that are *about* the
# requirement pass `provenance=` explicitly to drop or corrupt it.
SOME_PROVENANCE = {
    "sha": "0123456789abcdef0123456789abcdef01234567",
    "date": "2026-09-17",
    "command": "mutants-gate.py out.json --update --expect-shards x=1",
}


def baseline_file(path: pathlib.Path, crates: dict, target=None,
                  provenance=SOME_PROVENANCE) -> pathlib.Path:
    entries = {}
    for crate, pct in crates.items():
        entry: dict = {"caught_pct": pct}
        if provenance is not None:
            entry["provenance"] = dict(provenance) if isinstance(
                provenance, dict) else provenance
        entries[crate] = entry
    document: dict = {"crates": entries}
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

    def test_a_path_outside_crates_names_no_crate(self):
        # The `parts[0] == "crates"` half of the reducer. Without it,
        # `src/main.rs` yields a crate called `main.rs`: a floor keyed on a
        # filename, which no baseline has and every run would then fail
        # for want of. Workspace members live under crates/, and a mutant
        # from anywhere else is not attributable to one.
        result = self.score(
            mutant("sunrise-sync", CAUGHT, package="", file="src/main.rs"),
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("no mutants found", result.stderr)
        self.assertNotIn("main.rs", result.stdout)

    def test_a_mutant_naming_no_crate_is_skipped(self):
        # Not attributable to anything, so it cannot be counted anywhere —
        # and a run of nothing but these is a run with no mutants in it.
        result = self.score(mutant("sunrise-sync", CAUGHT, package="", file=""))
        self.assertEqual(result.returncode, 2)
        self.assertIn("no mutants found", result.stderr)


class GateContract(unittest.TestCase):
    """One temp directory per test; the gate always runs inside it.

    No git repository, deliberately. `--update` takes the revision it
    stamps a floor with from the `revision.json` written beside each
    outcomes file while the measurement ran, not from the tree it happens
    to be invoked in — which is the whole point of the mechanism, since
    those are different revisions whenever the measurement was long
    enough to be worth recording. `outcomes_file` writes that stamp, so
    nothing here needs a repository. The cases that *are* about reading a
    revision out of a working tree live in `FloorProvenance`, which
    builds one.
    """

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

    def test_a_baseline_of_the_wrong_shape_is_2(self):
        # Parses as JSON, is not a baseline. This is the file a person
        # hand-edits on every ratchet, and before the shapes were checked
        # each of these died on an AttributeError deep in the comparison —
        # which exits 1, the gate's code for "coverage regressed". The
        # worst available answer: a typo reported as a test failure.
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = self.tmp / "base.json"
        for text, expected in [
            ('{"crates": {"sunrise-sync": 50.0}}', "expected an object"),
            ('{"crates": []}', '"crates" is list'),
            ('[]', "top level is list"),
            ('"nope"', "top level is str"),
            ('{"crates": {"sunrise-sync": {"caught_pct": "50"}}}',
             "caught_pct"),
            # `true` is an int as far as isinstance is concerned, so the
            # bool clause is the one thing standing between this and
            # `1.0% vs floor True% — ok`, exit 0. Sibling of the `N >= 1`
            # and isdigit() clauses in the shard spec: same family, same
            # failure mode, and the one that was missed.
            ('{"crates": {"sunrise-sync": {"caught_pct": true}}}',
             'caught_pct" is bool, expected a number'),
        ]:
            with self.subTest(baseline=text):
                base.write_text(text)
                result = self.run_gate(str(run), "--baseline", str(base))
                self.assert_code(result, 2, "cannot use", expected)
                self.assertNotIn("Traceback", result.stderr)

    def test_a_baseline_with_no_crates_key_is_usable(self):
        # Absent is not malformed: an empty baseline is where every floor
        # starts, and the gate's answer to it is "no floor recorded".
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = self.tmp / "base.json"
        base.write_text('{"target_caught_pct": 90.0}')
        self.assert_code(
            self.run_gate(str(run), "--baseline", str(base)),
            1, "no floor recorded",
        )

    def test_bad_expect_shards_spec_is_2(self):
        # argparse's own exit for a type error, which is 2 and happens to
        # agree with the gate's "could not run" — asserted so a future
        # remap of the gate's codes has to confront the collision.
        #
        # Every clause of the spec parser gets a spelling here. The one
        # that matters most is `=0`: a crate declared as zero shards is
        # complete the moment it produces nothing, so it passes the
        # completeness check, contributes no outcomes, and is scored
        # against nothing — the crate silently dropped from the run, which
        # is the hole --expect-shards exists to close, reached through the
        # flag instead of through a dead runner.
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        for spec, expected in [
            ("sunrise-sync", "cannot parse"),          # no '='
            ("=1", "cannot parse"),                    # no crate
            ("sunrise-sync=", "cannot parse"),         # no count
            ("sunrise-sync=x", "cannot parse"),        # not a number
            ("sunrise-sync=1.5", "cannot parse"),      # not an integer
            ("sunrise-sync=-1", "cannot parse"),       # not a count
            ("sunrise-sync=0", "N >= 1"),              # not a shard
            ("sunrise-domain=0,sunrise-sync=1", "N >= 1"),
            ("", "at least one crate=N"),              # nothing declared
            (",", "at least one crate=N"),
            ("  ", "at least one crate=N"),
        ]:
            with self.subTest(spec=spec):
                self.assert_code(
                    self.run_gate(str(run), "--expect-shards", spec),
                    2, expected,
                )

    def test_a_zero_shard_crate_cannot_be_declared_away(self):
        # The behaviour the guard above is protecting, spelled out: were
        # `=0` accepted, this run would exit 0 with sunrise-domain neither
        # scored nor mentioned.
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = baseline_file(self.tmp / "base.json", {"sunrise-sync": 50.0})
        result = self.run_gate(
            str(run), "--baseline", str(base),
            "--expect-shards", "sunrise-domain=0,sunrise-sync=1",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("N >= 1", result.stderr)

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
        self.assert_code(
            result, 1, "shards missing", "1/2 shards",
            # Its own remedy paragraph, which the duplicate-artifacts cause
            # has asserted and this one did not — the asymmetry was the tell.
            "Re-run the failed shards rather",
        )
        self.assertNotIn("More files than shards", result.stderr)

    def test_more_files_than_shards_is_1(self):
        # The other half of the mismatch check, and the half no test
        # reached: `test_duplicate_artifacts_is_1` passes one file twice,
        # which the realpath dedup catches long before the counting. Two
        # *distinct* files for a crate declared as one shard is the case
        # that gets here — an artifact downloaded into two directories, or
        # a stale run left beside a fresh one — and every mutant in the
        # pair is counted, so the crate's rate is computed over a
        # population that does not exist.
        first = outcomes_file(
            self.tmp / "one" / "outcomes.json", "sunrise-sync", caught=1)
        second = outcomes_file(
            self.tmp / "two" / "outcomes.json", "sunrise-sync", caught=1)
        base = baseline_file(self.tmp / "base.json", {"sunrise-sync": 50.0})
        result = self.run_gate(
            str(first), str(second), "--baseline", str(base),
            "--expect-shards", "sunrise-sync=1",
        )
        self.assert_code(
            result, 1,
            "2/1 shards",
            "duplicate artifacts",
            "More files than shards",
        )
        # Not the other cause, whose remedy is to re-run the dead shards.
        self.assertNotIn("shards missing", result.stderr)

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
        # The per-crate line in the run listing, distinct from the summary
        # on stderr: it is what puts the crate in the report at all, and
        # the count is what says how much was thrown away.
        self.assertIn(
            "  sunrise-sync: no scorable mutants (3 unviable)", result.stdout)

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

    def test_a_crate_that_produced_nothing_is_still_reported(self):
        # Not one short shard — no shards. This is the shape CI actually
        # reaches: `mutants-gate` runs on always(), so if all six domain
        # jobs die there is no domain artifact to download and the crate
        # vanishes from `counts` entirely. Every other broken-crate test
        # supplies at least one file for the short crate, which is how a
        # `if crate not in counts: continue` in the mismatch report stayed
        # green: silence about a crate that produced nothing reads exactly
        # like a crate that was never in scope.
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = baseline_file(self.tmp / "base.json", {"sunrise-sync": 50.0})
        result = self.run_gate(
            str(run), "--baseline", str(base),
            "--expect-shards", "sunrise-domain=6,sunrise-sync=1",
        )
        self.assert_code(
            result, 1,
            "sunrise-domain: 0/6 shards, 0 mutants — shards missing",
            "these crates were not scored",
        )

    def test_a_crate_that_produced_nothing_does_not_pass_the_run(self):
        # The verdict, not just the message. A crate declared and absent
        # must not leave the gate green, whatever the crates that did
        # arrive scored.
        run = outcomes_file(
            self.tmp / "a.json", "sunrise-sync", caught=1, missed=1)
        base = baseline_file(self.tmp / "base.json", {"sunrise-sync": 50.0})
        result = self.run_gate(
            str(run), "--baseline", str(base),
            "--expect-shards", "sunrise-domain=6,sunrise-sync=1",
        )
        # sunrise-sync is at its floor and says so; the run still fails.
        self.assert_code(result, 1, "sunrise-sync: 50.0% vs floor 50.0% — ok")

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

    def test_an_unrecognised_summary_is_counted_nowhere(self):
        # cargo-mutants writes summaries this gate has no bucket for, and a
        # future version will write more. Counting them anyway puts an
        # unknown key in the tally, which `sum(bucket.values())` then
        # reports as mutants — see the report test below. What is asserted
        # here is the shape of the record that reaches
        # mutants/baseline.json, a committed file the next run reads: five
        # keys, chosen deliberately, whatever cargo-mutants invents next.
        run = document_file(self.tmp / "a.json", [
            mutant("sunrise-sync", CAUGHT),
            mutant("sunrise-sync", "SomethingElse"),
        ])
        base = baseline_file(self.tmp / "base.json", {})
        result = self.run_gate(
            str(run), "--update", "--baseline", str(base),
            "--expect-shards", "sunrise-sync=1",
        )
        self.assert_code(result, 0, "recorded 1 crate(s)")
        recorded = json.loads(base.read_text())["crates"]["sunrise-sync"]
        self.assertEqual(
            sorted(recorded),
            ["caught", "caught_pct", "missed", "provenance", "timeout",
             "unviable"],
        )
        self.assertEqual(recorded["caught"], 1)
        self.assertEqual(recorded["caught_pct"], 100.0)

    def test_an_unrecognised_summary_is_not_counted_in_the_report(self):
        run = document_file(self.tmp / "a.json", [
            mutant("sunrise-sync", CAUGHT),
            mutant("sunrise-sync", "SomethingElse"),
        ])
        base = baseline_file(self.tmp / "base.json", {"sunrise-sync": 100.0})
        result = self.run_gate(str(run), "--baseline", str(base))
        self.assert_code(
            result, 0,
            "1 mutants (1 caught, 0 missed, 0 timeout, 0 unviable)",
        )

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

    def test_both_mismatch_causes_in_one_run_stay_separate(self):
        # The composition, which each side being pinned separately does
        # not cover: one crate short, another duplicated, a third complete
        # and unfloored. Every paragraph has to appear exactly once and
        # name only the crates it is about — the failure mode is a report
        # that tells the reader to repeat a run that was not short, or to
        # narrow inputs that were not duplicated.
        short = outcomes_file(
            self.tmp / "d" / "outcomes.json", "sunrise-domain",
            caught=1, missed=1)
        twice = [
            outcomes_file(
                self.tmp / f"c{n}" / "outcomes.json", "sunrise-crypto",
                caught=1, missed=1)
            for n in range(2)
        ]
        unfloored = outcomes_file(
            self.tmp / "s" / "outcomes.json", "sunrise-sync",
            caught=1, missed=1)
        base = baseline_file(self.tmp / "base.json", {})
        result = self.run_gate(
            str(short), *(str(t) for t in twice), str(unfloored),
            "--baseline", str(base),
            "--expect-shards",
            "sunrise-domain=6,sunrise-crypto=1,sunrise-sync=1",
        )
        self.assert_code(
            result, 1,
            "sunrise-domain: 1/6 shards, 2 mutants — shards missing",
            "sunrise-crypto: 2/1 shards, 4 mutants — duplicate artifacts",
            "no floor recorded for",
        )
        stderr = result.stderr
        for once in [
            "A shard whose runner died",          # cause paragraph, missing
            "More files than shards",             # cause paragraph, duplicate
            "sunrise-domain arrived short",       # remedy, missing
            "sunrise-crypto arrived more than once",  # remedy, duplicate
        ]:
            self.assertEqual(stderr.count(once), 1, once)
        # Neither remedy reaches across to the other's crate.
        self.assertNotIn("sunrise-crypto arrived short", stderr)
        self.assertNotIn("sunrise-domain arrived more than once", stderr)
        self.assertNotIn("sunrise-sync arrived", stderr)

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
            "sunrise-domain arrived short",
            "Repeat the run",
        )
        self.assertNotIn("mise run mutants-baseline", result.stderr)
        # The other cause's remedy is the opposite of this one, so it must
        # not appear beside it.
        self.assertNotIn("Narrow the inputs to one file per shard",
                         result.stderr)

    def test_a_duplicated_run_is_told_to_narrow_not_to_repeat(self):
        # Two files for a one-shard crate, beside a crate with no floor.
        # Nothing went missing and nothing is partial here: repeating the
        # run is the one action guaranteed to reproduce it.
        first = outcomes_file(
            self.tmp / "one" / "outcomes.json", "sunrise-domain", caught=1)
        second = outcomes_file(
            self.tmp / "two" / "outcomes.json", "sunrise-domain", caught=1)
        unfloored = outcomes_file(
            self.tmp / "three" / "outcomes.json", "sunrise-sync",
            caught=1, missed=1)
        base = baseline_file(self.tmp / "base.json", {})
        result = self.run_gate(
            str(first), str(second), str(unfloored), "--baseline", str(base),
            "--expect-shards", "sunrise-domain=1,sunrise-sync=1",
        )
        self.assert_code(
            result, 1,
            "2/1 shards",
            "No floor can be recorded from this run",
            "sunrise-domain arrived more than once",
            "Narrow the inputs to one file per shard",
        )
        self.assertNotIn("mise run mutants-baseline", result.stderr)
        self.assertNotIn("arrived short", result.stderr)
        self.assertNotIn("shards that went missing", result.stderr)

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

    def test_allow_partial_says_nothing_when_the_counts_are_given(self):
        # The notice is the whole point of --allow-partial: floors recorded
        # without a completeness check describe what ran and not the crates,
        # and that has to be said out loud. But --expect-shards checks them,
        # so printing it here would be announcing a check that happened.
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = baseline_file(self.tmp / "base.json", {})
        result = self.run_gate(
            str(run), "--update", "--allow-partial", "--baseline", str(base),
            "--expect-shards", "sunrise-sync=1",
        )
        self.assert_code(result, 0, "recorded 1 crate(s)")
        self.assertNotIn("no completeness check", result.stdout)

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


class FloorProvenance(unittest.TestCase):
    """A recorded floor has to say what produced it.

    The exit code is the contract, not the message: `ci.yml` and the
    gate's own printed remedies branch on 2 meaning "nothing was scored"
    rather than "coverage regressed", and a baseline this gate cannot use
    is squarely the former.

    The pairing is the point. `malformed()` requiring `provenance` and
    `--update` writing it are one change: with only the first, the next
    `mise run mutants-baseline` writes a file the gate then refuses,
    because the update path replaces each crate entry wholesale.
    `test_update_writes_provenance_the_validator_accepts` is what holds
    the two halves together, and it would fail on either alone.

    What `sha` and `date` mean is asserted here too, not just their
    shape. Asserting shape alone is what let three committed floors ship
    in a vocabulary the writer could not produce: seven hex digits where
    it writes forty, and a `command` no `shlex.join(sys.argv)` can emit.
    So `test_update_records_the_format_the_committed_floors_are_in`
    reads the file this repository actually ships and holds it to the
    same format as a fresh write, and
    `test_update_records_the_measurement_revision_not_head` pins the
    distinction the field exists for.

    The tests in this class do build a git repository, because the ones
    below `--record-revision` are about reading a revision out of a
    working tree.
    """

    def setUp(self) -> None:
        self._repo = tempfile.TemporaryDirectory()
        self.tmp = pathlib.Path(self._repo.name)
        self.addCleanup(self._repo.cleanup)
        for args in (
            ("git", "init", "--quiet"),
            ("git", "config", "user.email", "gate@example.invalid"),
            ("git", "config", "user.name", "gate"),
            ("git", "commit", "--quiet", "--allow-empty", "-m", "fixture"),
        ):
            subprocess.run(args, cwd=self.tmp, capture_output=True,
                           text=True, check=True)

    def run_gate(self, *args: str, cwd=None) -> subprocess.CompletedProcess:
        return subprocess.run(
            [sys.executable, str(GATE), *args],
            cwd=cwd or self.tmp, capture_output=True, text=True,
        )

    def assert_code(self, result, expected: int, *fragments: str) -> None:
        output = result.stdout + result.stderr
        self.assertEqual(
            result.returncode, expected,
            f"expected exit {expected}, got {result.returncode}\n{output}")
        for fragment in fragments:
            self.assertIn(fragment, output)

    # --- 2: the baseline cannot be used ----------------------------------

    def test_floor_without_provenance_is_2(self):
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = baseline_file(
            self.tmp / "base.json", {"sunrise-sync": 50.0}, provenance=None)
        self.assert_code(
            self.run_gate(str(run), "--baseline", str(base)),
            2, "cannot use", "provenance", "expected an object")

    def test_floor_with_non_object_provenance_is_2(self):
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = baseline_file(
            self.tmp / "base.json", {"sunrise-sync": 50.0},
            provenance="1d4b484 on 2026-09-16")
        self.assert_code(
            self.run_gate(str(run), "--baseline", str(base)),
            2, "cannot use", '"crates.sunrise-sync.provenance" is str')

    def test_floor_with_a_missing_provenance_field_is_2(self):
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = baseline_file(
            self.tmp / "base.json", {"sunrise-sync": 50.0},
            provenance={"sha": "1d4b484", "date": "2026-09-16"})
        self.assert_code(
            self.run_gate(str(run), "--baseline", str(base)),
            2, "cannot use", "provenance.command")

    def test_floor_with_a_blank_provenance_field_is_2(self):
        # The case `git rev-parse` failing would have written if the writer
        # recorded an empty string instead of refusing. A blank sha has the
        # right shape and says nothing, which is the placeholder
        # mutants/baseline.json's own rule rejects.
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = baseline_file(
            self.tmp / "base.json", {"sunrise-sync": 50.0},
            provenance={"sha": "   ", "date": "2026-09-16", "command": "x"})
        self.assert_code(
            self.run_gate(str(run), "--baseline", str(base)),
            2, "cannot use", "provenance.sha")

    def test_crate_with_no_floor_needs_no_provenance(self):
        # An entry carrying no `caught_pct` constrains nothing, so there is
        # nothing to account for. This is the shape `sunrise-core` is in.
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = self.tmp / "base.json"
        base.write_text(json.dumps({"crates": {"sunrise-core": {}}}))
        self.assert_code(
            self.run_gate(str(run), "--baseline", str(base)),
            1, "NO FLOOR RECORDED")

    # --- 0: --update writes what the validator requires -------------------

    def test_update_writes_provenance_the_validator_accepts(self):
        run = outcomes_file(
            self.tmp / "a.json", "sunrise-sync", caught=3, missed=1)
        base = baseline_file(self.tmp / "base.json", {})
        self.assert_code(
            self.run_gate(
                str(run), "--update", "--baseline", str(base),
                "--expect-shards", "sunrise-sync=1"),
            0, "recorded 1 crate(s)")
        recorded = json.loads(base.read_text())["crates"]["sunrise-sync"]
        origin = recorded["provenance"]
        self.assertEqual(
            sorted(origin), ["command", "date", "dirty", "sha"])
        self.assertIn("--expect-shards", origin["command"])

        # The half that makes this one change rather than two: the file the
        # writer just produced is a file the validator accepts. Fails if
        # either the requirement or the write is dropped.
        self.assert_code(
            self.run_gate(str(run), "--baseline", str(base)),
            0, "75.0% vs floor 75.0% — ok")

    def test_update_records_the_measurement_revision_not_head(self):
        # The defect this whole mechanism exists for. The fixture's stamp
        # names a revision that is deliberately not this repository's
        # HEAD, which is what a real run looks like: the measurement took
        # hours, the tests that motivated it were committed while it ran,
        # and `--update` comes later. A gate that asked git would record
        # the wrong tree and nothing would contradict it.
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = baseline_file(self.tmp / "base.json", {})
        self.assert_code(
            self.run_gate(
                str(run), "--update", "--baseline", str(base),
                "--expect-shards", "sunrise-sync=1"),
            0, "recorded 1 crate(s)")
        origin = json.loads(
            base.read_text())["crates"]["sunrise-sync"]["provenance"]
        head = subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=self.tmp,
            capture_output=True, text=True, check=True).stdout.strip()
        self.assertEqual(origin["sha"], MEASURED_AT["sha"])
        self.assertNotEqual(origin["sha"], head)
        self.assertEqual(origin["date"], MEASURED_AT["date"])
        self.assertIs(origin["dirty"], False)

    def test_update_records_the_format_the_committed_floors_are_in(self):
        # The written values and the committed ones have to be one
        # vocabulary or `provenance` is not comparable across floors and
        # its `command` is not a recipe. Pinned against the shipped file
        # rather than against a restatement of it, so a re-record that
        # changed either side's shape is caught here.
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = baseline_file(self.tmp / "base.json", {})
        self.assert_code(
            self.run_gate(
                str(run), "--update", "--baseline", str(base),
                "--expect-shards", "sunrise-sync=1",
                "--command",
                "mise run mutants-baseline --expect-shards sunrise-sync=1"),
            0)
        written = json.loads(
            base.read_text())["crates"]["sunrise-sync"]["provenance"]

        committed = json.loads(
            (REPO / "mutants" / "baseline.json").read_text())["crates"]
        for crate, entry in committed.items():
            origin = entry.get("provenance")
            if origin is None:
                continue
            with self.subTest(crate=crate):
                # A full revision, not an abbreviation: `1d4b484` is
                # ambiguous in principle and not what any writer emits.
                self.assertEqual(len(origin["sha"]), len(written["sha"]))
                self.assertRegex(origin["sha"], r"\A[0-9a-f]{40}\Z")
                self.assertRegex(origin["date"], r"\A\d{4}-\d{2}-\d{2}\Z")
                # The human-facing invocation, not the gate's own argv.
                self.assertTrue(
                    origin["command"].startswith("mise run mutants-baseline"),
                    origin["command"])
        self.assertTrue(
            written["command"].startswith("mise run mutants-baseline"),
            written["command"])

    def test_the_command_defaults_to_this_process_argv(self):
        # The one caller with no friendlier form: a person handing the
        # gate a nightly's artifacts by hand. Their argv *is* the recipe.
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = baseline_file(self.tmp / "base.json", {})
        self.assert_code(
            self.run_gate(
                str(run), "--update", "--baseline", str(base),
                "--expect-shards", "sunrise-sync=1"),
            0)
        origin = json.loads(
            base.read_text())["crates"]["sunrise-sync"]["provenance"]
        self.assertIn("--update", origin["command"])
        self.assertIn("--expect-shards", origin["command"])

    def test_a_dirty_measurement_is_recorded_as_dirty(self):
        run = outcomes_file(
            self.tmp / "a.json", "sunrise-sync", caught=1,
            revision={**MEASURED_AT, "dirty": True})
        base = baseline_file(self.tmp / "base.json", {})
        result = self.run_gate(
            str(run), "--update", "--baseline", str(base),
            "--expect-shards", "sunrise-sync=1")
        self.assert_code(result, 0, "modified working tree")
        origin = json.loads(
            base.read_text())["crates"]["sunrise-sync"]["provenance"]
        self.assertIs(origin["dirty"], True)

    def test_a_non_boolean_dirty_flag_is_2(self):
        # `"false"` is a string and every string is truthy, so it reads
        # as clean to a person and as dirty to the code.
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = baseline_file(
            self.tmp / "base.json", {"sunrise-sync": 50.0},
            provenance={**SOME_PROVENANCE, "dirty": "false"})
        self.assert_code(
            self.run_gate(str(run), "--baseline", str(base)),
            2, "cannot use", "provenance.dirty", "expected a boolean")

    def test_update_with_allow_partial_writes_provenance_too(self):
        # A partial floor is still a standing constraint, so it still has
        # to say what produced it.
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = baseline_file(self.tmp / "base.json", {})
        self.assert_code(
            self.run_gate(
                str(run), "--update", "--allow-partial",
                "--baseline", str(base)),
            0, "no completeness check")
        origin = json.loads(
            base.read_text())["crates"]["sunrise-sync"]["provenance"]
        self.assertEqual(
            sorted(origin), ["command", "date", "dirty", "sha"])
        self.assertIn("--allow-partial", origin["command"])

    # --- 2: the measurement cannot say what revision produced it ---------

    def test_update_refuses_when_the_outcomes_carry_no_revision(self):
        # The recorder's HEAD is not an answer to this question, and
        # guessing it is what put a wrong revision in the file before.
        # Refusing is recoverable — the outcomes are still on disk.
        run = outcomes_file(
            self.tmp / "a.json", "sunrise-sync", caught=1, revision=None)
        base = baseline_file(self.tmp / "base.json", {})
        self.assert_code(
            self.run_gate(
                str(run), "--update", "--baseline", str(base),
                "--expect-shards", "sunrise-sync=1"),
            2, "carry no measurement revision", str(run))
        # Nothing was written: the refusal is before the file is touched.
        self.assertEqual(json.loads(base.read_text())["crates"], {})

    def test_update_refuses_when_two_shards_disagree_on_the_revision(self):
        # A crate scored across two trees is not a measurement of either.
        first = outcomes_file(
            self.tmp / "one" / "outcomes.json", "sunrise-sync", caught=1)
        second = outcomes_file(
            self.tmp / "two" / "outcomes.json", "sunrise-sync", caught=1,
            revision={**MEASURED_AT, "sha": "a" * 40})
        base = baseline_file(self.tmp / "base.json", {})
        self.assert_code(
            self.run_gate(
                str(first), str(second), "--update", "--baseline", str(base),
                "--expect-shards", "sunrise-sync=2"),
            2, "measured at more than one revision", "a" * 40)
        self.assertEqual(json.loads(base.read_text())["crates"], {})

    def test_shards_agreeing_on_the_revision_may_differ_in_date(self):
        # Thirteen CI shards start together and can finish either side of
        # midnight UTC. Refusing that would make the nightly unrecordable
        # for reasons that have nothing to do with the measurement.
        first = outcomes_file(
            self.tmp / "one" / "outcomes.json", "sunrise-sync", caught=1)
        second = outcomes_file(
            self.tmp / "two" / "outcomes.json", "sunrise-sync", caught=1,
            revision={**MEASURED_AT, "date": "2026-09-16"})
        base = baseline_file(self.tmp / "base.json", {})
        self.assert_code(
            self.run_gate(
                str(first), str(second), "--update", "--baseline", str(base),
                "--expect-shards", "sunrise-sync=2"),
            0)
        origin = json.loads(
            base.read_text())["crates"]["sunrise-sync"]["provenance"]
        # The earliest: the day the measurement began.
        self.assertEqual(origin["date"], "2026-09-15")

    def test_a_revision_stamp_with_a_blank_sha_is_2(self):
        run = outcomes_file(
            self.tmp / "a.json", "sunrise-sync", caught=1,
            revision={**MEASURED_AT, "sha": "   "})
        base = baseline_file(self.tmp / "base.json", {})
        self.assert_code(
            self.run_gate(
                str(run), "--update", "--baseline", str(base),
                "--expect-shards", "sunrise-sync=1"),
            2, 'records no "sha"')

    def test_a_revision_stamp_with_no_date_is_2(self):
        run = outcomes_file(
            self.tmp / "a.json", "sunrise-sync", caught=1,
            revision={"sha": "b" * 40})
        base = baseline_file(self.tmp / "base.json", {})
        self.assert_code(
            self.run_gate(
                str(run), "--update", "--baseline", str(base),
                "--expect-shards", "sunrise-sync=1"),
            2, 'records no "date"')

    def test_a_revision_stamp_with_no_dirty_flag_is_2(self):
        # The stamp the gate's own refusal text tells a person to write
        # by hand. Read as `false` — a missing key is falsy — it banked a
        # floor saying the measured tree was clean, which nobody had
        # asserted and nobody could check afterwards. Every other place
        # in this design reads an absent `dirty` as *unknown*; a stamp is
        # written at the one moment the question is answerable, so it is
        # required there rather than defaulted.
        run = outcomes_file(
            self.tmp / "a.json", "sunrise-sync", caught=1,
            revision={"sha": "b" * 40, "date": "2026-09-15"})
        base = baseline_file(self.tmp / "base.json", {})
        self.assert_code(
            self.run_gate(
                str(run), "--update", "--baseline", str(base),
                "--expect-shards", "sunrise-sync=1"),
            2, 'records no boolean "dirty"')
        # Nothing banked: the refusal is before the file is touched.
        self.assertEqual(json.loads(base.read_text())["crates"], {})

    def test_a_revision_stamp_with_a_non_boolean_dirty_flag_is_2(self):
        # The same trap the baseline's own `dirty` is typed against, one
        # file upstream: `"false"` is a string, every string is truthy,
        # and it reads as clean to a person and as dirty to the code.
        run = outcomes_file(
            self.tmp / "a.json", "sunrise-sync", caught=1,
            revision={**MEASURED_AT, "dirty": "false"})
        base = baseline_file(self.tmp / "base.json", {})
        self.assert_code(
            self.run_gate(
                str(run), "--update", "--baseline", str(base),
                "--expect-shards", "sunrise-sync=1"),
            2, 'records no boolean "dirty"')

    def test_one_dirty_shard_makes_the_whole_floor_dirty(self):
        # `dirty` is the union across the shards, not the last one read.
        # Thirteen shards measure one crate between them, so one of them
        # seeing uncommitted changes is enough to make the floor they
        # produce unreproducible at the revision beside it — and the
        # single-shard case cannot tell a union from an overwrite.
        first = outcomes_file(
            self.tmp / "one" / "outcomes.json", "sunrise-sync", caught=1)
        second = outcomes_file(
            self.tmp / "two" / "outcomes.json", "sunrise-sync", caught=1,
            revision={**MEASURED_AT, "dirty": True})
        base = baseline_file(self.tmp / "base.json", {})
        result = self.run_gate(
            str(first), str(second), "--update", "--baseline", str(base),
            "--expect-shards", "sunrise-sync=2")
        self.assert_code(result, 0, "modified working tree")
        origin = json.loads(
            base.read_text())["crates"]["sunrise-sync"]["provenance"]
        self.assertIs(origin["dirty"], True)

    def test_clean_shards_together_stay_clean(self):
        # The other direction of the same union, so that "always true"
        # is not a way of passing the case above.
        first = outcomes_file(
            self.tmp / "one" / "outcomes.json", "sunrise-sync", caught=1)
        second = outcomes_file(
            self.tmp / "two" / "outcomes.json", "sunrise-sync", caught=1)
        base = baseline_file(self.tmp / "base.json", {})
        result = self.run_gate(
            str(first), str(second), "--update", "--baseline", str(base),
            "--expect-shards", "sunrise-sync=2")
        self.assert_code(result, 0)
        self.assertNotIn("modified working tree", result.stderr)
        origin = json.loads(
            base.read_text())["crates"]["sunrise-sync"]["provenance"]
        self.assertIs(origin["dirty"], False)

    def test_the_stamp_refusal_names_all_three_required_fields(self):
        # The refusal invites a hand-written stamp, so it has to say what
        # one contains. Naming two of three is how the `dirty`-less stamp
        # got written in the first place.
        run = outcomes_file(
            self.tmp / "a.json", "sunrise-sync", caught=1, revision=None)
        base = baseline_file(self.tmp / "base.json", {})
        result = self.run_gate(
            str(run), "--update", "--baseline", str(base),
            "--expect-shards", "sunrise-sync=1")
        self.assert_code(result, 2)
        for field in ("sha", "date", "dirty"):
            self.assertIn(f'"{field}"', result.stderr)

    def test_the_stamp_is_found_one_level_above_the_outcomes(self):
        # cargo-mutants always writes `mutants.out/` under the directory
        # it is handed, so the run directory the task created is one
        # level up. Both layouts have to work or the local task and the
        # CI artifacts disagree about where the stamp lives.
        run = outcomes_file(
            self.tmp / "run" / "mutants.out" / "outcomes.json",
            "sunrise-sync", caught=1, revision=None)
        stamp_file(self.tmp / "run", MEASURED_AT)
        base = baseline_file(self.tmp / "base.json", {})
        self.assert_code(
            self.run_gate(
                str(run), "--update", "--baseline", str(base),
                "--expect-shards", "sunrise-sync=1"),
            0, "recorded 1 crate(s)")

    # --- the bootstrap: a baseline written before provenance existed -----

    def test_update_can_re_record_a_crate_in_a_legacy_baseline(self):
        # Reachable by reverting a floor-recording commit, by --update
        # against a baseline restored from an older ref, or by running
        # this script against a release branch's. With the full check
        # running before the write, the entry `--update` is about to
        # replace blocks its own replacement, and the only recovery is to
        # hand-edit the one field the design says must never be
        # hand-added — after a measurement that costs hours.
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = baseline_file(
            self.tmp / "base.json", {"sunrise-sync": 50.0}, provenance=None)
        self.assert_code(
            self.run_gate(
                str(run), "--update", "--baseline", str(base),
                "--expect-shards", "sunrise-sync=1"),
            0, "recorded 1 crate(s)")
        origin = json.loads(
            base.read_text())["crates"]["sunrise-sync"]["provenance"]
        self.assertEqual(origin["sha"], MEASURED_AT["sha"])

    def test_a_legacy_crate_the_run_did_not_measure_is_reported_after(self):
        # The deferred half. The crate that was measured is recorded —
        # the repair proceeds one crate at a time — and the file is
        # reported as not yet usable rather than as a clean write,
        # because the nightly gate cannot read it until the rest are
        # re-recorded too.
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = baseline_file(
            self.tmp / "base.json",
            {"sunrise-sync": 50.0, "sunrise-crypto": 70.0}, provenance=None)
        result = self.run_gate(
            str(run), "--update", "--baseline", str(base),
            "--expect-shards", "sunrise-sync=1")
        self.assert_code(
            result, 2, "recorded 1 crate(s)",
            "not yet a usable baseline",
            '"crates.sunrise-crypto.provenance"')
        crates = json.loads(base.read_text())["crates"]
        self.assertEqual(
            crates["sunrise-sync"]["provenance"]["sha"], MEASURED_AT["sha"])
        self.assertNotIn("provenance", crates["sunrise-crypto"])

    def test_a_baseline_of_the_wrong_shape_still_blocks_update(self):
        # The bootstrap relaxes the provenance requirement and nothing
        # else. A `crates` value that is not an object is not a legacy
        # file, it is a broken one, and merging into it would lose
        # whatever is there.
        run = outcomes_file(self.tmp / "a.json", "sunrise-sync", caught=1)
        base = self.tmp / "base.json"
        base.write_text(json.dumps({"crates": []}))
        self.assert_code(
            self.run_gate(
                str(run), "--update", "--baseline", str(base),
                "--expect-shards", "sunrise-sync=1"),
            2, "cannot use", '"crates" is list')

    # --- --record-revision: where the stamp comes from -------------------

    def test_record_revision_writes_the_head_of_the_tree(self):
        target = self.tmp / "out" / "revision.json"
        result = self.run_gate("--record-revision", str(target))
        self.assert_code(result, 0, "clean")
        head = subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=self.tmp,
            capture_output=True, text=True, check=True).stdout.strip()
        document = json.loads(target.read_text())
        self.assertEqual(document["sha"], head)
        self.assertIs(document["dirty"], False)
        self.assertRegex(document["date"], r"\A\d{4}-\d{2}-\d{2}\Z")

    def test_record_revision_sees_a_modified_tree(self):
        (self.tmp / "tracked.txt").write_text("committed\n")
        for args in (
            ("git", "add", "tracked.txt"),
            ("git", "commit", "--quiet", "-m", "tracked"),
        ):
            subprocess.run(args, cwd=self.tmp, capture_output=True,
                           text=True, check=True)
        (self.tmp / "tracked.txt").write_text("edited\n")
        target = self.tmp / "revision.json"
        self.assert_code(
            self.run_gate("--record-revision", str(target)), 0, "dirty")
        self.assertIs(json.loads(target.read_text())["dirty"], True)

    def test_record_revision_outside_a_repository_is_2(self):
        # The arm that `check=True` made unreachable. `git rev-parse
        # HEAD` exits non-zero and prints nothing, and a stamp with a
        # blank sha has the right shape and says nothing.
        outside = tempfile.TemporaryDirectory()
        self.addCleanup(outside.cleanup)
        where = pathlib.Path(outside.name)
        target = where / "revision.json"
        self.assert_code(
            self.run_gate("--record-revision", str(target), cwd=where),
            2, "printed nothing")
        self.assertFalse(target.exists())

    def test_record_revision_with_no_git_on_path_is_2(self):
        # Distinct news from the case above: no repository tooling at
        # all, rather than no repository.
        target = self.tmp / "revision.json"
        result = subprocess.run(
            [sys.executable, str(GATE), "--record-revision", str(target)],
            cwd=self.tmp, capture_output=True, text=True,
            env={**os.environ, "PATH": str(self.tmp / "empty")},
        )
        self.assert_code(result, 2, "cannot run git")
        self.assertFalse(target.exists())

    def test_no_outcomes_and_no_record_revision_is_2(self):
        self.assert_code(self.run_gate("--update"), 2, "usage")


class ShippedBaselineIsUsable(unittest.TestCase):
    """The baseline this repository actually ships passes its own validator.

    Every other test here synthesises a fixture. This one reads
    `mutants/baseline.json`, which is the file the nightly gate loads, and
    is the check that would have caught adding `provenance` to the
    validator without adding it to the committed floors.
    """

    def test_committed_baseline_is_well_formed(self):
        baseline = REPO / "mutants" / "baseline.json"
        document = json.loads(baseline.read_text())
        sys.path.insert(0, str(GATE.parent))
        try:
            spec = importlib.util.spec_from_file_location(
                "mutants_gate_under_test", GATE)
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
        finally:
            sys.path.pop(0)
        self.assertIsNone(module.malformed(document))


if __name__ == "__main__":
    unittest.main(verbosity=2)
