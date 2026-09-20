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
bypasses, and until something exercised it a typo in the default file set
would have shipped behind a check that blocks nothing.

The distinction the exit codes carry
------------------------------------

1 means an invocation is missing the flag and the remedy is to put it
back. 2 means the gate could not check anything — a file is unreadable,
or nothing it read invokes cargo-mutants at all. They are opposite
findings and only one of them is about the flags: a reader who sees a red
`mutants-flags-gate` and assumes 1 will go looking for a deleted argument
when the matrix has actually been rewritten out from under the gate.

"Nothing to check" is judged over the union of the files rather than per
file. One file with no invocation beside another that has one is an
ordinary tree — the matrix moved — and calling it a broken gate is how a
gate gets switched off. What the union cannot see is a count that falls
from two to one, so the gate also asserts how many invocations the tree
holds — as an equality, so that a count which grows is as loud as one
that shrinks; §"the file set" holds both halves, and the case that used
to assert exit 0 on a tree with one invocation and no third file now
asserts exit 2, because that tree has lost half of what the gate
compares.

The cases that matter most are the two adversarial sections. Every shape
in them was constructed against real copies of this repository's own
`mise.toml` and `.github/workflows/ci.yml`, with the flag deleted from
the invocation that measures the floor, and every one of them reported
exit 0.

§"text that is not the invocation" is the first set, against a gate whose
unit of checking was a joined logical line tested by substring
containment: a trailing `# dropped --all-features temporarily`, a
preceding `echo "we run with --all-features" && …`, and an ordinary
`cargo mutants --list … --all-features > population.txt &&` chained in
front of the measuring invocation. They are why the gate splits a logical
line into commands and tests the words of each command whose own first
two are `cargo mutants`.

§"text that unquotes, chains or passes through" is the second set,
against that gate: a single `&` instead of `&&`; a second invocation
inside one command, because only the first pair was taken;
`--exclude-re '--all-features'`, because lexing threw the quotes away and
a regex that mentions the flag lexed to the flag; `-- --all-features`,
which is an argument to `cargo test`; and an unbalanced quote in front of
`# keep --all-features later`, which fell back to a whitespace split and
handed the comment's words to the command. Two of that set go the other
way and are just as bad: `--output out/run#3 --all-features` was red on a
correct tree, and `./ci/wrap.sh --tag v1#2 cargo mutants -p x` was not
recognised as an invocation at all.

Run it with `mise run mutants-flags-gate-test`, or directly.
"""

from __future__ import annotations

import pathlib
import re
import subprocess
import sys
import tempfile
import unittest

GATE = pathlib.Path(__file__).resolve().parent / "mutants-flags-gate.py"

# Read out of the gate rather than written down again here. A second
# copy of a number is the thing this gate exists to stop, and a contract
# test that keeps its own copy asserts that the two copies agree instead
# of asserting anything about the tree.
EXPECTED_INVOCATIONS = int(re.search(
    r"^EXPECTED_INVOCATIONS = (\d+)$", GATE.read_text(), re.M).group(1))

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

    def run_gate(self, mise: str | None, ci: str | None, *,
                 expect: int | None = None):
        """The ordinary pair: a mise config and a workflow, both named."""
        mise_path = self.tmp / "mise.toml"
        ci_path = self.tmp / "ci.yml"
        if mise is not None:
            mise_path.write_text(mise)
        if ci is not None:
            ci_path.write_text(ci)
        return self.run_gate_on(mise_path, ci_path, expect=expect)

    def run_gate_on(self, *paths: pathlib.Path, expect: int | None = None):
        """The gate over exactly these paths, passed positionally.

        `expect` is `--expect-invocations`. A case whose fixture holds one
        invocation on purpose says so, because the gate's own default is
        the number this repository holds and a case that is about
        something else should not trip over it.
        """
        stated = ([] if expect is None
                  else ["--expect-invocations", str(expect)])
        return subprocess.run(
            [sys.executable, str(GATE), *stated,
             *(str(path) for path in paths)],
            cwd=self.tmp, capture_output=True, text=True,
        )

    def run_gate_with_defaults(self, *, expect: int | None = None):
        """The gate with no arguments, so it discovers its own file set.

        The defaults are the one part of the gate the live
        `Mutation flag gate` job exercises and nothing else did: every
        other case here names its files, by design. A typo in the default
        file set would have shipped with only a non-required job standing
        between it and master.
        """
        stated = ([] if expect is None
                  else ["--expect-invocations", str(expect)])
        return subprocess.run(
            [sys.executable, str(GATE), *stated],
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

    def test_an_escaped_backslash_does_not_continue_the_line(self):
        # The neighbour of the case above, one backslash along. A line
        # ending `out\\` ends the command — the pair is one literal
        # backslash — but a test that only asks whether the line ends in
        # a backslash joins the next line onto it, and the escaped space
        # then merges the words. Executed: an invocation ending
        # `--output out\\` above an `echo --all-features` reported
        # `OK: 2` with no flag on the command that measures.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    ' --all-features --jobs 1 --output "out/x"',
                    ' --jobs 1 --output out\\\\\necho --all-features'),
                CI_WITH_FLAG,
            ),
            1, "--all-features is missing from 1 of 2")

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

    def test_an_escaped_quote_inside_a_quoted_argument_is_one_argument(self):
        # The case above with one backslash added, and the direction the
        # two-grammar version got wrong on a correct tree. A regex that
        # has to match a double quote is written `"a\"&&b"`. When the
        # line was scanned twice — once to split it, once to lex it —
        # the splitter did not honour the backslash, mis-closed the
        # quotation at it, re-opened at the real closing quote, and cut
        # the invocation in half at the `&&` inside the argument.
        # Executed at the previous head: exit 2 on a tree whose flags
        # are perfect. No fixture in this file contained a
        # backslash-escaped quote at all.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    '--output "out/x"',
                    '--exclude-re "a\\"&&b" --output "out/x"'),
                CI_WITH_FLAG,
            ),
            0, "2 cargo-mutants invocation(s) carry")

    def test_an_escaped_space_and_an_empty_argument_are_ordinary(self):
        # Two shapes a lexer gets to have an opinion about and this one
        # should not: a path with an escaped space in it is one word,
        # and `''` is an argument that happens to be empty. Neither is
        # the flag and neither hides it; the tree is correct and stays
        # green. Unasserted until now, which is how the dead newline in
        # SEPARATORS survived as long as it did.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    '--output "out/x"',
                    "--exclude-re '' --output out/my\\ run"),
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

    def test_an_escaped_quote_does_not_disable_the_comment_rule(self):
        # The case above with one backslash added to the path, and the
        # original trailing-comment hole back verbatim. `--output
        # "out/\"x"` desynchronised the splitter's quote state from the
        # lexer's, and a splitter that believes it is inside a quotation
        # does not honour a `#` either — so the comment's words became
        # the invocation's own and the flag in it vouched for a command
        # that does not have it. Executed: exit 0.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    ' --all-features --jobs 1 --output "out/x"',
                    ' --jobs 1 --output "out/\\"x" # put --all-features back'),
                CI_WITH_FLAG,
            ),
            1, "--all-features is missing from 1 of 2")

    def test_an_escaped_quote_does_not_disable_the_separator(self):
        # The same desync in the other rule the splitter owns. With the
        # rest of the line read as quoted, `&&` stopped ending the
        # command, so a neighbouring `echo` became words of the
        # invocation — which is the shape the split into commands was
        # introduced to make red two rounds ago, reachable again through
        # one backslash. Executed: exit 0.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    ' --all-features --jobs 1 --output "out/x"',
                    ' --jobs 1 --output "out/\\"x" && echo --all-features'),
                CI_WITH_FLAG,
            ),
            1, "--all-features is missing from 1 of 2")

    def test_an_escaped_quote_outside_a_quotation_is_not_a_quotation(self):
        # The third shape: the backslash-escaped quote is not inside a
        # quoted argument at all, so a scanner that honours neither
        # backslashes nor anything after them opens a quotation on a
        # character the shell treats as a literal `"`. Executed: exit 0
        # with the measuring invocation unflagged.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    ' --all-features --jobs 1 --output "out/x"',
                    ' --jobs 1 --output \\"out && echo --all-features'),
                CI_WITH_FLAG,
            ),
            1, "--all-features is missing from 1 of 2")

    # --- text that unquotes, chains or passes through to the flag --------
    #
    # A second round of shapes, each executed against real copies of this
    # repository's own `mise.toml` and `.github/workflows/ci.yml` with the
    # flag deleted from the invocation that measures the floor, and each
    # one reporting exit 0 from the gate that split lines into commands
    # and tested `shlex` tokens for the flag.

    def test_a_single_ampersand_ends_a_command(self):
        # `&&` was a separator and `&` was not, so two shapes one
        # character apart got opposite verdicts. Here the flag belongs to
        # the *next* command — an ordinary `cargo test` run alongside the
        # backgrounded measurement — and with `&` unrecognised the whole
        # thing is one command whose words include the flag, so the
        # measuring invocation passes on the strength of somebody else's
        # argument.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    ' --all-features', '').replace(
                    '--output "out/x"',
                    '--output "out/x" & cargo test --all-features'),
                CI_WITH_FLAG,
            ),
            1, "missing from 1 of 2")

    def test_a_backgrounded_list_call_does_not_vouch_for_the_measurement(self):
        # The shape as it was executed against this repository's own
        # files: round-1's `&&` case with one `&` removed. It reported
        # `OK: 2 … carry --all-features` while the invocation that
        # produces the floor had none. Two properties hold it red now —
        # `&` ends a command, and every `cargo mutants` pair inside a
        # command is an invocation — and it is asserted whole because it
        # is the shape somebody actually types.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    'cargo mutants -p "$usage_crate" --all-features',
                    'cargo mutants --list -p "$usage_crate" --all-features '
                    '> pop.txt & cargo mutants -p "$usage_crate"'),
                CI_WITH_FLAG,
            ),
            1, "missing from 1 of 3")

    def test_the_flag_inside_quotes_does_not_count_as_the_flag(self):
        # `--exclude-re '--all-features'` is a regex that mentions the
        # flag, and lexing threw the quotes away, so it lexed to a token
        # equal to the flag and satisfied the test. A feature selection
        # is a word somebody wrote, not a word something unquoted into.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    ' --all-features',
                    " --exclude-re '--all-features'"),
                CI_WITH_FLAG,
            ),
            1, "missing from 1 of 2")

    def test_a_consistently_quoted_flag_is_the_flag(self):
        # The innocent neighbour of the case above, and the reason that
        # case cannot be the whole rule. `-p x "--all-features"` is the
        # feature selection with quotes around it — the shell delivers
        # exactly the same argument — and a gate that is red on it is
        # red on a correct tree, which this gate's own docstring twice
        # calls how a gate gets switched off in a week. Executed at the
        # previous head: exit 1, both ways round. What separates it from
        # the case above is where the word sits: after `--exclude-re` it
        # is that option's value, and after `-p x` it is a switch.
        for quoted in ('"--all-features"', "'--all-features'"):
            with self.subTest(quoted=quoted):
                self.assert_code(
                    self.run_gate(
                        MISE_WITH_FLAG.replace(' --all-features',
                                               f" {quoted}"),
                        CI_WITH_FLAG,
                    ),
                    0, "2 cargo-mutants invocation(s) carry")

    def test_a_flag_assembled_out_of_parts_is_not_the_flag(self):
        # The other side of the line the case above draws. One
        # consistent pair of quotes around the whole word is the flag
        # written with quotes; anything else is a word that merely
        # unquotes to it. `--all-"features"` is assembled out of pieces
        # and `$'--all-features'` does not even unquote to the flag —
        # ANSI-C quoting is contrived in a workflow, and widening the
        # rule to admit it would buy nothing any real invocation needs.
        for shape in ('--all-"features"', "$'--all-features'"):
            with self.subTest(shape=shape):
                self.assert_code(
                    self.run_gate(
                        MISE_WITH_FLAG.replace(' --all-features',
                                               f" {shape}"),
                        CI_WITH_FLAG,
                    ),
                    1, "missing from 1 of 2")

    def test_a_quoted_flag_after_a_boolean_switch_is_the_flag(self):
        # The shape that made "the predecessor starts with `-`" the wrong
        # question. A boolean switch is option-shaped and takes no value,
        # so the word after it is a switch of its own — and cargo-mutants
        # has plenty of them. All three were executed at the previous
        # head and all three were exit 1 on a tree whose feature
        # selection is genuinely present, which this gate's own docstring
        # twice calls how a gate gets switched off in a week.
        for switch in ("--no-times", "--no-shuffle", "-v", "--in-place",
                       "--list"):
            for quoted in ('"--all-features"', "'--all-features'"):
                with self.subTest(switch=switch, quoted=quoted):
                    self.assert_code(
                        self.run_gate(
                            MISE_WITH_FLAG.replace(
                                ' --all-features',
                                f" {switch} {quoted}"),
                            CI_WITH_FLAG,
                        ),
                        0, "2 cargo-mutants invocation(s) carry")

    def test_the_flag_unquoted_after_a_value_taking_option_is_its_value(self):
        # The other half of the same rule, and the half that used to be
        # missing: the discriminator sat below the source-text test, so
        # the bare spelling never reached it. Executed at the previous
        # head: `--exclude-re --all-features` was EXIT 0 with the
        # measuring invocation carrying no feature selection, while
        # `--exclude-re '--all-features'` — the same word, the same
        # place, one pair of quotes apart — was exit 1.
        for option in ("--exclude-re", "--examine-re", "--output", "-E",
                       "--file", "--in-diff"):
            with self.subTest(option=option):
                self.assert_code(
                    self.run_gate(
                        MISE_WITH_FLAG.replace(
                            ' --all-features', f" {option} --all-features"),
                        CI_WITH_FLAG,
                    ),
                    1, "missing from 1 of 2")

    def test_an_option_that_took_its_value_with_an_equals_leaves_a_switch(self):
        # `--exclude-re=--all-features` is one word, it is not the flag's
        # source text, and the invocation really does carry no feature
        # selection — so it is exit 1, and that verdict is correct rather
        # than an inversion to be repaired. What the `=` spelling must
        # not do is swallow the NEXT word: the option already took its
        # value inside itself, so what follows is a switch again.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    ' --all-features', " --exclude-re=^foo"),
                CI_WITH_FLAG,
            ),
            1, "missing from 1 of 2")
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    ' --all-features', " --exclude-re=^foo --all-features"),
                CI_WITH_FLAG,
            ),
            0, "2 cargo-mutants invocation(s) carry")

    def test_an_unknown_option_shaped_word_lets_the_flag_count(self):
        # The direction the named set is deliberately wrong in. An option
        # this gate has never heard of — a future cargo-mutants flag, a
        # wrapper's own — makes the word after it COUNT, so a set that
        # goes stale costs a false green on a tree somebody wrote oddly
        # and never a false red on a correct one.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    ' --all-features', " --not-a-real-option --all-features"),
                CI_WITH_FLAG,
            ),
            0, "2 cargo-mutants invocation(s) carry")

    def test_the_flag_after_a_double_dash_does_not_count(self):
        # Everything after cargo-mutants' `--` is handed to the test
        # runner. `-- --all-features` is an argument to `cargo test` and
        # says nothing about which features cargo-mutants *built*, which
        # is the only thing the floor depends on.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    ' --all-features --jobs 1',
                    ' --jobs 1').replace(
                    '--output "out/x"', '--output "out/x" -- --all-features'),
                CI_WITH_FLAG,
            ),
            1, "missing from 1 of 2")

    def test_a_quoted_or_escaped_double_dash_is_still_the_passthrough(self):
        # The neighbour of the case above, one quote character along,
        # and the place `raw` equality points the wrong way. Strictness
        # about source text is conservative on the flag — it refuses
        # something that might not be a feature selection — and
        # permissive on the terminator, where it refuses to believe in a
        # `--` the shell delivers anyway. `"--"`, `'--'` and `\--` are
        # all one separator by the time cargo-mutants sees them.
        # Executed at the previous head: exit 0 for each.
        for written in ('"--"', "'--'", "\\--"):
            with self.subTest(written=written):
                self.assert_code(
                    self.run_gate(
                        MISE_WITH_FLAG.replace(
                            ' --all-features --jobs 1',
                            ' --jobs 1').replace(
                            '--output "out/x"',
                            f'--output "out/x" {written} --all-features'),
                        CI_WITH_FLAG,
                    ),
                    1, "missing from 1 of 2")

    def test_a_flag_inside_a_command_substitution_does_not_vouch(self):
        # The lexer modelled quotes and escapes and nothing else, so
        # `$(`, `)` and backticks were ordinary characters and a word
        # belonging to a NESTED command became a word of the invocation
        # around it. Executed at the previous head with a control: this
        # line was exit 0, and the same line with `--all-features` taken
        # out of the substitution was exit 1. The flag is an argument to
        # `cargo metadata`; it says nothing about what cargo-mutants
        # built.
        for substitution in (
            "$(cargo metadata --all-features --no-deps --format-version 1)",
            "`cargo metadata --all-features --no-deps`",
        ):
            with self.subTest(substitution=substitution):
                self.assert_code(
                    self.run_gate(
                        MISE_WITH_FLAG.replace(
                            '-p "$usage_crate" --all-features',
                            f"-p {substitution}"),
                        CI_WITH_FLAG,
                    ),
                    1, "missing from 1 of 2")

    def test_a_flag_outside_a_substitution_still_vouches(self):
        # The innocent neighbour, and the reason the substitution has to
        # be one opaque WORD of the enclosing command rather than
        # nothing at all. Dropping it vacated the argument position it
        # occupied, so `-p $(…)` left `--all-features` sitting where
        # `-p`'s value sits and the gate went red on a correct tree.
        # Treating `$(`, `)` and backticks as separators instead is the
        # same failure by another route: it ends the enclosing command
        # at the substitution, and the flag after it belongs to nothing.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    '-p "$usage_crate" --all-features',
                    "-p $(cargo metadata --no-deps) --all-features"),
                CI_WITH_FLAG,
            ),
            0, "2 cargo-mutants invocation(s) carry")

    def test_an_invocation_inside_a_substitution_is_still_an_invocation(self):
        # Dropping the substitution's words was the other tempting
        # answer and it loses this: a `cargo mutants` written inside
        # `$( )` really runs. It is checked as a command of its own, so
        # it raises the count and is held to the flag like any other.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    'cargo mutants -p "$usage_crate" --all-features',
                    'echo $(cargo mutants -p "$usage_crate")'),
                CI_WITH_FLAG,
            ),
            1, "missing from 1 of 2")

    def test_an_unclosed_command_substitution_is_2_not_a_pass(self):
        # An unbalanced `$(` used to be invisible: every character of it
        # was ordinary, so the line lexed happily and the gate reported
        # OK. It refuses now, for the reason an unbalanced quotation
        # does — a typo must not be a way of passing.
        for opener, closer in (("$(", ")"), ("`", "`")):
            with self.subTest(opener=opener):
                self.assert_code(
                    self.run_gate(
                        MISE_WITH_FLAG.replace(
                            '-p "$usage_crate" --all-features',
                            f"-p {opener}cargo metadata --all-features"),
                        CI_WITH_FLAG,
                    ),
                    2, "for a command substitution", "mise.toml")

    def test_a_separator_inside_a_substitution_does_not_end_the_outer(self):
        # `$(printf '%s' "$shard" | tr / -)` is house style here —
        # mise.toml writes one — and the `|` inside it belongs to the
        # nested command. Ending the enclosing invocation there would
        # strip the flag off everything after it.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    '-p "$usage_crate"',
                    '-p $(printf \'%s\' "$usage_crate" | tr / -)'),
                CI_WITH_FLAG,
            ),
            0, "2 cargo-mutants invocation(s) carry")

    def test_two_invocations_in_one_command_are_both_checked(self):
        # Taking the first `cargo mutants` pair in a command and stopping
        # made the second invisible — the same hole the split into
        # commands closes one level up, one level down. A wrapper handed
        # both is the shape that has no separator to split on.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    'cargo mutants -p "$usage_crate" --all-features',
                    './ci/both.sh cargo mutants -p a --all-features '
                    'cargo mutants -p b'),
                CI_WITH_FLAG,
            ),
            1, "missing from 1 of 3")

    def test_an_invocation_ends_where_the_next_one_begins(self):
        # The case above with the flag on the other one. Both orders
        # have to be red, and only one of them is red for the reason
        # anybody would guess: if an invocation ran to the end of the
        # command rather than to the start of its neighbour, the first
        # one here would be handed the second's `--all-features` and the
        # command would report clean. Unasserted before — the existing
        # order passes whether or not the boundary is drawn.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    'cargo mutants -p "$usage_crate" --all-features',
                    './ci/both.sh cargo mutants -p a '
                    'cargo mutants -p b --all-features'),
                CI_WITH_FLAG,
            ),
            1, "missing from 1 of 3")

    def test_a_command_that_will_not_lex_is_2_not_a_pass(self):
        # An unbalanced quote used to fall back to a whitespace split,
        # which applied no comment rule at all and so handed the words of
        # a trailing comment to the invocation. A typo turned the gate
        # green on an unflagged invocation; now it says it cannot judge
        # the command, which is what exit 2 is for.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    ' --all-features --jobs 1 --output "out/x"',
                    ' --jobs 1 --output "out/x\' # keep --all-features later'),
                CI_WITH_FLAG,
            ),
            2, "this line names cargo-mutants and will not lex as shell")

    def test_an_invocation_the_old_prefilter_could_not_see_is_checked(self):
        # Two grammars for one decision: a regular expression over the
        # RAW text decided whether to lex, and the lexer decided what an
        # invocation was. They disagree. Both of these are real,
        # unflagged invocations — the shell runs cargo-mutants — and
        # neither raw text matches `\bcargo\s+mutants\b`. At the
        # previous head the gate did not report a missing flag; it
        # reported "found 1 … expected exactly 2", a bookkeeping error,
        # and only because the equality happens to exist. Take the count
        # away and that is a silent green.
        for written in ('cargo "mutants"', "car\\go mutants",
                        "'cargo' mutants", "cargo mut''ants"):
            with self.subTest(written=written):
                self.assert_code(
                    self.run_gate(
                        MISE_WITH_FLAG.replace(
                            'cargo mutants -p "$usage_crate" --all-features',
                            f'{written} -p "$usage_crate"'),
                        CI_WITH_FLAG,
                    ),
                    1, "missing from 1 of 2")

    def test_a_line_that_will_not_lex_and_names_nothing_is_skipped(self):
        # The other half, and the reason the prefilter could not simply
        # be deleted. The file set is every *.yml, *.yaml and *.sh under
        # .github/, most of which is not shell: an apostrophe in an
        # ordinary YAML scalar is an unbalanced quote to a shell lexer.
        # Blocking a merge for one is how a gate gets switched off, so a
        # line that will not lex and names no cargo-mutants is skipped
        # and counted.
        self.write("mise.toml", MISE_WITH_FLAG)
        self.write(".github/workflows/ci.yml", CI_WITH_FLAG)
        self.write(
            ".github/workflows/other.yml",
            "jobs:\n  b:\n    steps:\n"
            "      - name: Don't build it by hand\n"
            "        run: true\n")
        result = self.run_gate_with_defaults()
        self.assert_code(result, 0, "2 cargo-mutants invocation(s) carry")
        self.assertIn("did not lex as shell", result.stdout)

    def test_a_line_that_will_not_lex_and_names_the_tool_is_2(self):
        # Escalation on evidence. The gate cannot tell whether this is
        # prose or an invocation it is failing to read, and it says so
        # rather than telling the reader to balance a quote that is an
        # English apostrophe.
        self.write("mise.toml", MISE_WITH_FLAG)
        self.write(".github/workflows/ci.yml", CI_WITH_FLAG)
        self.write(
            ".github/workflows/other.yml",
            "jobs:\n  b:\n    steps:\n"
            "      - name: Don't run cargo mutants by hand\n"
            "        run: true\n")
        self.assert_code(
            self.run_gate_with_defaults(),
            2, "names cargo-mutants and will not lex as shell",
            "cannot tell")

    def test_the_unlexable_tally_is_reported_not_silent(self):
        # Not a verdict, and deliberately not silent. Every line is
        # lexed now; a tally that quietly grew from forty to four
        # hundred would mean the gate had stopped reading most of what
        # it was pointed at, and nothing else would say so.
        self.write("mise.toml", MISE_WITH_FLAG)
        self.write(".github/workflows/ci.yml", CI_WITH_FLAG)
        clean = self.run_gate_with_defaults()
        self.assert_code(clean, 0)
        self.write(".github/scripts/prose.sh", "#!/bin/bash\necho it's fine\n")
        noisier = self.run_gate_with_defaults()
        self.assert_code(noisier, 0)
        self.assertNotEqual(
            re.search(r"\((\d+) line", clean.stdout).group(1),
            re.search(r"\((\d+) line", noisier.stdout).group(1))

    def test_a_hash_inside_a_word_is_not_a_comment(self):
        # One comment rule, direction one. `#` starts a comment where it
        # starts a word; `out/run#3` is a path. Lexing with a second,
        # stricter rule truncated the command there and reported a
        # correct tree red — which the gate's own docstring twice calls
        # how a gate gets switched off in a week.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG,
                "jobs:\n  m:\n    steps:\n"
                "      - run: cargo mutants -p x --output out/run#3 "
                "--all-features\n",
            ),
            0, "2 cargo-mutants invocation(s) carry")

    def test_a_hash_inside_a_word_does_not_hide_the_invocation(self):
        # Direction two, and the worse one. The same disagreement cut
        # `./ci/wrap.sh --tag v1#2 cargo mutants -p x` off before
        # `cargo`, so the invocation was not recognised at all and the
        # gate printed OK about the files that were left.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG,
                "jobs:\n  m:\n    steps:\n"
                "      - run: ./ci/wrap.sh --tag v1#2 cargo mutants -p x "
                "--jobs 1\n",
            ),
            1, "missing from 1 of 2", "v1#2")

    def test_a_wrapper_assignment_does_not_vouch_for_the_invocation(self):
        # The case `invocations_in`'s docstring is written to justify and
        # which nothing asserted. Two properties hold it: everything in
        # front of `cargo` is dropped rather than searched, and a word
        # *contains* the flag without *being* it. `FLAGS=--all-features`
        # is a string the invocation never receives unless something
        # expands it, and a containment test over the words would take it
        # for the feature selection.
        self.assert_code(
            self.run_gate(
                MISE_WITH_FLAG.replace(
                    'cargo mutants -p "$usage_crate" --all-features',
                    'FLAGS=--all-features cargo mutants -p "$usage_crate"'),
                CI_WITH_FLAG,
            ),
            1, "missing from 1 of 2")

    def test_a_folded_scalar_says_the_remedy_is_the_backslashes(self):
        # The restriction stated rather than removed: joining is
        # backslash-only, so a YAML folded scalar reads here as several
        # commands and the one holding `cargo mutants` loses the
        # arguments under it. Parsing the workflow YAML would make this a
        # different tool; what the next reader needs instead is to be
        # told the fix is not a second copy of the flag.
        result = self.run_gate(
            MISE_WITH_FLAG,
            "jobs:\n  m:\n    steps:\n"
            "      - run: >-\n"
            "          cargo mutants -p x\n"
            "          --all-features\n"
            "          --jobs 1\n",
        )
        self.assert_code(result, 1, "missing from 1 of 2")
        self.assertIn("folded scalar", result.stderr)
        self.assertIn("put the backslashes back", result.stderr)

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

    def test_the_defaults_reach_a_composite_action(self):
        # A composite action runs `run:` steps exactly as a workflow
        # does, and this repository has one with eight of them. Globbing
        # `.github/workflows/` alone left it outside the gate: an
        # unflagged invocation there was executed and reported OK.
        self.write("mise.toml", MISE_WITH_FLAG)
        self.write(".github/workflows/ci.yml", CI_WITH_FLAG)
        self.write(
            ".github/actions/rust-checks/action.yml",
            "runs:\n  using: composite\n  steps:\n"
            "    - run: cargo mutants -p x --jobs 1\n")
        result = self.run_gate_with_defaults()
        self.assert_code(result, 1, "missing from 1 of 3")
        self.assertIn("action.yml", result.stderr)

    def test_the_defaults_reach_a_shell_script(self):
        # The other location the first repair left out. A job that calls
        # a script under `.github/scripts/` runs whatever is in it, and
        # what was in it was invisible.
        self.write("mise.toml", MISE_WITH_FLAG)
        self.write(".github/workflows/ci.yml", CI_WITH_FLAG)
        self.write(
            ".github/scripts/nightly.sh",
            "#!/bin/bash\ncargo mutants -p sunrise-core --jobs 1\n")
        result = self.run_gate_with_defaults()
        self.assert_code(result, 1, "missing from 1 of 3")
        self.assertIn("nightly.sh", result.stderr)

    def test_the_defaults_reach_a_yaml_beside_the_workflows(self):
        # `.github/` at any depth, not one directory of it. A matrix
        # moved to a file of its own outside `workflows/` is still
        # something that runs.
        self.write("mise.toml", MISE_WITH_FLAG)
        self.write(".github/workflows/ci.yml", CI_WITH_FLAG)
        self.write(
            ".github/mutants.yml",
            "jobs:\n  m:\n    steps:\n"
            "      - run: cargo mutants -p x --jobs 1\n")
        result = self.run_gate_with_defaults()
        self.assert_code(result, 1, "missing from 1 of 3")
        self.assertIn("mutants.yml", result.stderr)

    def test_the_defaults_with_no_workflow_directory_are_2(self):
        # The tree the union test cannot distinguish from a healthy one:
        # `.github/workflows/` gone altogether, `mise.toml`'s invocation
        # alone and perfectly flagged. Executed, and it printed `OK: 1`.
        # The count is the only thing that sees it.
        self.write("mise.toml", MISE_WITH_FLAG)
        result = self.run_gate_with_defaults()
        self.assert_code(result, 2, "found 1 `cargo mutants` invocation(s)")
        self.assertNotIn("OK:", result.stdout)

    def test_the_real_tree_is_in_step_and_holds_the_stated_count(self):
        # The one case that does not synthesise its files. Every other
        # case here names or writes its own, by design — but that left
        # `EXPECTED_INVOCATIONS` asserted against nothing except the
        # live `Mutation flag gate` job, which is not a required check,
        # so a number that had stopped describing this repository would
        # have had nothing blocking to say so. This runs the gate the
        # way CI runs it, from the repository root, with no arguments.
        root = GATE.parent.parent.parent
        result = subprocess.run(
            [sys.executable, str(GATE)],
            cwd=root, capture_output=True, text=True)
        self.assert_code(result, 0, "cargo-mutants invocation(s) carry")
        self.assertIn(f"OK: {EXPECTED_INVOCATIONS} ", result.stdout)

    # --- 2: the gate could not check anything ----------------------------

    def test_invocations_redistributed_at_a_constant_total_are_2(self):
        # M8, and what a scalar total cannot see. Delete the `ci.yml`
        # matrix invocation and add a second to `mise.toml`: the total
        # is still 2, every invocation carries the flag, and CI runs no
        # mutation testing at all. Executed at the previous head — exit
        # 0, `OK: 2` — on ONE edit, not two. The same move with the
        # second invocation put in a new workflow (round 3's M4, which
        # round 3 recorded as not closed) is the same news.
        self.write("mise.toml", MISE_WITH_FLAG + MISE_WITH_FLAG.replace(
            "[tasks.mutants]", "[tasks.mutants-again]"))
        self.write(".github/workflows/ci.yml",
                   "jobs:\n  build:\n    steps: []\n")
        self.assert_code(
            self.run_gate_with_defaults(),
            2, "in the wrong place", ".github/workflows/ci.yml",
            "the `mutants` matrix")

    def test_the_local_task_losing_its_invocation_is_2(self):
        # The mirror, and the reason this is stated per role rather than
        # as "ci.yml must hold one". A tree where the nightly measures
        # and nobody can reproduce it locally is the same divergence
        # from the other end.
        self.write("mise.toml", "[tasks.test]\nrun = \"cargo test\"\n")
        self.write(".github/workflows/ci.yml", CI_WITH_FLAG)
        self.write(
            ".github/workflows/nightly.yml",
            "jobs:\n  m:\n    steps:\n"
            "      - run: cargo mutants -p x --all-features --jobs 1\n")
        self.assert_code(
            self.run_gate_with_defaults(),
            2, "in the wrong place", "mise.toml",
            "the local `mutants` task")

    def test_a_role_holding_a_second_invocation_is_not_a_failure(self):
        # A floor per role, not an equality per role. `mise.toml`
        # acquiring a second invocation is a legitimate change; what it
        # needs is the total edited, which is a different message and a
        # different fix.
        self.write("mise.toml", MISE_WITH_FLAG + MISE_WITH_FLAG.replace(
            "[tasks.mutants]", "[tasks.mutants-again]"))
        self.write(".github/workflows/ci.yml", CI_WITH_FLAG)
        result = self.run_gate_with_defaults(expect=3)
        self.assert_code(result, 0, "3 cargo-mutants invocation(s) carry")

    def test_the_per_role_rule_does_not_apply_to_named_paths(self):
        # A caller who names paths is checking their own fixtures, not
        # this repository's layout — which is what every other case in
        # this file does, and why the rule is keyed on the default set.
        mise = self.write("elsewhere.toml", MISE_WITH_FLAG)
        self.assert_code(
            self.run_gate_on(mise, expect=1),
            0, "1 cargo-mutants invocation(s) carry")

    def test_a_missing_role_is_reported_after_a_missing_flag(self):
        # The same ordering argument the count already makes, one step
        # further down. A missing flag is the specific news; where the
        # invocations sit is the news that the gate no longer describes
        # the tree.
        self.write("mise.toml", MISE_WITH_FLAG.replace(
            " --all-features", "") + MISE_WITH_FLAG.replace(
            "[tasks.mutants]", "[tasks.mutants-again]"))
        self.write(".github/workflows/ci.yml",
                   "jobs:\n  build:\n    steps: []\n")
        self.assert_code(
            self.run_gate_with_defaults(), 1, "missing from 1 of 2")

    def test_a_file_with_no_invocation_beside_one_that_has_it_is_not_2(self):
        # Judged over the union, not per file. Moving the matrix out of
        # one workflow and into another leaves a tree that is entirely in
        # step; reporting that as a broken gate is how a gate gets
        # switched off. The invocation has to still be *somewhere* the
        # gate reads, which is what distinguishes this from the case
        # below — the earlier version of this test had an empty ci.yml
        # and no third file, and asserted exit 0 on a tree that had lost
        # half of what the gate compares.
        moved = self.write(
            "nightly.yml",
            "jobs:\n  m:\n    steps:\n"
            "      - run: cargo mutants -p x --all-features --jobs 1\n")
        mise = self.write("mise.toml", MISE_WITH_FLAG)
        empty = self.write("ci.yml", "jobs:\n  build:\n    steps: []\n")
        self.assert_code(
            self.run_gate_on(mise, empty, moved),
            0, "2 cargo-mutants invocation(s) carry")

    def test_the_invocation_count_falling_below_the_expected_is_2(self):
        # The hole the union leaves, and the reason a count is asserted
        # at all. One invocation of two has left the file set entirely —
        # moved to a file nothing globs, or deleted — and every file the
        # gate still reads looks perfect. The union is non-empty, so
        # nothing above this notices; the gate is comparing one thing
        # against nothing and would otherwise print OK.
        self.assert_code(
            self.run_gate(MISE_WITH_FLAG, "jobs:\n  build:\n    steps: []\n"),
            2, "found 1 `cargo mutants` invocation(s)",
            "expected exactly 2", "EXPECTED_INVOCATIONS")

    def test_the_invocation_count_rising_above_the_expected_is_2(self):
        # The direction a floor could not see, and the one a tree
        # actually moves in. A third invocation is added and nothing
        # needs editing, so the number stops describing the tree while
        # the gate stays green — after which deleting one of the
        # original two lands back on 2 and is still green. Executed at
        # the previous head: `OK: 3`, then `OK: 2` with the ci.yml
        # matrix invocation gone. The neighbour of the case above, one
        # invocation in the other direction.
        third = self.write(
            "release.yml",
            "jobs:\n  audit:\n    steps:\n"
            "      - run: cargo mutants -p x --all-features --jobs 1\n")
        mise = self.write("mise.toml", MISE_WITH_FLAG)
        ci = self.write("ci.yml", CI_WITH_FLAG)
        result = self.run_gate_on(mise, ci, third)
        self.assert_code(
            result, 2, "found 3 `cargo mutants` invocation(s)",
            "expected exactly 2", "EXPECTED_INVOCATIONS in this script to 3")
        self.assertNotIn("OK:", result.stdout)

    def test_a_count_that_is_wrong_does_not_hide_a_missing_flag(self):
        # Which of the two codes a tree with both gets. The flag is the
        # specific news; the count is the gate saying it no longer
        # describes the tree. Reporting the count first would hide a
        # corrupted floor behind a bookkeeping error, so the flag
        # verdict goes first and the count is raised on the next run.
        third = self.write(
            "release.yml",
            "jobs:\n  audit:\n    steps:\n"
            "      - run: cargo mutants -p x --jobs 1\n")
        mise = self.write("mise.toml", MISE_WITH_FLAG)
        ci = self.write("ci.yml", CI_WITH_FLAG)
        self.assert_code(
            self.run_gate_on(mise, ci, third), 1, "missing from 1 of 3")

    def test_the_expected_count_is_a_number_a_caller_can_state(self):
        # The escape hatch the failure text names. A tree that genuinely
        # holds one invocation is not a broken gate, and the way to say
        # so is to say the number — deliberately, in the change that
        # removes the invocation.
        self.assert_code(
            self.run_gate(MISE_WITH_FLAG, "jobs:\n  build:\n    steps: []\n",
                          expect=1),
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
