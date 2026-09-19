#!/usr/bin/env python3
"""The exit-code contract of `mutants-flags-gate.py`, as assertions.

Why this file exists
--------------------

The gate's whole value is that it goes red when two files stop agreeing,
and the only way to see that happen is to hand it two files that disagree.
Editing the repository's own `mise.toml` to check would be a change nobody
wants committed, and running the gate by hand against a tree that happened
to be on disk is what every other gate here learned not to rely on. So
every case below synthesises its own files in a temp directory and names
them on the command line. Nothing here reads the repository's `mise.toml`
or `.github/workflows/ci.yml` — the `mutants-flags-gate` job does that,
and a contract test that also did would go red for whatever the tree
happens to be rather than for a change to the contract.

The exception is deliberate: `run_gate_with_defaults` runs the gate with
no arguments inside a temp directory laid out like a repository, because
the default file set is the one part of the gate that naming files
bypasses, and until something exercised it a typo in the default workflow
directory would have shipped behind a check that blocks nothing.

The distinction the exit codes carry
------------------------------------

1 means an invocation is missing the flag and the remedy is to put it
back. 2 means the gate could not check anything — a file is unreadable,
or nothing it read invokes cargo-mutants at all. They are opposite
findings and only one of them is about the flags: a reader who sees a red
`mutants-flags-gate` and assumes 1 will go looking for a deleted argument
when the matrix has actually been rewritten out from under the gate.

The second is judged over the union of the files rather than per file.
One file with no invocation beside another that has one is an ordinary
tree — the matrix moved — and calling it a broken gate is how a gate gets
switched off.

The cases that matter most are the three under §"text that is not the
invocation". Each is a shape that was constructed against real copies of
this repository's own `mise.toml` and `.github/workflows/ci.yml`, with the
flag deleted from the invocation that measures the floor, and each one
reported exit 0 from a gate whose unit of checking was a joined logical
line tested by substring containment:

* a trailing `# dropped --all-features temporarily`, because only
  whole-line comments were dropped;
* a preceding `echo "we run with --all-features" && …` on the same
  joined line;
* an ordinary `cargo mutants --list … --all-features > population.txt &&`
  chained in front of the measuring invocation — a one-line,
  entirely non-adversarial edit.

All three now fail with exit 1. They are the reason the gate splits a
logical line into commands and tests the flag against the tokens of the
command whose first two are `cargo mutants`, rather than asking whether
the text of the line contains the flag anywhere.

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
        """The ordinary pair: a mise config and a workflow, both named."""
        mise_path = self.tmp / "mise.toml"
        ci_path = self.tmp / "ci.yml"
        if mise is not None:
            mise_path.write_text(mise)
        if ci is not None:
            ci_path.write_text(ci)
        return self.run_gate_on(mise_path, ci_path)

    def run_gate_on(self, *paths: pathlib.Path):
        """The gate over exactly these paths, passed positionally."""
        return subprocess.run(
            [sys.executable, str(GATE), *(str(path) for path in paths)],
            cwd=self.tmp, capture_output=True, text=True,
        )

    def run_gate_with_defaults(self):
        """The gate with no arguments, so it discovers its own file set.

        The defaults are the one part of the gate the live
        `Mutation flag gate` job exercises and nothing else did: every
        other case here names its files, by design. A typo in the default
        workflow directory would have shipped with only a non-required
        job standing between it and master.
        """
        return subprocess.run(
            [sys.executable, str(GATE)],
            cwd=self.tmp, capture_output=True, text=True,
        )

    def write(self, relative: str, text: str) -> pathlib.Path:
        path = self.tmp / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        return path

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
        # cargo-mutants is a comment has no invocation. Handed nothing
        # else, the gate must say it checked nothing rather than report a
        # clean pass, or commenting the matrix out would read as green.
        commented = self.write(
            "only-a-comment.yml",
            "jobs:\n"
            "  mutants:\n"
            "    steps:\n"
            "      # - run: cargo mutants -p x --all-features\n",
        )
        self.assert_code(
            self.run_gate_on(commented), 2, "no `cargo mutants` invocation")

    # --- text that is not the invocation ---------------------------------
    #
    # Three shapes that each defeated a line-and-substring gate against
    # real copies of this repository's own files. Every one of them left
    # the measuring invocation without the flag and reported exit 0.

    def test_a_trailing_comment_mentioning_the_flag_is_not_a_pass(self):
        # Shape A. `logical_lines` drops comments that occupy a whole
        # line; this one sits at the end of the invocation's own line, so
        # the flag was "present" on the line and absent from the command.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    " --all-features",
                    "").replace(
                    '--output "out/x"',
                    '--output "out/x"  # dropped --all-features temporarily'),
                CI_WITH_FLAG,
            ),
            1, "--all-features is missing from 1 of 2")

    def test_a_neighbouring_command_mentioning_the_flag_is_not_a_pass(self):
        # Shape B. A joined `echo` in front of the invocation. Nothing
        # about it is adversarial — a task that announces what it is
        # about to do looks exactly like this.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    'cargo mutants -p "$usage_crate" --all-features',
                    'echo "note: we run with --all-features" && \\\n'
                    'cargo mutants -p "$usage_crate"'),
                CI_WITH_FLAG,
            ),
            1, "--all-features is missing from 1 of 2")

    def test_two_invocations_on_one_line_are_counted_separately(self):
        # Shape C, and the one that makes the case for splitting rather
        # than for merely dropping trailing comments. A `--list` call
        # chained in front of the measuring invocation carries the flag
        # legitimately; the invocation that produces the floor does not.
        # A line-counting gate saw one unit, found the flag in it, and
        # reported OK — which is the 27.17%-vs-36.89% corruption this
        # gate exists to prevent, wearing a green tick.
        result = self.run_gate(
            MISE_WITH_FLAG,
            CI_WITH_FLAG.replace(
                "          cargo mutants \\\n",
                "          cargo mutants --list -p ${{ matrix.crate }} "
                "--all-features > population.txt && \\\n"
                "          cargo mutants \\\n").replace(
                "            --all-features \\\n", ""),
        )
        # Three invocations, not two: the count is what says the unit is
        # an invocation and not a line.
        self.assert_code(result, 1, "missing from 1 of 3")

    def test_a_list_call_is_held_to_the_flag_too(self):
        # The converse of the case above, stated so the rule is not read
        # as "only the measuring invocation counts". Every `cargo mutants`
        # command is held to the flag, because a `--list` that enumerates
        # a different population from the one being measured is the same
        # class of divergence.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG,
                CI_WITH_FLAG.replace(
                    "          cargo mutants \\\n",
                    "          cargo mutants --list -p x > population.txt "
                    "&& \\\n"
                    "          cargo mutants \\\n"),
            ),
            1, "missing from 1 of 3")

    def test_a_separator_inside_a_quoted_argument_is_not_a_separator(self):
        # The splitter has to be quote-aware or it invents commands: an
        # argument containing `;` or `|` cuts the invocation in half, and
        # a flag written after that argument lands in a fragment that is
        # no longer the invocation. The gate then calls a correct tree
        # red — which is how a gate gets switched off in a week. The flag
        # sits after the quoted separator here deliberately; before it,
        # the case passes whether or not quotes are honoured.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    ' --all-features', '').replace(
                    '--output "out/x"',
                    '--exclude-re "a|b; c" --output "out/x" --all-features'),
                CI_WITH_FLAG,
            ),
            0, "2 cargo-mutants invocation(s) carry")

    def test_a_trailing_comment_cannot_supply_the_flag_at_all(self):
        # The stronger form of shape A: the comment is the *only* place
        # the flag appears, and it is attached to the invocation with no
        # separator in between, so nothing but comment-stripping keeps it
        # out of the command's tokens.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    ' --all-features --jobs 1 --output "out/x"',
                    ' --jobs 1 --output "out/x" # --all-features'),
                CI_WITH_FLAG,
            ),
            1, "--all-features is missing from 1 of 2")

    # --- the file set ----------------------------------------------------

    def test_a_third_file_holding_an_invocation_is_checked(self):
        # The hazard the two hard-coded paths left open: an executable
        # copy somewhere the gate was never told about stayed invisible
        # while the gate reported OK on the two it knew.
        third = self.write(
            "release.yml",
            "jobs:\n"
            "  audit:\n"
            "    steps:\n"
            "      - run: cargo mutants -p x --jobs 1\n",
        )
        mise = self.write("mise.toml", MISE_WITH_FLAG)
        ci = self.write("ci.yml", CI_WITH_FLAG)
        self.assert_code(
            self.run_gate_on(mise, ci, third),
            1, "missing from 1 of 3", "release.yml")

    def test_the_defaults_are_mise_plus_every_workflow(self):
        # Exercises `default_paths()` itself, which every other case here
        # bypasses by naming its files. A workflow the defaults do not
        # reach is a file nothing checks.
        self.write("mise.toml", MISE_WITH_FLAG)
        self.write(".github/workflows/ci.yml", CI_WITH_FLAG)
        self.write(
            ".github/workflows/nightly.yaml",
            "jobs:\n  m:\n    steps:\n"
            "      - run: cargo mutants -p x --jobs 1\n")
        result = self.run_gate_with_defaults()
        self.assert_code(result, 1, "missing from 1 of 3")
        self.assertIn("nightly.yaml", result.stderr)

    def test_the_defaults_pass_on_a_tree_that_is_in_step(self):
        self.write("mise.toml", MISE_WITH_FLAG)
        self.write(".github/workflows/ci.yml", CI_WITH_FLAG)
        self.assert_code(
            self.run_gate_with_defaults(),
            0, "2 cargo-mutants invocation(s) carry")

    # --- 2: the gate could not check anything ----------------------------

    def test_a_file_with_no_invocation_beside_one_that_has_it_is_not_2(self):
        # Judged over the union, not per file. Moving the matrix out of
        # one workflow and into another leaves a tree that is entirely in
        # step; reporting that as a broken gate is how a gate gets
        # switched off.
        self.assert_code(
            self.run_gate(MISE_WITH_FLAG, "jobs:\n  build:\n    steps: []\n"),
            0, "1 cargo-mutants invocation(s) carry")

    def test_no_invocation_anywhere_is_2(self):
        self.assert_code(
            self.run_gate("[tasks.test]\nrun = \"cargo test\"\n",
                          "jobs:\n  build:\n    steps: []\n"),
            2, "no `cargo mutants` invocation in any of",
            "mise.toml", "ci.yml")

    def test_a_missing_file_is_2(self):
        self.assert_code(
            self.run_gate(None, CI_WITH_FLAG), 2, "cannot read")

    def test_empty_files_are_2_not_0(self):
        # The failure mode this gate's two codes exist for: nothing to
        # check is not the same news as nothing wrong.
        result = self.run_gate("", "")
        self.assert_code(result, 2)
        self.assertNotIn("OK:", result.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
