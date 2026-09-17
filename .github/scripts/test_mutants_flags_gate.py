#!/usr/bin/env python3
"""The exit-code contract of `mutants-flags-gate.py`, as assertions.

Why this file exists
--------------------

The gate's whole value is that it goes red when two files stop agreeing,
and the only way to see that happen is to hand it two files that disagree.
Editing the repository's own `mise.toml` to check would be a change nobody
wants committed, and running the gate by hand against a tree that happened
to be on disk is what every other gate here learned not to rely on. So
every case below synthesises its own pair of files in a temp directory and
passes them with `--mise` and `--workflow`. Nothing here reads the
repository's `mise.toml` or `.github/workflows/ci.yml` — the
`mutants-flags-gate` job does that, and a contract test that also did
would go red for whatever the tree happens to be rather than for a change
to the contract.

The distinction the exit codes carry
------------------------------------

1 means the two places disagree and the remedy is to put the flag back. 2
means the gate could not check anything — a file is unreadable, or one of
them no longer invokes cargo-mutants at all. They are opposite findings
and only one of them is about the flags: a reader who sees a red
`mutants-flags-gate` and assumes 1 will go looking for a deleted argument
when the matrix has actually been rewritten out from under the gate.

The case that matters most is `test_comment_mentioning_the_flag_is_not_a_pass`.
Both real files discuss `--all-features` in prose and `ci.yml` quotes a
complete `cargo mutants --list ... --all-features` command in a comment. A
gate that counted those would pass on the strength of a sentence about the
flag while the command underneath it had lost it, which is an exact
inversion of what it is for.

Run it with `mise run mutants-flags-gate-test`, or directly.
"""

from __future__ import annotations

import pathlib
import subprocess
import sys
import tempfile
import unittest

GATE = pathlib.Path(__file__).resolve().parent / "mutants-flags-gate.py"

# The shapes the two real files have, reduced to what the gate reads: one
# invocation on a single line, and one spread over continuations.
MISE_WITH_FLAG = """\
[tasks.mutants]
description = "Run cargo-mutants over one scoped crate"
run = '''
cargo mutants -p "$usage_crate" --all-features --jobs 1 --output "out/x"
'''
"""

MISE_WITHOUT_FLAG = MISE_WITH_FLAG.replace(" --all-features", "")

CI_WITH_FLAG = """\
jobs:
  mutants:
    steps:
      - name: Mutate
        run: |
          cargo mutants \\
            -p ${{ matrix.crate }} \\
            --all-features \\
            --jobs 1
"""

CI_WITHOUT_FLAG = CI_WITH_FLAG.replace("            --all-features \\\n", "")


class FlagsGateContract(unittest.TestCase):
    """One temp directory per test; the gate is pointed at what is in it."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = pathlib.Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)

    def run_gate(self, mise: str | None, ci: str | None):
        mise_path = self.tmp / "mise.toml"
        ci_path = self.tmp / "ci.yml"
        if mise is not None:
            mise_path.write_text(mise)
        if ci is not None:
            ci_path.write_text(ci)
        return subprocess.run(
            [sys.executable, str(GATE),
             "--mise", str(mise_path), "--workflow", str(ci_path)],
            cwd=self.tmp, capture_output=True, text=True,
        )

    def assert_code(self, result, expected: int, *fragments: str) -> None:
        output = result.stdout + result.stderr
        self.assertEqual(
            result.returncode, expected,
            f"expected exit {expected}, got {result.returncode}\n{output}")
        for fragment in fragments:
            self.assertIn(fragment, output)

    # --- 0: both invocations carry the flag ------------------------------

    def test_both_carrying_the_flag_is_0(self):
        self.assert_code(
            self.run_gate(MISE_WITH_FLAG, CI_WITH_FLAG),
            0, "2 cargo-mutants invocation(s) carry --all-features")

    def test_the_flag_is_found_across_a_continuation(self):
        # The property the whole joining step exists for: in `ci.yml` the
        # flag is on its own line, four lines below the command. Without
        # joining, this fixture reads as an invocation with no flag and
        # the gate is red on a correct tree — the failure mode that gets
        # a gate switched off in a week.
        result = self.run_gate(MISE_WITH_FLAG, CI_WITH_FLAG)
        self.assert_code(result, 0)
        self.assertNotIn("missing", result.stderr)

    # --- 1: an invocation is missing the flag ----------------------------

    def test_mise_dropping_the_flag_is_1(self):
        self.assert_code(
            self.run_gate(MISE_WITHOUT_FLAG, CI_WITH_FLAG),
            1, "--all-features is missing from 1 of 2", "mise.toml:4")

    def test_ci_dropping_the_flag_is_1(self):
        self.assert_code(
            self.run_gate(MISE_WITH_FLAG, CI_WITHOUT_FLAG),
            1, "--all-features is missing from 1 of 2", "ci.yml:6")

    def test_both_dropping_the_flag_is_1(self):
        # Two files that agree with each other and disagree with the rule.
        # A gate that only compared them to each other would pass here,
        # which is why this one checks each against the flag instead.
        self.assert_code(
            self.run_gate(MISE_WITHOUT_FLAG, CI_WITHOUT_FLAG),
            1, "--all-features is missing from 2 of 2")

    def test_the_failure_names_the_third_prose_copy(self):
        # The rule is stated in three places and this gate checks two. The
        # one it cannot check has to be named where somebody is already
        # editing the flags, or it silently falls out of step.
        self.assert_code(
            self.run_gate(MISE_WITHOUT_FLAG, CI_WITH_FLAG),
            1, "docs/10-cross-cutting/testing.md")

    def test_comment_mentioning_the_flag_is_not_a_pass(self):
        # An invocation with the flag deleted, directly under a comment
        # quoting a complete command that has it. Both real files contain
        # comments of exactly this shape.
        self.assert_code(
            self.run_gate(
                MISE_WITHOUT_FLAG,
                "jobs:\n"
                "  mutants:\n"
                "    steps:\n"
                "      # measured with `cargo mutants --list --all-features`\n"
                "      - run: cargo mutants -p x --jobs 1\n",
            ),
            1, "--all-features is missing from 2 of 2")

    def test_a_comment_is_not_counted_as_an_invocation(self):
        # The other half of the same claim: a file whose only mention of
        # cargo-mutants is a comment has no invocation, and that is a 2
        # rather than a 0. Otherwise commenting the matrix out would read
        # as clean.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG,
                "jobs:\n"
                "  mutants:\n"
                "    steps:\n"
                "      # - run: cargo mutants -p x --all-features\n",
            ),
            2, "no `cargo mutants` invocation")

    # --- 2: the gate could not check anything ----------------------------

    def test_no_invocation_in_the_workflow_is_2(self):
        self.assert_code(
            self.run_gate(MISE_WITH_FLAG, "jobs:\n  build:\n    steps: []\n"),
            2, "no `cargo mutants` invocation", "ci.yml")

    def test_no_invocation_in_mise_is_2(self):
        self.assert_code(
            self.run_gate("[tasks.test]\nrun = \"cargo test\"\n",
                          CI_WITH_FLAG),
            2, "no `cargo mutants` invocation", "mise.toml")

    def test_a_missing_file_is_2(self):
        self.assert_code(
            self.run_gate(None, CI_WITH_FLAG), 2, "cannot read")

    def test_an_empty_workflow_is_2_not_0(self):
        # The failure mode this gate's two codes exist for: nothing to
        # check is not the same news as nothing wrong.
        result = self.run_gate(MISE_WITH_FLAG, "")
        self.assert_code(result, 2)
        self.assertNotIn("OK:", result.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
