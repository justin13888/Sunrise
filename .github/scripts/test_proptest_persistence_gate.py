#!/usr/bin/env python3
"""The exit-code contract of `proptest-persistence-gate.py`, as assertions.

Why this file exists
--------------------

The gate's own `--self-test` runs `scan_source` over six string literals,
which covers the `config` and `direct` rules and nothing else. Three of the
five codes it can emit — `ignored`, `untracked` and `flat` — are decided by
the *filesystem and the index* rather than by the source text, so they
cannot be reached from a string literal at all, and nothing exercised them.
Neither did anything exercise the listing that decides which files are
integration tests, the "no `<crate>/tests/*.rs`" route that says the scan is
broken rather than the tree is clean, or the two exit codes those produce.

Both directions are here. `Rejects` pins all five codes; `Accepts` pins the
shapes a plausible tightening would start reporting — a `proptest!` named in
prose, a proptest under `src/` where the default works, a config built by a
helper beside the block, a brace inside a string literal. #118's convention
is worth holding; a gate that fires on a doc comment is not.

Fixtures are synthesised crates
-------------------------------

Every case builds a git repository in a temp directory with one crate and
one integration test, and drives the gate with `--root`. Nothing here reads
this repository's own tests: the `proptest-persistence` job does that, and a
contract test that also did would go red for whatever property test somebody
added this week. The gate shells out to `git ls-files` and
`git check-ignore`, which is why the fixture is a repository and not a bare
directory.

Run it with `python3 .github/scripts/test_proptest_persistence_gate.py`.
"""

from __future__ import annotations

import pathlib
import subprocess
import sys
import tempfile
import unittest

GATE = pathlib.Path(__file__).resolve().parent / "proptest-persistence-gate.py"

CLEAN = 0
VIOLATION = 1
COULD_NOT_RUN = 2

CRATE = "crates/c"
TEST = f"{CRATE}/tests/redaction.rs"
WANT = f"{CRATE}/proptest-regressions/tests/redaction.txt"

CONFIGURED = '''
proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/redaction.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]

    #[test]
    fn holds(s in "[a-z]{1,4}") {
        prop_assert!(!s.is_empty(), "a brace { in a string is not a brace");
    }
}
'''

NO_CONFIG = '''
proptest! {
    #[test]
    fn holds(x in 0u8..4) { prop_assert!(x < 4); }
}
'''

WRONG_PATH = '''
proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/somewhere_else.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]
    #[test]
    fn holds(x in 0u8..4) { prop_assert!(x < 4); }
}
'''


class GateCase(unittest.TestCase):
    """One temp repository with one crate, and one run of the gate over it."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = pathlib.Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)
        self.repo = self.tmp / "repo"
        self.repo.mkdir()
        self._git("init", "-q")
        self._git("config", "user.email", "gate@example.invalid")
        self._git("config", "user.name", "Gate")

    def _git(self, *args: str) -> None:
        subprocess.run(
            ["git", "-C", str(self.repo), *args], check=True, capture_output=True, text=True
        )

    def write(self, rel: str, text: str, track: bool = True) -> pathlib.Path:
        path = self.repo / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        if track:
            self._git("add", "--force", "--", rel)
        return path

    def integration_test(self, text: str, name: str = TEST) -> None:
        """One `<crate>/tests/*.rs`, which is the unit this gate scans."""
        self.write(name, text)

    def run_gate(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [sys.executable, str(GATE), "--root", str(self.repo), *args],
            capture_output=True,
            text=True,
            cwd=self.tmp,
        )

    def assert_code(self, result: subprocess.CompletedProcess, code: int, *fragments: str) -> None:
        output = result.stdout + result.stderr
        self.assertEqual(
            result.returncode, code, f"expected exit {code}, got {result.returncode}:\n{output}"
        )
        for fragment in fragments:
            self.assertIn(fragment, output)


class SelfTest(GateCase):
    """The source-level rules, asserted before the gate reads a tree."""

    def test_the_self_test_passes_alone(self):
        result = subprocess.run(
            [sys.executable, str(GATE), "--self-test"], capture_output=True, text=True
        )
        self.assertEqual(result.returncode, CLEAN, result.stdout + result.stderr)
        self.assertIn("OK: proptest-persistence self-test clean", result.stdout)

    def test_the_self_test_runs_as_a_precondition_of_a_check(self):
        self.integration_test("#[test]\nfn t() {}\n")
        self.assertIn("OK: proptest-persistence self-test clean", self.run_gate().stdout)


class Clean(GateCase):
    """What a passing run says, and that it says how much it read."""

    def test_a_configured_proptest_exits_zero(self):
        self.integration_test(CONFIGURED)
        self.assert_code(
            self.run_gate(),
            CLEAN,
            "OK: proptest-persistence clean (1 proptest file(s) of 1 integration tests).",
        )

    def test_a_committed_counterexample_file_is_what_the_convention_wants(self):
        self.integration_test(CONFIGURED)
        self.write(WANT, "cc 0123456789abcdef\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: proptest-persistence clean")


class Rejects(GateCase):
    """Each of the five codes, including the three the self-test cannot reach."""

    def test_a_block_with_no_proptest_config(self):
        self.integration_test(NO_CONFIG)
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            f"proptest-persistence[config]: {TEST}:",
            "falls back to `SourceParallel`",
        )

    def test_a_file_that_names_no_direct_persistence(self):
        self.integration_test(NO_CONFIG)
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            f"proptest-persistence[direct]: {TEST}:",
            'names no `FileFailurePersistence::Direct("proptest-regressions/tests/redaction.txt")`',
        )

    def test_a_block_copied_from_another_test_keeps_the_wrong_path(self):
        # The point of deriving the expected path from the file rather than
        # matching loosely: two tests sharing a counterexample file is a
        # silent divergence, not a syntax error.
        self.integration_test(WRONG_PATH)
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            "proptest-persistence[direct]",
            "persists to 'proptest-regressions/tests/somewhere_else.txt'",
            "belong at 'proptest-regressions/tests/redaction.txt'",
        )

    def test_a_second_block_added_without_a_config(self):
        # The shape the rule is really for: the first block is right, and the
        # one added beside it later is not.
        self.integration_test(CONFIGURED + NO_CONFIG)
        self.assert_code(self.run_gate(), VIOLATION, "proptest-persistence[config]")

    def test_a_gitignore_rule_over_the_counterexample_path(self):
        # These files are tracked on purpose: a shrunken case is the one test
        # input the suite cannot regenerate. An ignore rule hides them and
        # quietly reintroduces what #118 removed.
        self.integration_test(CONFIGURED)
        self.write(".gitignore", "proptest-regressions/\n")
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            f"proptest-persistence[ignored]: {WANT}:1:",
            "Counterexample files are tracked on purpose",
        )

    def test_a_counterexample_file_that_git_does_not_track(self):
        self.integration_test(CONFIGURED)
        self.write(WANT, "cc 0123456789abcdef\n", track=False)
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            f"proptest-persistence[untracked]: {WANT}:1:",
            "this counterexample file exists and git does not track it.",
        )

    def test_proptests_fallback_output_sitting_beside_the_test(self):
        # The symptom `.gitignore` deliberately does not hide, now that
        # something other than a human reading `git status` looks for it.
        self.integration_test(CONFIGURED)
        flat = f"{CRATE}/tests/redaction.proptest-regressions"
        self.write(flat, "cc 0123456789abcdef\n", track=False)
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            f"proptest-persistence[flat]: {flat}:1:",
            "proptest's fallback output",
        )

    def test_every_problem_is_reported_not_only_the_first(self):
        self.integration_test(NO_CONFIG)
        self.write(f"{CRATE}/tests/other.rs", NO_CONFIG)
        result = self.run_gate()
        self.assert_code(result, VIOLATION, TEST, f"{CRATE}/tests/other.rs", "4 problem(s).")


class Accepts(GateCase):
    """The near-misses. A gate that fires on these is one people ignore."""

    def test_a_proptest_named_in_prose_is_not_a_block(self):
        self.integration_test("//! This comment says `proptest!` and must not count as one.\n\n#[test]\nfn t() {}\n")
        self.assert_code(
            self.run_gate(),
            CLEAN,
            "OK: proptest-persistence clean (0 proptest file(s) of 1 integration tests).",
        )

    def test_a_proptest_named_in_a_string_literal_is_not_a_block(self):
        self.integration_test('#[test]\nfn t() {\n    let _ = "proptest! { }";\n}\n')
        self.assert_code(self.run_gate(), CLEAN, "OK: proptest-persistence clean")

    def test_a_brace_inside_a_string_does_not_end_the_block(self):
        # Brace matching runs over masked text for this reason: a `{` in a
        # string literal is a character, and reading it as a brace closes the
        # block early and loses the config that follows.
        self.integration_test(CONFIGURED)
        self.assert_code(self.run_gate(), CLEAN, "OK: proptest-persistence clean")

    def test_a_config_built_by_a_helper_beside_the_block(self):
        # The convention's own alternative, which `op_envelope_proptest.rs`
        # uses. The rules are "every block is configured, and this file names
        # the right path" rather than a call graph nobody can follow.
        self.integration_test(
            'fn config() -> ProptestConfig {\n'
            '    ProptestConfig {\n'
            '        failure_persistence: Some(Box::new(\n'
            '            proptest::test_runner::FileFailurePersistence::Direct(\n'
            '                "proptest-regressions/tests/redaction.txt",\n'
            '            ),\n'
            '        )),\n'
            '        ..ProptestConfig::default()\n'
            '    }\n'
            '}\n'
            'proptest! {\n'
            '    #![proptest_config(config())]\n'
            '    #[test]\n'
            '    fn holds(x in 0u8..4) { prop_assert!(x < 4); }\n'
            '}\n'
        )
        self.assert_code(self.run_gate(), CLEAN, "OK: proptest-persistence clean")

    def test_a_proptest_under_src_is_out_of_scope(self):
        # The `SourceParallel` default works there, so requiring an explicit
        # setting would be requiring a workaround for a problem that does not
        # exist.
        self.integration_test("#[test]\nfn t() {}\n")
        self.write(f"{CRATE}/src/lib.rs", NO_CONFIG)
        self.assert_code(self.run_gate(), CLEAN, "OK: proptest-persistence clean")

    def test_a_nested_tests_directory_is_not_an_integration_test_target(self):
        # Cargo only builds `<crate>/tests/*.rs` as targets; a helper module
        # a directory deeper is not one.
        self.integration_test("#[test]\nfn t() {}\n")
        self.write(f"{CRATE}/tests/helpers/mod.rs", NO_CONFIG)
        self.assert_code(
            self.run_gate(),
            CLEAN,
            "OK: proptest-persistence clean (0 proptest file(s) of 1 integration tests).",
        )

    def test_an_integration_test_with_no_property_test_is_fine(self):
        self.integration_test("#[test]\nfn ordinary() {}\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: proptest-persistence clean")


class CouldNotRun(GateCase):
    """Exit 2 is "nothing was scanned", and must not read as a verdict."""

    def test_a_tree_with_no_integration_tests_is_a_broken_scan(self):
        # Not "clean". A listing that stopped finding things reports every
        # test as compliant, which is the way a gate dies quietly.
        self.write(f"{CRATE}/src/lib.rs", "pub fn f() {}\n")
        self.assert_code(
            self.run_gate(),
            COULD_NOT_RUN,
            "no `<crate>/tests/*.rs` found; the scan is broken.",
        )

    def test_a_root_outside_a_git_work_tree_is_a_failed_run(self):
        outside = self.tmp / "not-a-repo"
        outside.mkdir()
        result = subprocess.run(
            [sys.executable, str(GATE), "--root", str(outside)],
            capture_output=True,
            text=True,
            cwd=self.tmp,
        )
        self.assertEqual(result.returncode, COULD_NOT_RUN, result.stdout + result.stderr)
        self.assertIn("could not list tracked files", result.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
