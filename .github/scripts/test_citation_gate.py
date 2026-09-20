#!/usr/bin/env python3
"""The exit-code contract of `citation-gate.py`, as assertions.

Why this file exists
--------------------

The gate's whole value is a *discrimination*: it has to fire on
`crates/sunrise-core/src/nonexistent.rs:1` and stay silent on `Vec<u8>`,
`cargo test --workspace`, `task.update` and the 600 bare filenames in this
repository that are shorthand rather than citations. Either half failing
makes it worthless, and the two fail in opposite directions from the same
edit — a rule loosened to catch one more real citation catches a hundred
code snippets, and one tightened to stop the snippets stops catching
anything.

The three anchors are where that tension actually lives, so
`TheCitingFilesOwnDirectory` pins both sides of the one that carries most
of this repository's citations: a `../` path has a single reading and a
dangling one must fail, while a bare name or a `./` path has two readings
and must be declined rather than guessed at.

So the near-misses below are not padding. Every one of them is a span that
actually appears in this tree, and each has its own assertion that the gate
classified it as *not a citation*. The `Discrimination` case would go red
for a rule that fired on everything, which is the mutation the `Failures`
case cannot see.

Fixtures are synthesised repositories
-------------------------------------

Every case builds a git repository in a temp directory, writes a handful of
files into it and drives the gate with `--root`. Nothing here reads this
repository's own tree: the `citations` job does that, and a contract test
that also did would go red for whatever document somebody edited this week
rather than for a change to the contract. `git init` plus `git add` is all
the setup needed — the gate reads the index through `git ls-files`, and the
fixtures are never committed.

`ALLOWED` is a literal in the gate, and every run checks it for staleness
against the tree it was pointed at, so a fixture repository that does not
contain this repository's eight excused citations fails on the stale check
before it can be asked anything else. Cases therefore run against a copy of
the gate with that literal rewritten, the way `test_file_size_gate.py`
rewrites `THRESHOLDS` and `test_orphan_crate_gate.py` rewrites `EXEMPT` —
the logic under test is still the shipped logic, read from the shipped file
at run time. The copy gets `docs-link-gate.py` beside it, because the gate
imports that sibling for the resolution anchor 2 uses.

Run it with `python3 .github/scripts/test_citation_gate.py`.
"""

from __future__ import annotations

import pathlib
import re
import subprocess
import sys
import tempfile
import unittest

GATE = pathlib.Path(__file__).resolve().parent / "citation-gate.py"

CLEAN = 0
DANGLING = 1
COULD_NOT_RUN = 2

ALLOWED_LITERAL = re.compile(
    r"^ALLOWED: dict\[tuple\[str, str\], str\] = \{.*?^\}$", re.DOTALL | re.MULTILINE
)

# The gate imports this one for `resolve_relative`, so a copy of the gate needs
# a copy of it beside the copy.
SIBLING = GATE.parent / "docs-link-gate.py"


def rewritten_gate(
    directory: pathlib.Path, allowed: dict[tuple[str, str], str] | None = None
) -> pathlib.Path:
    """The shipped gate with its `ALLOWED` literal replaced.

    Only the literal moves. Every rule the gate applies is the one on disk, so
    a change to `classify` is felt here even though the allowlist is not this
    repository's.
    """
    source = GATE.read_text(encoding="utf-8")
    body = "\n".join(f"    {key!r}: {reason!r}," for key, reason in (allowed or {}).items())
    replacement = (
        "ALLOWED: dict[tuple[str, str], str] = {" + (f"\n{body}\n" if body else "") + "}"
    )
    source, count = ALLOWED_LITERAL.subn(lambda _: replacement, source, count=1)
    if count != 1:
        raise AssertionError("the ALLOWED literal is no longer where this test expects it")
    (directory / SIBLING.name).write_text(SIBLING.read_text(encoding="utf-8"), encoding="utf-8")
    path = directory / "gate-under-test.py"
    path.write_text(source, encoding="utf-8")
    return path


class GateCase(unittest.TestCase):
    """One temp git repository per test, and one run of the gate over it."""

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

    def write(self, rel: str, text: str) -> pathlib.Path:
        """Write a file and stage it, which is what makes git track it."""
        path = self.repo / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        self._git("add", "--", rel)
        return path

    def run_gate(
        self, *args: str, allowed: dict[tuple[str, str], str] | None = None
    ) -> subprocess.CompletedProcess:
        gate = rewritten_gate(self.tmp, allowed)
        return subprocess.run(
            [sys.executable, str(gate), "--root", str(self.repo), *args],
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
    """The gate's own rules, asserted before it reads a tree."""

    def test_the_self_test_passes_alone(self):
        gate = rewritten_gate(self.tmp)
        result = subprocess.run(
            [sys.executable, str(gate), "--self-test"], capture_output=True, text=True
        )
        self.assertEqual(result.returncode, CLEAN, result.stdout + result.stderr)
        self.assertIn("OK: citations self-test clean", result.stdout)

    def test_the_self_test_runs_as_a_precondition_of_a_check(self):
        # Every check prints the self-test line first. A gate whose rules
        # have drifted must say so before it reports on a tree.
        self.write("docs/a.md", "Nothing here.\n")
        result = self.run_gate()
        self.assertIn("OK: citations self-test clean", result.stdout)

    def _mutated_gate(self, old: str, new: str) -> pathlib.Path:
        """A copy of the gate with one rule broken, for the two cases below.

        The mutation is asserted to have landed, so a refactor that moves the
        text it edits fails here rather than silently leaving the gate intact
        and the cases below passing for the wrong reason.
        """
        gate = rewritten_gate(self.tmp)
        source = gate.read_text(encoding="utf-8")
        self.assertIn(old, source, "the text this mutation edits has moved")
        gate.write_text(source.replace(old, new, 1), encoding="utf-8")
        return gate

    # `impl` back in `SYMBOL_DECL`'s alternation. It is the reversal of a
    # decision this change took, and exactly one self-test case asserts it, so
    # the failure below is attributable rather than a wall of them.
    IMPL_BACK = (
        r'r"(?:fn|struct|enum|trait|type|static|union)[ \t]+"',
        r'r"(?:fn|struct|enum|trait|type|static|union|impl)[ \t]+"',
    )

    def test_a_broken_rule_makes_the_self_test_exit_one(self):
        # **The guard that gates every other rule, guarded.** Nothing else
        # proves `self_test` can return anything but 0: the two cases above
        # assert only the passing string, so a harness that had stopped
        # detecting anything -- `wrong()` never firing, its argument inverted,
        # the failure arm unreachable -- would look exactly the same from
        # here, and every rule in this file would be resting on it.
        #
        # So: break one rule the self-test asserts, and require the harness to
        # say so, with exit 1 and the message that case wrote.
        gate = self._mutated_gate(*self.IMPL_BACK)
        result = subprocess.run(
            [sys.executable, str(gate), "--self-test"], capture_output=True, text=True
        )
        output = result.stdout + result.stderr
        self.assertEqual(result.returncode, DANGLING, output)
        self.assertIn("::error::citations self-test:", output)
        self.assertIn("expected `impl` not to resolve", output)
        self.assertNotIn("OK: citations self-test clean", output)

    def test_a_failed_self_test_stops_the_gate_before_it_reads_a_tree(self):
        # The self-test is a PRECONDITION, not a report. A gate whose rules
        # have drifted must not go on to pronounce on a repository with them,
        # because every verdict it printed would be computed by the rules it
        # just failed to uphold.
        self.write("docs/a.md", "See `crates/c/src/lib.rs`.\n")
        gate = self._mutated_gate(*self.IMPL_BACK)
        result = subprocess.run(
            [sys.executable, str(gate), "--root", str(self.repo)],
            capture_output=True,
            text=True,
            cwd=self.tmp,
        )
        output = result.stdout + result.stderr
        self.assertEqual(result.returncode, DANGLING, output)
        self.assertNotIn("anchored citation(s)", output)

    def test_the_self_test_counts_every_case_it_ran(self):
        # `wrong()` counts the cases, so the number is a measurement rather
        # than a literal somebody has to remember to edit -- but it is a
        # PRINT, and a print constrains nothing. Half the self-test could be
        # deleted and the run would still say "OK" and exit 0.
        #
        # A floor, then, in the shape this file already uses for the
        # allowlist: 100 is not a count anybody derived, it is a tripwire, and
        # tripping it should be a conversation rather than a commit.
        #
        # **What the number does and does not promise.** The self-test runs
        # 119 cases today, so the floor carries 19 cases of slack and a
        # deletion smaller than that is silent here. That is the price of a
        # tripwire rather than a derived count: a number recomputed at every
        # legitimate retirement is a number that gets relaxed on the commit
        # that retires one, and the failure this exists to catch is the large
        # one -- half the self-test cut, the gate still printing "OK:
        # citations self-test clean (56 cases)" and still exiting 0. Raise
        # the floor when the slack stops being worth it; do not derive it.
        gate = rewritten_gate(self.tmp)
        result = subprocess.run(
            [sys.executable, str(gate), "--self-test"], capture_output=True, text=True
        )
        self.assertEqual(result.returncode, CLEAN, result.stdout + result.stderr)
        found = re.search(r"self-test clean \((\d+) cases\)", result.stdout)
        self.assertIsNotNone(found, f"the self-test's count line moved:\n{result.stdout}")
        self.assertGreaterEqual(
            int(found.group(1)), 100, "the self-test lost cases; restore them or say why"
        )

    def test_this_suite_keeps_its_own_cases(self):
        # The floor above guards the self-test's count. Nothing guarded this
        # file's, and the asymmetry is the gap: a rule deleted from
        # `self_test` trips a tripwire, while the contract case that pins the
        # same rule can be deleted with the suite still printing OK. Both
        # directions of that silence matter, because most of what this change
        # added lives here rather than there.
        #
        # Counted from the source rather than from a run, so it constrains
        # the file on disk and not whatever a filtered invocation happened to
        # execute. Same idiom and same caveat as the two tripwires beside it:
        # the slack is deliberate, and raising the floor is a
        # decision rather than bookkeeping. 100 against 109 today.
        source = pathlib.Path(__file__).read_text(encoding="utf-8")
        self.assertGreaterEqual(
            len(re.findall(r"^    def test_", source, re.MULTILINE)),
            100,
            "this suite lost cases; restore them or say why",
        )

    def test_the_shipped_allowlist_stays_small(self):
        # The list is the gate's one escape hatch and the thing most likely to
        # grow instead of the docs getting fixed. Twelve is not a limit anybody
        # derived; it is a tripwire, and passing it should be a conversation
        # rather than a commit.
        found = ALLOWED_LITERAL.search(GATE.read_text(encoding="utf-8"))
        self.assertIsNotNone(found, "the ALLOWED literal moved")
        self.assertLessEqual(
            found.group(0).count("    (\n"), 12, "the allowlist has grown; fix the docs instead"
        )


class Clean(GateCase):
    """What a passing run says, and that it says a number."""

    def test_a_tree_of_resolving_citations_exits_zero(self):
        self.write("README.md", "Root file.\n")
        self.write("docs/a.md", "See `README.md` and `docs/b.md:2`.\n")
        self.write("docs/b.md", "one\ntwo\nthree\n")
        result = self.run_gate()
        self.assert_code(result, CLEAN, "OK: citations clean.", "2 anchored citation(s)")

    def test_the_last_line_of_a_file_is_in_bounds(self):
        # An off-by-one in `line_count` is invisible from either side unless
        # the boundary itself is pinned, and it is the cheapest bug to write.
        self.write("docs/three.md", "one\ntwo\nthree\n")
        self.write("docs/a.md", "See `docs/three.md:3`.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")

    def test_a_file_with_no_trailing_newline_still_counts_its_last_line(self):
        self.write("docs/three.md", "one\ntwo\nthree")
        self.write("docs/a.md", "See `docs/three.md:3`.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")

    def test_a_directory_citation_with_an_admitted_extension_resolves(self):
        # Rare, and the branch exists so such a name reports the right thing
        # rather than "no such file": nothing in this repository is a directory
        # ending in an admitted extension today, which is exactly why the case
        # is synthesised rather than borrowed.
        self.write("schemas/bundle.json/part.json", "{}\n")
        self.write("docs/a.md", "See `schemas/bundle.json`.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")


class Failures(GateCase):
    """Each distinct way a citation can be broken, and its message."""

    def test_a_path_that_names_nothing_fails(self):
        self.write("docs/a.md", "See `crates/sunrise-core/src/nonexistent.rs:1`.\n")
        self.write("crates/sunrise-core/src/lib.rs", "fn main() {}\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "::error file=docs/a.md,line=1::citations: "
            "`crates/sunrise-core/src/nonexistent.rs:1` names no file git tracks.",
            "1 citation(s) resolve to nothing",
        )

    def test_a_line_past_the_end_of_the_file_fails(self):
        self.write("docs/short.md", "one\ntwo\n")
        self.write("docs/a.md", "See `docs/short.md:99999`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "cites line 99999, but `docs/short.md` has 2 line(s).",
        )

    def test_a_range_past_the_end_of_the_file_fails(self):
        self.write("docs/short.md", "one\ntwo\n")
        self.write("docs/a.md", "See `docs/short.md:1-9`.\n")
        self.assert_code(self.run_gate(), DANGLING, "cites lines 1-9, but `docs/short.md` has 2 line(s).")

    def test_an_empty_range_fails_even_when_the_file_is_long_enough(self):
        # A backwards range is a typo that no existence check can catch:
        # both endpoints are in bounds.
        self.write("docs/long.md", "x\n" * 100)
        self.write("docs/a.md", "See `docs/long.md:50-40`.\n")
        self.assert_code(self.run_gate(), DANGLING, "cites an empty range (50-40).")

    def test_line_zero_fails(self):
        self.write("docs/long.md", "x\n" * 10)
        self.write("docs/a.md", "See `docs/long.md:0`.\n")
        self.assert_code(self.run_gate(), DANGLING, "cites line 0; line numbers start at 1.")

    def test_a_line_cited_on_a_directory_fails(self):
        self.write("schemas/bundle.json/part.json", "{}\n")
        self.write("docs/a.md", "See `schemas/bundle.json:12`.\n")
        self.assert_code(
            self.run_gate(), DANGLING, "cites a line, but `schemas/bundle.json` is a directory."
        )

    def test_a_file_that_exists_on_disk_but_is_untracked_fails(self):
        # The citation is dead for every reader on github.com, which is what
        # "exists" has to mean. Written without staging it.
        (self.repo / "docs").mkdir(parents=True, exist_ok=True)
        (self.repo / "docs" / "untracked.md").write_text("here\n", encoding="utf-8")
        self.write("docs/a.md", "See `docs/untracked.md`.\n")
        self.assert_code(self.run_gate(), DANGLING, "`docs/untracked.md` names no file git tracks.")

    def test_every_failure_is_reported_not_only_the_first(self):
        self.write(
            "docs/a.md",
            "First `docs/gone-one.md`.\n\nSecond `docs/gone-two.md`.\n",
        )
        self.write("docs/b.md", "Third `docs/gone-three.md`.\n")
        result = self.run_gate()
        self.assert_code(
            result,
            DANGLING,
            "docs/a.md,line=1",
            "docs/a.md,line=3",
            "docs/b.md,line=1",
            "3 citation(s) resolve to nothing",
        )


class Discrimination(GateCase):
    """The near-misses. A gate that fires on these is worse than none.

    Every span here appears in this repository's prose today.
    """

    def near_miss(self, span: str) -> None:
        self.write("docs/a.md", f"Prose about {span} and nothing else.\n")
        result = self.run_gate()
        self.assert_code(result, CLEAN, "OK: citations clean.")

    def test_a_rust_type_is_not_a_citation(self):
        self.near_miss("`Vec<u8>`")

    def test_a_shell_command_is_not_a_citation(self):
        self.near_miss("`cargo test --workspace`")

    def test_a_flag_is_not_a_citation(self):
        self.near_miss("`--all-features`")

    def test_a_rust_path_expression_is_not_a_citation(self):
        self.near_miss("`sunrise_core::engine`")

    def test_a_version_number_is_not_a_citation(self):
        # `0.1.0` is a path shape under a naive rule: `0.1` plus a `.0`
        # extension. The extension has to start with a letter.
        self.near_miss("`0.1.0`")

    def test_an_attribute_is_not_a_citation(self):
        self.near_miss("`#[derive(Debug)]`")

    def test_a_span_carrying_more_than_a_path_is_not_a_citation(self):
        # The path resolves to nothing, so a rule that read a path out of the
        # middle of a span would fail here.
        self.near_miss("`see crates/gone.rs`")

    def test_a_prose_filename_with_no_directory_is_not_checked(self):
        # The single most common shape in this tree, and the one that cannot
        # be resolved without guessing which directory is meant.
        self.near_miss("the parser in `qr.rs`, whose predecessor `pair_qr_v0.rs` is gone")

    def test_a_partial_path_relative_to_nothing_is_not_checked(self):
        # A fragment of a path the surrounding paragraph established. It
        # resolves against neither the root nor the citing file's directory,
        # and which directory was meant is not a thing a tool can decide.
        self.near_miss("`Views/TaskEditorView.swift:97` and `api/observe.rs`")

    def test_an_op_kind_is_not_a_citation(self):
        # The closed extension set exists for these: a shape rule reads
        # `task.update` as a path with a `.update` extension, and this tree
        # writes hundreds of them.
        self.near_miss("`task.update`, `Task.blocks` and `focus.end`")

    def test_a_dotted_runtime_path_is_not_a_repository_citation(self):
        # `./sunrise.toml` is what the server looks for in its working
        # directory, written beside `/etc/sunrise/sunrise.toml`. `./` is
        # therefore claimed only when it resolves, unlike `../`.
        self.near_miss("`./sunrise.toml`, after `$SUNRISE_CONFIG`")

    def test_declined_spans_are_counted_and_split_rather_than_dropped(self):
        # Declining to check something is only honest if the run says how much
        # it declined, and says which kind: a bare name and a partial path are
        # declined for the same reason but size the hole differently.
        self.write("docs/a.md", "See `recovery.md` and `api/observe.rs`.\n")
        result = self.run_gate("--list-unanchored")
        self.assert_code(
            result,
            CLEAN,
            "2 path-like span(s) were NOT checked — 1 bare filename(s) and 1 partial path(s)",
            "docs/a.md:1: `recovery.md`",
            "docs/a.md:1: `api/observe.rs`",
        )


class Masking(GateCase):
    """Where a backtick is not a code span."""

    def test_a_fenced_code_block_is_not_scanned(self):
        self.write(
            "docs/a.md",
            "Prose.\n\n```text\nA sample citing `docs/never-existed.md`\n```\n\nMore prose.\n",
        )
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")

    def test_a_tilde_fence_closes_only_with_tildes(self):
        self.write(
            "docs/a.md",
            "~~~\n```\n`docs/never-existed.md`\n```\n~~~\n",
        )
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")

    def test_yaml_front_matter_is_not_scanned(self):
        self.write("docs/a.md", "---\nsee: `docs/never-existed.md`\n---\n\nProse.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")

    def test_an_unclosed_opening_rule_is_not_front_matter(self):
        # A document that opens with a thematic break must not lose its body.
        # Getting this wrong silently drops every citation in the file, which
        # is a green run that checked nothing.
        self.write("docs/a.md", "---\n\nProse citing `docs/never-existed.md`.\n")
        self.assert_code(self.run_gate(), DANGLING, "`docs/never-existed.md` names no file")

    def test_a_citation_in_a_heading_or_a_block_quote_is_still_checked(self):
        self.write("docs/a.md", "# See `docs/gone-one.md`\n\n> And `docs/gone-two.md`.\n")
        self.assert_code(self.run_gate(), DANGLING, "gone-one.md", "gone-two.md", "2 citation(s)")


class RustDocComments(GateCase):
    """Only doc comment prose is read out of a `.rs` file."""

    def setUp(self) -> None:
        super().setUp()
        # A `docs/` at the root, so a `docs/...` span is claimed by an anchor
        # and the cases below are about the masking rather than about the
        # anchor rule.
        self.write("docs/index.md", "An index.\n")

    def test_a_doc_comment_citation_is_checked(self):
        self.write("crates/c/src/lib.rs", "//! See `docs/never-existed.md`.\npub fn f() {}\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "::error file=crates/c/src/lib.rs,line=1::citations: "
            "`docs/never-existed.md` names no file git tracks.",
        )

    def test_an_ordinary_comment_is_not_a_doc_comment(self):
        self.write("crates/c/src/lib.rs", "// See `docs/never-existed.md`.\npub fn f() {}\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")

    def test_four_slashes_is_not_a_doc_comment(self):
        self.write("crates/c/src/lib.rs", "//// See `docs/never-existed.md`.\npub fn f() {}\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")

    def test_a_string_literal_is_not_prose(self):
        self.write("crates/c/src/lib.rs", 'pub const P: &str = "`docs/never-existed.md`";\n')
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")

    def test_a_fenced_example_inside_a_doc_comment_is_not_scanned(self):
        self.write(
            "crates/c/src/lib.rs",
            "//! Prose.\n//!\n//! ```rust\n//! // `docs/never-existed.md`\n//! let x = 1;\n//! ```\npub fn f() {}\n",
        )
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")

    def test_an_unclosed_fence_does_not_swallow_the_next_block(self):
        # Fence state belongs to one run of one marker kind. Without that, an
        # unbalanced fence in one comment silences every comment after it.
        self.write(
            "crates/c/src/lib.rs",
            "//! ```\n//! unterminated\npub fn f() {}\n\n/// See `docs/never-existed.md`.\npub fn g() {}\n",
        )
        self.assert_code(self.run_gate(), DANGLING, "crates/c/src/lib.rs,line=5")


class Anchors(GateCase):
    """Where a citation is resolved from, and where it is not."""

    def setUp(self) -> None:
        super().setUp()
        # A repository with both a top-level `tests/` and a crate that has
        # one, which is the collision the crate anchor exists for.
        self.write("tests/chaos/README.md", "A chaos harness.\n")
        self.write("crates/sunrise-cli/src/main.rs", "fn main() {}\n")
        self.write("crates/sunrise-cli/tests/cli.rs", "#[test]\nfn t() {}\n")

    def test_a_crate_relative_path_resolves_from_inside_its_crate(self):
        # Cargo fixes the layout, so `tests/cli.rs` written here means
        # `crates/sunrise-cli/tests/cli.rs` and can mean nothing else.
        self.write("crates/sunrise-cli/src/livesync.rs", "//! Driven by `tests/cli.rs`.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")

    def test_the_same_path_from_a_document_has_no_crate_to_anchor_to(self):
        self.write("docs/a.md", "Driven by `tests/cli.rs`.\n")
        self.assert_code(self.run_gate(), DANGLING, "`tests/cli.rs` names no file git tracks.")

    def test_the_crate_anchor_does_not_reach_another_crate(self):
        self.write("crates/other/src/lib.rs", "//! See `tests/cli.rs`.\n")
        self.assert_code(self.run_gate(), DANGLING, "crates/other/src/lib.rs")

    def test_a_first_segment_naming_nothing_at_any_anchor_is_declined(self):
        # `src/main.rs` is claimed inside a crate and claimed by nothing from a
        # document, so it is counted rather than reported.
        self.write("docs/a.md", "See `src/main.rs`.\n")
        self.assert_code(self.run_gate(), CLEAN, "path-like span(s) were NOT checked")

    def test_legacy_is_not_scanned(self):
        # The pre-rewrite tree cites paths that resolve against this
        # repository and mean something else there.
        self.write("legacy/README.md", "See `docs/never-existed.md`.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")

    def test_a_path_into_legacy_is_still_checked_from_a_scanned_file(self):
        # Not scanning `legacy/` is about what it *says*, not about whether
        # the rest of the tree may point into it.
        self.write("legacy/README.md", "The pre-rewrite tree.\n")
        self.write("docs/a.md", "See `legacy/gone.md`.\n")
        self.assert_code(self.run_gate(), DANGLING, "`legacy/gone.md` names no file git tracks.")


class TheCitingFilesOwnDirectory(GateCase):
    """Anchor 2, which is how most of this repository actually cites.

    Split down the middle on purpose: a `../` path has one reading and is
    held to it, while a bare name or a `./` path has two and is only
    checked where the reading is unambiguous.
    """

    def setUp(self) -> None:
        super().setUp()
        self.write("docs/03-crypto/recovery.md", "# Recovery\n")
        self.write("docs/03-crypto/key-rotation.md", "one\ntwo\nthree\n")
        self.write("docs/06-server/api.md", "# API\n")

    def test_a_climbing_path_that_resolves_is_checked(self):
        self.write("docs/05-sync/transports.md", "See `../06-server/api.md`.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.", "1 anchored citation(s)")

    def test_a_climbing_path_that_resolves_to_nothing_fails(self):
        # The case this anchor exists for. Before it, the span was declined in
        # silence and the reader followed a dead pointer.
        self.write("docs/05-sync/transports.md", "See `../03-crypto/does-not-exist.md`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "::error file=docs/05-sync/transports.md,line=1::citations: "
            "`../03-crypto/does-not-exist.md` names no file git tracks.",
        )

    def test_a_climbing_path_is_checked_from_a_rust_doc_comment_too(self):
        self.write("crates/c/src/lib.rs", "//! See `../../../docs/03-crypto/gone.md`.\n")
        self.assert_code(self.run_gate(), DANGLING, "crates/c/src/lib.rs,line=1")

    def test_a_line_number_on_a_climbing_path_is_checked(self):
        self.write("docs/05-sync/a.md", "See `../03-crypto/key-rotation.md:99`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "cites line 99, but `docs/03-crypto/key-rotation.md` has 3 line(s).",
        )

    def test_a_path_that_climbs_out_of_the_repository_fails(self):
        self.write("docs/a.md", "See `../../elsewhere.toml`.\n")
        self.assert_code(self.run_gate(), DANGLING, "climbs out of the repository.")

    def test_a_bare_sibling_that_resolves_is_checked(self):
        # Asserting the *count*, not just the verdict: if this anchor stopped
        # resolving, the span would be declined and the run would still be
        # clean, so "exit 0" alone proves nothing here.
        self.write("docs/03-crypto/primitives.md", "See `key-rotation.md:3`.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.", "1 anchored citation(s)")

    def test_a_bare_sibling_with_a_line_past_the_end_fails(self):
        # The proof that the sibling reading is *checked* rather than merely
        # counted: `key-rotation.md` has three lines.
        self.write("docs/03-crypto/primitives.md", "See `key-rotation.md:99`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "cites line 99, but `docs/03-crypto/key-rotation.md` has 3 line(s).",
        )

    def test_a_bare_name_that_is_not_a_sibling_is_declined_not_failed(self):
        # `recovery.md` in `docs/06-server/` is shorthand for the one in
        # `docs/03-crypto/`. Claiming it would fail a correct document, which
        # is the one thing a gate may not do.
        self.write("docs/06-server/auth.md", "See `recovery.md`.\n")
        self.assert_code(self.run_gate(), CLEAN, "1 bare filename(s)")

    def test_a_dotted_path_that_resolves_is_checked(self):
        self.write("docs/03-crypto/primitives.md", "See `./key-rotation.md:3`.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")

    def test_a_dotted_path_that_does_not_resolve_is_declined_not_failed(self):
        # `./sunrise.toml` names the server's working directory, not this tree.
        self.write("docs/06-server/self-hosting.md", "Then `./sunrise.toml`.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.", "1 bare filename(s)")

    def test_the_resolution_is_the_docs_link_gates_own(self):
        # Imported rather than reimplemented, because that gate resolves
        # `[a](../x.md)` by the same convention and two implementations of
        # "where does that point" can disagree about the same tree.
        gate = GATE.read_text(encoding="utf-8")
        self.assertIn('_sibling("docs_link_gate", "docs-link-gate.py").resolve_relative', gate)
        self.assertIn("def resolve_relative(", SIBLING.read_text(encoding="utf-8"))


class Extensions(GateCase):
    """Rule 2: the closed set, and what it keeps out."""

    def test_an_extension_outside_the_set_is_prose(self):
        # `.update` is an op kind. Nothing in this repository is a file of
        # that type, so a span ending in one is not a path.
        self.write("docs/a.md", "The `task.update` op and the `Task.blocks` field.\n")
        result = self.run_gate()
        self.assert_code(result, CLEAN, "0 anchored citation(s)")
        self.assertNotIn("path-like span(s) were NOT checked", result.stdout)

    def test_every_admitted_extension_is_lower_case_and_alphabetic(self):
        # The membership test lower-cases what it looks up, so an upper-case
        # entry in the literal would be unreachable and silently drop a type.
        import re as _re

        found = _re.search(r'EXTENSIONS = frozenset\(\n?\s*"([^"]+)"', GATE.read_text(encoding="utf-8"))
        self.assertIsNotNone(found, "the EXTENSIONS literal moved")
        for extension in found.group(1).split():
            self.assertTrue(extension.islower() and extension.isalnum(), extension)


class Symbols(GateCase):
    """The optional `#symbol` suffix, both of its verdicts, and its silence.

    The suffix exists because the check it sits beside cannot see the failure
    that actually happens: a citation does not usually rot into a line that is
    gone, it rots into a line that is still there and now says something else.
    ADR-0034 is the measured case — eight of its nine `path:line` citations
    landed on unrelated code inside about one release cycle and every one of
    them was green here.

    So the two halves below are both load-bearing, and they pull in opposite
    directions exactly like the anchors do. A suffix that never fires buys
    nothing; a suffix that fires on a citation which is *fine* turns a
    widening into a breaking change, and the 1566 citations already in this
    repository carry no suffix at all and must go on meaning what they meant.
    `test_a_citation_with_no_suffix_is_unchanged` is the second half, and it
    is the one to read first if this class ever goes red.
    """

    RUST = (
        "/// Doc above the item.\n"           # 1
        "#[allow(dead_code)]\n"               # 2
        "pub(super) fn wanted(\n"             # 3
        "    x: u8,\n"                        # 4
        ") -> u8 {\n"                         # 5
        "    x\n"                             # 6
        "}\n"                                 # 7
        "\n"                                  # 8
        "fn elsewhere() -> u8 {\n"            # 9
        "    0\n"                             # 10
        "}\n"                                 # 11
    )

    def rust(self) -> None:
        self.write("crates/c/src/lib.rs", self.RUST)

    def test_a_symbol_that_contains_the_cited_line_passes(self):
        self.rust()
        self.write("docs/a.md", "See `crates/c/src/lib.rs:3#wanted`.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.", "1 anchored citation(s)")

    def test_a_citation_of_the_symbols_own_doc_comment_is_inside_it(self):
        # This repository keeps its reasoning in doc comments, so most
        # citations worth making point at one. A span that stopped at the
        # `fn` line would fail every one of them.
        self.rust()
        self.write("docs/a.md", "See `crates/c/src/lib.rs:1#wanted`.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")

    def test_a_doc_comment_above_a_split_attribute_is_inside_the_item(self):
        # rustfmt splits an attribute too wide for the line limit, and its
        # last line is `)]` -- neither a `///` nor a `#[`. A start walk that
        # stopped there put the item's whole doc comment outside its own span
        # and reported a correct citation of that doc as broken, on four real
        # files in this repository. Both edges are asserted: the doc is in,
        # and the blank line above it is still out.
        self.write(
            "crates/c/src/lib.rs",
            "pub const OTHER: u8 = 0;\n"                # 1
            "\n"                                        # 2
            "/// Doc above the item.\n"                  # 3
            "#[derive(\n"                               # 4
            "    Debug, Clone, Copy, PartialEq, Eq,\n"   # 5
            ")]\n"                                      # 6
            "pub struct Wanted {\n"                     # 7
            "    pub field: u8,\n"                      # 8
            "}\n",                                      # 9
        )
        self.write("docs/a.md", "See `crates/c/src/lib.rs:3#Wanted`.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")

        self.write("docs/b.md", "See `crates/c/src/lib.rs:2#Wanted`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "cites line 2, but `Wanted` in `crates/c/src/lib.rs` spans 3-9.",
        )

    def test_an_array_close_above_an_item_is_not_read_as_an_attribute(self):
        # The guard on the case above. `];` closes a `static` initialiser and
        # is not an attribute, so the walk must leave the span where it is
        # rather than climbing into the item above -- a span that swallowed
        # its neighbour would pass a citation that belongs to neither.
        self.write(
            "crates/c/src/lib.rs",
            "pub static LIST: [u8; 2] = [\n"   # 1
            "    1, 2,\n"                      # 2
            "];\n"                             # 3
            "pub fn wanted() -> u8 {\n"        # 4
            "    0\n"                          # 5
            "}\n",                             # 6
        )
        self.write("docs/a.md", "See `crates/c/src/lib.rs:4#wanted`.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")

        self.write("docs/b.md", "See `crates/c/src/lib.rs:1#wanted`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "cites line 1, but `wanted` in `crates/c/src/lib.rs` spans 4-6.",
        )

    def test_a_blank_line_above_a_bare_close_stops_the_attribute_climb(self):
        # `attribute_opener`'s first inner bail, reached. The case above bails
        # at the *outer* guard -- its `];` does not end in `]` -- so it never
        # enters the loop at all and the inner bails were pinned by nothing.
        #
        # Every fixture in this group has the same shape: a bare `]` directly
        # above the item, and a real `#[` two lines up with the bail's trigger
        # between them. That is what makes the bail load-bearing rather than
        # incidental. Delete it and the climb reaches the `#[`, the span opens
        # at line 1, and the assertion below goes green when it must not: the
        # span would have swallowed an attribute belonging to something else.
        self.write(
            "crates/c/src/lib.rs",
            "#[allow(dead_code)]\n"        # 1
            "\n"                           # 2  the bail
            "]\n"                          # 3
            "pub fn wanted() -> u8 {\n"    # 4
            "    0\n"                      # 5
            "}\n",                         # 6
        )
        self.write("docs/a.md", "See `crates/c/src/lib.rs:4#wanted`.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")

        self.write("docs/b.md", "See `crates/c/src/lib.rs:1#wanted`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "cites line 1, but `wanted` in `crates/c/src/lib.rs` spans 4-6.",
        )

    def test_a_comment_above_a_bare_close_stops_the_attribute_climb(self):
        # The second inner bail. rustfmt writes no `//` inside an attribute,
        # so one here means the `]` below closes something else.
        self.write(
            "crates/c/src/lib.rs",
            "#[allow(dead_code)]\n"        # 1
            "// an ordinary comment\n"     # 2  the bail
            "]\n"                          # 3
            "pub fn wanted() -> u8 {\n"    # 4
            "    0\n"                      # 5
            "}\n",                         # 6
        )
        self.write("docs/a.md", "See `crates/c/src/lib.rs:1#wanted`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "cites line 1, but `wanted` in `crates/c/src/lib.rs` spans 4-6.",
        )

    def test_a_line_ending_in_a_brace_or_semicolon_stops_the_attribute_climb(self):
        # The third inner bail, all three of its terminators. `{`, `}` and `;`
        # each end a construct rustfmt would never leave open inside an
        # attribute, so each one means the climb has left the attribute and is
        # walking into the item above.
        for terminator, middle in (
            ("{", "fn above() -> u8 {"),
            ("}", "}"),
            (";", "const ABOVE: u8 = 0;"),
        ):
            with self.subTest(terminator=terminator):
                self.write(
                    "crates/c/src/lib.rs",
                    "#[allow(dead_code)]\n"        # 1
                    f"{middle}\n"                  # 2  the bail
                    "]\n"                          # 3
                    "pub fn wanted() -> u8 {\n"    # 4
                    "    0\n"                      # 5
                    "}\n",                         # 6
                )
                self.write("docs/a.md", "See `crates/c/src/lib.rs:1#wanted`.\n")
                self.assert_code(
                    self.run_gate(),
                    DANGLING,
                    "cites line 1, but `wanted` in `crates/c/src/lib.rs` spans 4-6.",
                )

    def test_a_bare_close_at_the_top_of_the_file_stops_the_attribute_climb(self):
        # The loop running off the top of the file, which is the one exit
        # `attribute_opener` takes without deciding anything. There is no `#[`
        # to find, so the span must stay exactly where the walk left it.
        self.write(
            "crates/c/src/lib.rs",
            "]\n"                          # 1
            "pub fn wanted() -> u8 {\n"    # 2
            "    0\n"                      # 3
            "}\n",                         # 4
        )
        self.write("docs/a.md", "See `crates/c/src/lib.rs:2#wanted`.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")

        self.write("docs/b.md", "See `crates/c/src/lib.rs:1#wanted`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "cites line 1, but `wanted` in `crates/c/src/lib.rs` spans 2-4.",
        )

    def test_a_symbol_that_does_not_contain_the_cited_line_fails(self):
        self.rust()
        self.write("docs/a.md", "See `crates/c/src/lib.rs:10#wanted`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "::error file=docs/a.md,line=1",
            "cites line 10, but `wanted` in `crates/c/src/lib.rs` spans 1-7.",
        )

    def test_a_range_that_leaves_the_symbol_fails(self):
        # Half in is out. A range is one citation and the sentence is about
        # all of it.
        self.rust()
        self.write("docs/a.md", "See `crates/c/src/lib.rs:6-10#wanted`.\n")
        self.assert_code(self.run_gate(), DANGLING, "cites lines 6-10, but `wanted`")

    def test_a_symbol_the_file_does_not_declare_fails(self):
        self.rust()
        self.write("docs/a.md", "See `crates/c/src/lib.rs:3#renamed`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "names `renamed`, which `crates/c/src/lib.rs` does not declare.",
        )

    def test_a_line_less_citation_checks_only_that_the_symbol_is_there(self):
        # The form for citing into a file somebody else is rewriting: it
        # cannot rot into a wrong line because it names none, and it still
        # fails when the item is renamed away.
        self.rust()
        self.write("docs/a.md", "See `crates/c/src/lib.rs#wanted`.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.", "1 anchored citation(s)")

    def test_a_line_less_citation_of_a_missing_symbol_fails(self):
        self.rust()
        self.write("docs/a.md", "See `crates/c/src/lib.rs#renamed`.\n")
        self.assert_code(self.run_gate(), DANGLING, "does not declare")

    def test_a_name_declared_twice_is_one_target(self):
        # A trait method and its impl are both `twice`. Taking the first
        # declaration would fail a correct citation of the second, and a gate
        # that fails a correct document is worse than one that passes a wrong
        # one -- which is why `symbol_span` returns a list.
        self.write(
            "crates/c/src/lib.rs",
            "trait T {\n"                    # 1
            "    fn twice(&self) -> u8;\n"   # 2
            "}\n"                            # 3
            "\n"                             # 4
            "impl T for u8 {\n"              # 5
            "    fn twice(&self) -> u8 {\n"  # 6
            "        2\n"                    # 7
            "    }\n"                        # 8
            "}\n",                           # 9
        )
        self.write("docs/a.md", "See `crates/c/src/lib.rs:2#twice` and `crates/c/src/lib.rs:7#twice`.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.", "2 anchored citation(s)")

    def test_a_line_in_neither_declaration_names_both_spans(self):
        self.write(
            "crates/c/src/lib.rs",
            "trait T {\n    fn twice(&self) -> u8;\n}\n\nimpl T for u8 {\n"
            "    fn twice(&self) -> u8 {\n        2\n    }\n}\n",
        )
        self.write("docs/a.md", "See `crates/c/src/lib.rs:4#twice`.\n")
        self.assert_code(self.run_gate(), DANGLING, "spans 2-2, 6-8.")

    # ---- the start walk's remaining shapes, recorded as they answer today ----
    #
    # The four cases below are characterisation, not specification. Each one
    # is a shape `symbol_span`'s line-based start walk gets wrong, none is
    # reached by a citation in this repository today, and each says which
    # direction it errs in. They are here because the walk shipped pinned for
    # exactly one attribute shape, and the shape it was not pinned for was a
    # live defect on four files. A shape nobody wrote a case for is a shape
    # nobody measured.
    #
    # A fifth case used to sit here, for `impl<'a> Foo<'a>`, on the reading
    # that the generic parameters were what `SYMBOL_DECL` could not match. It
    # is not characterisation any more and it is not about the start walk:
    # `impl` left the alternation deliberately, so `impl Foo` and
    # `impl<'a> Foo<'a>` are now equally and intentionally unresolvable. The
    # verdict is asserted by `test_an_impl_block_is_not_a_citation_target`,
    # where the reason is stated correctly.

    def test_a_blank_line_between_the_doc_and_the_item_cuts_the_doc_out(self):
        # rustc attaches this doc comment to `wanted` across the blank line;
        # the walk does not, so a citation of the doc is reported broken.
        # False FAILURE, the dangerous direction. Latent: no occurrence of
        # this shape in the tree. Fixing it means letting the run cross a
        # blank line, which also lets it cross into whatever sits above.
        self.write(
            "crates/c/src/lib.rs",
            "/// Doc.\n"            # 1
            "\n"                    # 2
            "pub fn wanted() {\n"   # 3
            "    ()\n"              # 4
            "}\n",                  # 5
        )
        self.write("docs/a.md", "See `crates/c/src/lib.rs:1#wanted`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "cites line 1, but `wanted` in `crates/c/src/lib.rs` spans 3-5.",
        )

    def test_an_ordinary_comment_inside_the_doc_run_truncates_the_span(self):
        # A `//` line between two `///` lines ends the run, so every doc line
        # above it is outside the item. False FAILURE again, same direction
        # and same remedy as above. Latent: no occurrence in the tree.
        self.write(
            "crates/c/src/lib.rs",
            "/// Doc line one.\n"     # 1
            "// An ordinary note.\n"  # 2
            "/// Doc line two.\n"     # 3
            "pub fn wanted() {\n"     # 4
            "    ()\n"                # 5
            "}\n",                    # 6
        )
        self.write("docs/a.md", "See `crates/c/src/lib.rs:1#wanted`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "cites line 1, but `wanted` in `crates/c/src/lib.rs` spans 3-6.",
        )

    def test_a_macro_rules_declaration_is_not_found(self):
        # `macro_rules!` is not in `SYMBOL_DECL`'s alternation and the `!`
        # would not match its whitespace either way. False FAILURE, and
        # unreachable rather than merely latent: this repository declares no
        # macro this way, so nothing can cite one until something does.
        self.write(
            "crates/c/src/lib.rs",
            "/// Doc.\n"                 # 1
            "macro_rules! wanted {\n"    # 2
            "    () => {};\n"            # 3
            "}\n",                       # 4
        )
        self.write("docs/a.md", "See `crates/c/src/lib.rs:2#wanted`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "names `wanted`, which `crates/c/src/lib.rs` does not declare.",
        )

    def test_a_declaration_inside_a_raw_string_widens_the_span(self):
        # The walk reads lines, not Rust, so a declaration written inside a
        # raw string literal is found and contributes a phantom span. This is
        # the one shape here that errs the safe way: containment is tested
        # against the union, so a phantom can only make a citation pass that
        # would otherwise fail. Pinned as the union, both spans named, so that
        # a later narrowing of `symbol_span` is felt here rather than in a
        # document going red.
        self.write(
            "crates/c/src/lib.rs",
            '/// Doc.\n'                        # 1
            'pub const SRC: &str = r#"\n'       # 2
            'pub fn wanted() -> u8 { 0 }\n'     # 3
            '"#;\n'                             # 4
            '\n'                                # 5
            '/// Real doc.\n'                   # 6
            'pub fn wanted() -> u8 {\n'         # 7
            '    0\n'                           # 8
            '}\n',                              # 9
        )
        self.write("docs/a.md", "See `crates/c/src/lib.rs:3#wanted`.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")

        self.write("docs/b.md", "See `crates/c/src/lib.rs:5#wanted`.\n")
        self.assert_code(self.run_gate(), DANGLING, "spans 3-3, 6-9.")

    def test_a_span_whose_close_is_never_found_runs_to_the_end_of_the_file(self):
        # The documented safe direction, asserted rather than assumed. An item
        # whose closing brace never appears at its own indent -- a truncated
        # file, or a shape the line-based walk cannot match -- gets a span
        # that runs to EOF. Over-broad, so it can only ever PASS a citation,
        # never fail a correct one, which is the only direction this gate is
        # allowed to be wrong in.
        #
        # Line 5 is textually inside `other`, and the assertion is that
        # `#wanted` accepts it anyway. Narrow the fallback to the declaration
        # line and this goes red, which is the point: the cost of the safe
        # direction is stated here instead of being discovered in a document.
        self.write(
            "crates/c/src/lib.rs",
            "/// Doc.\n"                   # 1
            "pub fn wanted() -> u8 {\n"    # 2
            "    0\n"                      # 3
            "    // never closed\n"        # 4
            "fn other() -> u8 { 0 }\n",    # 5
        )
        self.write("docs/a.md", "See `crates/c/src/lib.rs:5#wanted`.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")

    def test_a_tracked_rust_file_absent_from_the_worktree_stops_the_gate(self):
        # `git_tracked` reads the index, so a file staged and then removed
        # from the working tree is still a file this gate believes exists.
        # `symbol_span` has an `OSError` arm for exactly that, returning no
        # spans -- but through the command line that arm cannot be reached for
        # a `.rs` target, because the scan reads every tracked `.rs` file
        # looking for code spans and raises on this one first.
        #
        # So the outcome is exit 2, "the gate could not run", and NOT a
        # citation verdict. That is the right answer and the one worth
        # pinning: a missing file must never be reported as a document naming
        # a symbol that does not exist. `symbol_span`'s arm is exercised
        # directly in the gate's own `self_test`, which can call it without a
        # scan in front of it.
        path = self.write("crates/c/src/lib.rs", "pub fn wanted() -> u8 {\n    0\n}\n")
        path.unlink()
        self.write("docs/a.md", "See `crates/c/src/lib.rs#wanted`.\n")
        self.assert_code(
            self.run_gate(),
            COULD_NOT_RUN,
            "could not read crates/c/src/lib.rs",
        )

    def test_a_suffix_that_is_not_an_item_name_fails_and_subtracts_nothing(self):
        # The defect this grammar was widened for. `#Engine::f` and `#f()` are
        # how a Rust method is written by hand, and while the symbol group was
        # itself the identifier pattern such a span failed `CITATION` outright
        # -- so it was not counted, not listed as unanchored, and LOST the
        # path and line checks it had carried before the suffix was added.
        # Appending a suffix made the build greener by checking less, which is
        # the one thing a suffix must never be able to do.
        #
        # Both halves are asserted: the suffix is reported, and the line check
        # still runs on the same span and still comes first.
        self.rust()
        self.write("docs/a.md", "See `crates/c/src/lib.rs:3#wanted::inner`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "carries `#wanted::inner`, which is not a Rust item name this gate can resolve",
        )

        self.write("docs/b.md", "See `crates/c/src/lib.rs:3#wanted()`.\n")
        self.assert_code(self.run_gate(), DANGLING, "carries `#wanted()`, which is not a Rust")

        # The subtraction, stated directly. A line past the end of the file is
        # reported as such even though the suffix beside it is unparseable.
        self.write("docs/c.md", "See `crates/c/src/lib.rs:99#wanted::inner`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "cites line 99, but `crates/c/src/lib.rs` has 11 line(s).",
        )

    def test_an_unparseable_suffix_on_a_dangling_path_still_reports_the_path(self):
        # The other half of the subtraction: a path that resolves to nothing,
        # carrying a suffix the gate cannot read, used to vanish from the run
        # entirely -- where the same path with `:5` instead reports a dangling
        # file. The path verdict is the older and more useful one, so it keeps
        # precedence over the suffix verdict.
        self.write("crates/c/src/lib.rs", "pub fn wanted() -> u8 {\n    0\n}\n")
        self.write("docs/a.md", "See `crates/c/src/gone.rs#Engine::f`.\n")
        self.assert_code(self.run_gate(), DANGLING, "names no file git tracks.")

    def test_a_github_line_fragment_is_declined_not_failed(self):
        # `#L702` is a github.com permalink fragment and the one
        # non-declaration `#` form a Rust-path span plausibly carries. It
        # names a line, not an item, so there is nothing to resolve and no
        # reading under which failing it would be right -- and this gate may
        # not invent a failure for a spelling it never promised to read.
        # Declined exactly as a non-Rust suffix is, and to the same extent:
        # the SYMBOL check is declined and the span stays counted.
        self.rust()
        self.write(
            "docs/a.md",
            "See `crates/c/src/lib.rs:3#L3` and `crates/c/src/lib.rs#L3`.\n",
        )
        result = self.run_gate()
        self.assert_code(result, CLEAN, "OK: citations clean.", "2 anchored citation(s)")
        self.assertNotIn("path-like span(s) were NOT checked", result.stdout)

    def test_every_github_fragment_spelling_is_declined(self):
        # github.com emits a range for any multi-line selection and a column
        # form when columns are included. This repository's citations are
        # overwhelmingly ranges (`:31-63`, `:294-300`), so the range fragment
        # is the one a contributor actually pastes -- and while the pattern
        # read `L\d+` alone, every one of these red-lined a document for a
        # spelling the gate had declared it would decline.
        #
        # No digit cap either: a permalink is not wrong for being long.
        self.rust()
        self.write(
            "docs/a.md",
            "See `crates/c/src/lib.rs#L3-L9`, `crates/c/src/lib.rs:3#L3-L9`, "
            "`crates/c/src/lib.rs#L3C5-L9C20`, `crates/c/src/lib.rs#L3C5` and "
            "`crates/c/src/lib.rs#L1234567890`.\n",
        )
        self.assert_code(
            self.run_gate(), CLEAN, "OK: citations clean.", "5 anchored citation(s)"
        )

    def test_a_line_fragment_does_not_excuse_the_checks_beside_it(self):
        # The placement, stated as five cases because each is a different
        # branch of `classify` and a decline taken at the top removed all of
        # them at once. Every span here fails without its suffix; appending a
        # permalink fragment may not make the build greener.
        self.rust()
        for span, fragment in (
            ("crates/c/src/lib.rs:99#L99", "cites line 99, but `crates/c/src/lib.rs` has 11"),
            ("crates/c/src/gone.rs#L5", "names no file git tracks."),
            ("crates/c/src/lib.rs:50-40#L1", "cites an empty range (50-40)."),
            ("crates/c/src/lib.rs:0#L0", "cites line 0; line numbers start at 1."),
            ("../../etc/passwd.rs:1#L1", "climbs out of the repository."),
        ):
            with self.subTest(span=span):
                self.write("docs/a.md", f"See `{span}`.\n")
                self.assert_code(self.run_gate(), DANGLING, fragment)

    def test_an_impl_block_is_not_a_citation_target(self):
        # An `impl` block is a container, not the item a sentence is about.
        # While `impl` was in `SYMBOL_DECL`'s alternation, `#Engine` resolved
        # to 97.7% of `sync.rs` and containment against it was no stronger
        # than the line-existence check the suffix exists to improve on --
        # while counting, in this gate's own output, as a checked citation
        # indistinguishable from a real one.
        #
        # Asserted from both sides: the `impl` name does not resolve, and a
        # `fn` declared inside the block still does, so dropping the keyword
        # cost the gate no real target.
        self.write(
            "crates/c/src/lib.rs",
            "pub struct Other;\n"         # 1
            "impl Wanted {\n"             # 2
            "    pub fn inner(&self) {\n" # 3
            "    }\n"                     # 4
            "}\n",                        # 5
        )
        self.write("docs/a.md", "See `crates/c/src/lib.rs:2#Wanted`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "names `Wanted`, which `crates/c/src/lib.rs` does not declare.",
        )

        self.write("docs/a.md", "See `crates/c/src/lib.rs:3#inner`.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")

        # The generic form, which used to be recorded as a separate defect on
        # the reading that the lifetimes were what the pattern could not
        # match. With `impl` out of the alternation the two forms are the same
        # case and the same deliberate verdict, and this asserts they stay
        # that way rather than one of them quietly resolving again.
        self.write(
            "crates/c/src/lib.rs",
            "pub struct Other;\n"         # 1
            "impl<'a> Wanted<'a> {\n"     # 2
            "    pub fn f(&self) {}\n"    # 3
            "}\n",                        # 4
        )
        self.write("docs/a.md", "See `crates/c/src/lib.rs:2#Wanted`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "names `Wanted`, which `crates/c/src/lib.rs` does not declare.",
        )

    def test_a_mod_is_not_a_citation_target(self):
        # `mod` left the alternation for the reason `impl` did, and the
        # measurement is worse rather than better: `mod tests` in
        # `crates/sunrise-server/src/api/sync/suite.rs` spans lines 9 to 1720
        # of a 1720-line file -- 99.5%, against `impl Engine`'s 97.7% -- and
        # `mod tests` at the tail of a file is this repository's dominant
        # idiom, not an edge shape. Containment against a span that size
        # certifies nothing the line-existence check did not already certify,
        # while counting in the gate's own output as a checked citation
        # indistinguishable from a real one.
        #
        # The fixture is that shape in miniature: `mod tests` covering all but
        # the first line. While `mod` resolved, a citation of ANY line in it
        # came back clean, which is the property being removed.
        self.write(
            "crates/c/src/lib.rs",
            "pub fn outside() {}\n"       # 1
            "#[cfg(test)]\n"              # 2
            "mod tests {\n"               # 3
            "    fn a() {}\n"             # 4
            "    fn b() {}\n"             # 5
            "}\n",                        # 6
        )
        self.write("docs/a.md", "See `crates/c/src/lib.rs:4#tests`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "names `tests`, which `crates/c/src/lib.rs` does not declare.",
        )

        # Asserted from the other side too, so the removal is the keyword's
        # and not the fixture's: an item declared inside the block is still a
        # target, and still bounds the lines it actually covers.
        self.write("docs/a.md", "See `crates/c/src/lib.rs:4#a`.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.")

        self.write("docs/a.md", "See `crates/c/src/lib.rs:5#a`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "cites line 5, but `a` in `crates/c/src/lib.rs` spans 4-4.",
        )

    def test_the_suffix_grammar_has_no_length_bound(self):
        # A bound on the grammar side re-creates, at its own edge, the
        # subtraction the grammar was widened to remove. While the group read
        # `\S{1,128}`, a 129-character suffix and a bare trailing `#` failed
        # `CITATION` outright, so the span was dropped from the run and lost
        # the line check it already had -- the same defect as writing
        # `#Engine::f`, reached by a different spelling.
        #
        # The length limit lives in `SYMBOL_NAME` instead, where overrunning
        # it is a reported failure rather than a silent exit.
        self.rust()
        for suffix in ("a" * 129, "", "#"):
            with self.subTest(suffix=suffix):
                self.write("docs/a.md", f"See `crates/c/src/lib.rs:99#{suffix}`.\n")
                self.assert_code(
                    self.run_gate(),
                    DANGLING,
                    "cites line 99, but `crates/c/src/lib.rs` has 11 line(s).",
                )

        # 128 is inside `SYMBOL_NAME`, 129 is outside it, and the two verdicts
        # differ in which question went unanswered -- never in whether the
        # span was looked at.
        self.write("docs/a.md", "See `crates/c/src/lib.rs:3#" + "a" * 128 + "`.\n")
        self.assert_code(self.run_gate(), DANGLING, "does not declare")
        self.write("docs/a.md", "See `crates/c/src/lib.rs:3#" + "a" * 129 + "`.\n")
        self.assert_code(self.run_gate(), DANGLING, "is not a Rust item name")

    def test_a_suffix_holding_whitespace_keeps_every_check_and_is_reported(self):
        # The last edge at which the grammar still subtracted. While the
        # symbol group read `\S*`, `:99#two words` came back CLEAN where
        # `:99` was red: the whole span failed `CITATION`, so it was not
        # counted, not listed as unanchored, and lost the line check it
        # already had. Appending a suffix made the build greener by checking
        # less -- the same defect as `#Engine::f` and the 129-character run,
        # reached by a third spelling.
        #
        # Every kind of whitespace, because they are one rule and only the
        # first is visible: a plain space, a tab, a no-break space, and the
        # bare trailing space that is the most human-reachable spelling of
        # all -- `foo.rs:99# ` is a stray keystroke nothing renders.
        self.rust()
        for suffix in ("two words", "two\twords", "two\u00a0words", " "):
            with self.subTest(suffix=suffix):
                self.write("docs/a.md", f"See `crates/c/src/lib.rs:99#{suffix}`.\n")
                self.assert_code(
                    self.run_gate(),
                    DANGLING,
                    "cites line 99, but `crates/c/src/lib.rs` has 11 line(s).",
                )

        # And where the path and line are fine, the suffix itself is the
        # failure -- reported, never skipped.
        self.write("docs/a.md", "See `crates/c/src/lib.rs:3#two words`.\n")
        self.assert_code(self.run_gate(), DANGLING, "is not a Rust item name")

    def test_a_carriage_return_loses_the_span_before_the_grammar_sees_it(self):
        # The one character the suffix grammar's "no character excluded" does
        # NOT cover, pinned so the sentence that now says so has a test
        # behind it.
        #
        # Nothing in `classify` decides this. The scanner reads every file
        # through `open(..., encoding="utf-8")`, whose universal-newline
        # translation rewrites a lone `\r` to `\n` before `CODE_SPAN` runs,
        # and `CODE_SPAN`'s body is `[^\n]+?` -- so the backticks never close
        # and there is no span to classify. The result is a citation counted
        # in NEITHER tally: not anchored, not declined.
        #
        # **This is not the suffix subtracting a check**, which is the
        # reading to rule out, and the second half of the test is what rules
        # it out: a `\r` in the PATH and a `\r` in the LINE NUMBER lose the
        # span just as completely, and so does one in a span carrying no `#`
        # at all. It is a property of the span, identical before and after
        # this change, and no tracked `.md` or `.rs` file in this repository
        # contains one. Repairing it would mean reading with `newline=""` at
        # ONE call site -- the scanner's; `line_count` and `symbol_span` both
        # open `"rb"` and `newline=` does not reach them -- over the 454 `.md`
        # and `.rs` files this repository's own run scans, of which zero hold
        # a lone `\r` and zero hold CRLF. So the measured radius of that
        # repair is ZERO files, and the behaviour is kept because nothing
        # produces the input, not because the change would be wide. Recorded
        # here rather than changed, and a future reader reopening it is
        # deciding against that cost and not a larger one.
        #
        # `self.rust()` below is load-bearing: `0 anchored citation(s)` holds
        # only because that fixture writes no citation of its own, so a
        # fixture that starts citing something fails this case for a reason
        # that has nothing to do with carriage returns.
        self.rust()

        # The control: without the `\r` this exact span is red.
        self.write("docs/a.md", "See `crates/c/src/lib.rs:99#ab`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "cites line 99, but `crates/c/src/lib.rs` has 11 line(s).",
        )

        for body in (
            "crates/c/src/lib.rs:99#a\rb",  # in the suffix
            "crates/c/src/lib.rs:9\r9",  # in the line number
            "crates/c/src/li\rb.rs:99",  # in the path
            "crates/c/src/lib.rs\r",  # no `#` anywhere
        ):
            with self.subTest(body=body):
                self.write("docs/a.md", f"See `{body}`.\n")
                result = self.run_gate()
                self.assert_code(result, CLEAN, "OK: citations clean.")
                self.assertIn("0 anchored citation(s)", result.stdout)
                self.assertNotIn("path-like span(s) were NOT checked", result.stdout)

    def test_prose_holding_a_space_is_still_not_a_citation(self):
        # The other half of the widening, and the reason it costs nothing.
        # What keeps prose out of the grammar is the PATH group, never the
        # symbol group -- and it keeps these five out three different ways,
        # which is worth writing down because the one-line version ("none of
        # them carries a `#`") is false:
        #
        #   docs/a.md and docs/b.md   whitespace
        #   see docs/prose.md         whitespace
        #   cargo test --workspace    whitespace
        #   #[derive(Debug)]          CARRIES a `#`, and the path group's
        #                             first character class has no `#` in it
        #   Vec<u8>                   no whitespace, no `#`; no dotted
        #                             extension either
        #
        # `docs/a.md and docs/b.md` is a sentence about two files and has to
        # stay one.
        self.write("docs/b.md", "one\n")
        self.write(
            "docs/a.md",
            "Prose: `docs/a.md and docs/b.md`, `see docs/prose.md`, "
            "`cargo test --workspace`, `Vec<u8>`, `#[derive(Debug)]`.\n",
        )
        result = self.run_gate()
        self.assert_code(result, CLEAN, "OK: citations clean.", "0 anchored citation(s)")
        self.assertNotIn("path-like span(s) were NOT checked", result.stdout)

    def test_a_citation_with_no_suffix_is_unchanged(self):
        # The widening, asserted as a widening. Every citation in this
        # repository is this shape, and all three verdicts it can reach have
        # to be exactly what they were before `#symbol` existed.
        self.rust()
        self.write(
            "docs/a.md",
            "In range `crates/c/src/lib.rs:10`, whole file `crates/c/src/lib.rs`.\n",
        )
        self.assert_code(self.run_gate(), CLEAN, "OK: citations clean.", "2 anchored citation(s)")

        self.write("docs/b.md", "Past the end `crates/c/src/lib.rs:12`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "cites line 12, but `crates/c/src/lib.rs` has 11 line(s).",
        )

    def test_a_suffix_on_a_target_that_is_not_rust_declines_only_the_symbol(self):
        # `docs/x.md#heading` is a link fragment, and there is no resolver for
        # a symbol outside Rust. It must not crash and must not fail -- but
        # the decline is of the SYMBOL check, not of the span. Every one of
        # these resolves, so every one is counted, exactly as the same span
        # without its suffix would be.
        self.write("docs/b.md", "one\ntwo\n")
        self.write("Cargo.toml", "[workspace]\n")
        self.write(
            "docs/a.md",
            "See `docs/b.md#heading`, `docs/b.md:1#heading` and `Cargo.toml#package`.\n",
        )
        result = self.run_gate()
        self.assert_code(result, CLEAN, "OK: citations clean.", "3 anchored citation(s)")
        self.assertNotIn("path-like span(s) were NOT checked", result.stdout)

    def test_a_non_rust_suffix_does_not_excuse_a_dangling_path(self):
        # The half the placement decides. A suffix the gate cannot read is no
        # reason to stop reading the path beside it: `docs/gone.md:5` is a
        # reported dangling citation, so `docs/gone.md#heading` is one too.
        #
        # Taken before resolution -- which is where this decline first sat --
        # the span vanished from the run and the dangling file went unseen,
        # which is a document the gate would have caught being made green by
        # a fragment somebody pasted.
        self.write("docs/a.md", "See `docs/gone.md#heading`.\n")
        self.assert_code(self.run_gate(), DANGLING, "names no file git tracks.")

    def test_a_non_rust_suffix_does_not_excuse_a_line_past_the_end(self):
        # The same rule at the line check rather than the path check, because
        # the two are separate branches and only a case each pins both.
        self.write("docs/b.md", "one\ntwo\n")
        self.write("docs/a.md", "See `docs/b.md:9#heading`.\n")
        self.assert_code(
            self.run_gate(), DANGLING, "cites line 9, but `docs/b.md` has 2 line(s)."
        )

    def test_a_symbol_on_a_directory_fails_whatever_the_extension(self):
        # A directory declares nothing under any grammar, so the verdict
        # cannot turn on the extension of a path that names no file. It used
        # to: the exemption here tested `declined`, whose other half is "the
        # target is not Rust", and that made `bundle.json#thing` a clean,
        # counted pass while `mod.rs#anything` failed -- an asymmetry nothing
        # chose and no citation in this repository has ever reached, since the
        # only tracked directories with extension-shaped names are `.cargo`,
        # `.github` and `.vscode` and none of those suffixes is in
        # `EXTENSIONS`.
        #
        # What the non-Rust arm declines is the SYMBOL check. The directory
        # check is one of the others the span keeps, exactly as the path and
        # line checks are.
        self.write("schemas/bundle.json/part.json", "{}\n")
        self.write("docs/a.md", "See `schemas/bundle.json#thing`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "names `thing`, but `schemas/bundle.json` is a directory.",
        )

        # The markdown spelling of the same suffix, which is the one a reader
        # would call a link anchor rather than a symbol. Still a directory.
        self.write("docs/a.md", "See `schemas/bundle.json#heading`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "names `heading`, but `schemas/bundle.json` is a directory.",
        )

    def test_a_permalink_fragment_on_a_directory_fails_like_the_line_does(self):
        # `#L702` names a LINE. A directory has none -- which is not this
        # test's claim but the gate's own, made three lines earlier in
        # `classify` about `dir:702` and reported as "cites a line, but ... is
        # a directory".
        #
        # So the two spellings of that one assertion get the one verdict. The
        # fragment was briefly exempt here on the ground that "declares
        # nothing" answers nothing about a line, which is true and answers the
        # wrong objection: "has no lines either" is the answer, and the gate
        # already gives it. Under the exemption `dir#L702` was a clean pass
        # counted as an anchored citation -- verified, with nothing verified,
        # which is the single shape this whole suffix exists to prevent.
        #
        # Asserted on both extensions, because the directory verdict must not
        # become extension-dependent again on any of its arms.
        self.write("crates/c/src/mod.rs/inner.rs", "pub fn anything() {}\n")
        self.write("schemas/bundle.json/part.json", "{}\n")
        for body, message in (
            ("crates/c/src/mod.rs#L702", "names `L702`, but `crates/c/src/mod.rs` is a directory."),
            (
                "crates/c/src/mod.rs#L702-L710",
                "names `L702-L710`, but `crates/c/src/mod.rs` is a directory.",
            ),
            (
                "schemas/bundle.json#L702",
                "names `L702`, but `schemas/bundle.json` is a directory.",
            ),
        ):
            with self.subTest(body=body):
                self.write("docs/a.md", f"See `{body}`.\n")
                self.assert_code(self.run_gate(), DANGLING, message)

        # And the spelling that always failed, unchanged, so the pair is
        # asserted rather than just the half that moved.
        self.write("docs/a.md", "See `crates/c/src/mod.rs:702#L702`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "cites a line, but `crates/c/src/mod.rs` is a directory.",
        )

    def test_a_bare_hash_on_a_directory_fails_naming_no_symbol(self):
        # A trailing `#` with nothing after it parses, because the grammar
        # carries no lower bound on the suffix -- a bound there would drop the
        # span from the run with the checks it already had. On a directory it
        # reaches the failure above with an empty symbol, so the message names
        # nothing between its backticks. That is the honest report: the
        # citation named no item, and the path names no file either.
        self.write("crates/c/src/mod.rs/inner.rs", "pub fn anything() {}\n")
        self.write("docs/a.md", "See `crates/c/src/mod.rs#`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "names ``, but `crates/c/src/mod.rs` is a directory.",
        )

    def test_a_symbol_on_a_rust_named_directory_fails(self):
        # The same verdict as the `.json` directory above, reached through the
        # extension the gate does have a resolver for -- which is the point of
        # keeping both: they must not differ. A directory declares nothing, so
        # there is no reading under which this citation is correct, which is
        # why it is a failure and not a decline and why no correct document
        # can be red-lined by it.
        #
        # A silent clean pass here -- counted as an anchored, checked citation
        # for a symbol no resolver was ever consulted about -- is exactly the
        # unverified suffix the feature exists to prevent. It was reachable in
        # this change's own first draft of the suffix and not before it: with
        # no `#` group in `CITATION` the span never parsed as a citation, so
        # it was skipped rather than passed.
        self.write("crates/c/src/mod.rs/inner.rs", "pub fn anything() {}\n")
        self.write("docs/a.md", "See `crates/c/src/mod.rs#anything`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "names `anything`, but `crates/c/src/mod.rs` is a directory.",
        )

    def test_a_line_and_a_symbol_on_a_directory_still_reports_the_line(self):
        # Both are true of the citation and the line is the older verdict, so
        # it keeps precedence. Pinned so the message does not drift when
        # somebody reorders the branch.
        self.write("crates/c/src/mod.rs/inner.rs", "pub fn anything() {}\n")
        self.write("docs/a.md", "See `crates/c/src/mod.rs:5#anything`.\n")
        self.assert_code(
            self.run_gate(),
            DANGLING,
            "cites a line, but `crates/c/src/mod.rs` is a directory.",
        )

    def test_a_bare_directory_path_is_still_clean(self):
        # The rule the change above had to leave alone. A directory cited
        # with neither a line nor a symbol resolves exactly as it did before
        # the suffix existed.
        self.write("crates/c/src/mod.rs/inner.rs", "pub fn anything() {}\n")
        self.write("docs/a.md", "See `crates/c/src/mod.rs`.\n")
        self.assert_code(
            self.run_gate(), CLEAN, "OK: citations clean.", "1 anchored citation(s)"
        )


class Allowlist(GateCase):
    """The escape hatch, and the two guards that keep it from rotting."""

    ENTRY = ("docs/a.md", "crates/gone.rs")

    def test_an_entry_excuses_the_sentence_it_names(self):
        self.write("docs/a.md", "`crates/gone.rs` was deleted, and this records that.\n")
        self.write("crates/c/src/lib.rs", "pub fn f() {}\n")
        self.assert_code(
            self.run_gate(allowed={self.ENTRY: "the sentence asserts the absence"}),
            CLEAN,
            "OK: citations clean.",
        )

    def test_an_entry_excuses_one_file_and_not_the_spelling_everywhere(self):
        # Keyed on the pair, so an ADR recording a deletion does not also
        # silence a live doc comment that rotted into the same path.
        self.write("docs/a.md", "`crates/gone.rs` was deleted, and this records that.\n")
        self.write("docs/b.md", "Go and read `crates/gone.rs`.\n")
        self.write("crates/c/src/lib.rs", "pub fn f() {}\n")
        self.assert_code(
            self.run_gate(allowed={self.ENTRY: "the sentence asserts the absence"}),
            DANGLING,
            "::error file=docs/b.md,line=1",
        )

    def test_an_entry_whose_path_comes_back_is_a_failed_run(self):
        # The tree moved under the list. Exit 2, because nothing was scored.
        self.write("docs/a.md", "`crates/gone.rs` is here again.\n")
        self.write("crates/gone.rs", "pub fn f() {}\n")
        self.assert_code(
            self.run_gate(allowed={self.ENTRY: "the sentence asserts the absence"}),
            COULD_NOT_RUN,
            "git tracks it now; delete the entry.",
        )

    def test_an_entry_whose_citation_is_gone_is_a_failed_run(self):
        # The document moved under the list. Without this an entry outlives
        # the sentence it was written for and silently widens the hatch.
        self.write("docs/a.md", "The sentence was rewritten and cites nothing.\n")
        self.assert_code(
            self.run_gate(allowed={self.ENTRY: "the sentence asserts the absence"}),
            COULD_NOT_RUN,
            "no such citation is there any more; delete the entry.",
        )


class CouldNotRun(GateCase):
    """Exit 2 is "nothing was scored", and must not read as a verdict."""

    def test_a_tree_with_no_markdown_or_rust_cannot_be_scanned(self):
        self.write("Cargo.toml", "[workspace]\n")
        self.assert_code(
            self.run_gate(),
            COULD_NOT_RUN,
            "git reports no markdown or Rust to scan; the gate could not run.",
        )

    def test_a_root_outside_a_git_work_tree_is_a_failed_run(self):
        outside = self.tmp / "not-a-repo"
        outside.mkdir()
        gate = rewritten_gate(self.tmp)
        result = subprocess.run(
            [sys.executable, str(gate), "--root", str(outside)],
            capture_output=True,
            text=True,
            cwd=self.tmp,
        )
        self.assertEqual(result.returncode, COULD_NOT_RUN, result.stdout + result.stderr)
        self.assertIn("`git ls-files` failed", result.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
