#!/usr/bin/env python3
"""The exit-code contract of `citation-gate.py`, as assertions.

Why this file exists
--------------------

The gate's whole value is a *discrimination*: it has to fire on
`crates/sunrise-core/src/nonexistent.rs:1` and stay silent on `Vec<u8>`,
`cargo test --workspace` and the 1,500 path-shaped spans in this repository
that are shorthand rather than citations. Either half failing makes it
worthless, and the two fail in opposite directions from the same edit — a
regex loosened to catch one more real citation catches a hundred code
snippets, and a regex tightened to stop the snippets stops catching
anything.

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

`ALLOWED` and `DEFERRED` are literals in the gate, and every run checks both
for staleness against the tree it was pointed at, so a fixture repository
that does not contain this repository's excused and deferred citations fails
on the stale check before it can be asked anything else. Cases therefore run
against a copy of the gate with both literals rewritten, the way
`test_file_size_gate.py` rewrites `THRESHOLDS` and
`test_orphan_crate_gate.py` rewrites `EXEMPT` — the logic under test is still
the shipped logic, read from the shipped file at run time.

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

def literal(name: str) -> re.Pattern[str]:
    return re.compile(
        rf"^{name}: dict\[tuple\[str, str\], str\] = \{{.*?^\}}$", re.DOTALL | re.MULTILINE
    )


ALLOWED_LITERAL = literal("ALLOWED")
DEFERRED_LITERAL = literal("DEFERRED")


def rewritten_gate(
    directory: pathlib.Path,
    allowed: dict[tuple[str, str], str] | None = None,
    deferred: dict[tuple[str, str], str] | None = None,
) -> pathlib.Path:
    """The shipped gate with its two in-file lists replaced.

    Only the literals move. Every rule the gate applies is the one on disk,
    so a change to `classify` is felt here even though neither list is this
    repository's.
    """
    source = GATE.read_text(encoding="utf-8")
    for name, pattern, entries in (
        ("ALLOWED", ALLOWED_LITERAL, allowed or {}),
        ("DEFERRED", DEFERRED_LITERAL, deferred or {}),
    ):
        body = "\n".join(f"    {key!r}: {reason!r}," for key, reason in entries.items())
        replacement = (
            f"{name}: dict[tuple[str, str], str] = {{" + (f"\n{body}\n" if body else "") + "}"
        )
        source, count = pattern.subn(lambda _, r=replacement: r, source, count=1)
        if count != 1:
            raise AssertionError(f"the {name} literal is no longer where this test expects it")
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
        self,
        *args: str,
        allowed: dict[tuple[str, str], str] | None = None,
        deferred: dict[tuple[str, str], str] | None = None,
    ) -> subprocess.CompletedProcess:
        gate = rewritten_gate(self.tmp, allowed, deferred)
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

    def test_the_shipped_lists_stay_small(self):
        # Both are the gate's escape hatches and the thing most likely to grow
        # instead of the docs getting fixed. Twelve and four are not limits
        # anybody derived; they are tripwires, and passing one should be a
        # conversation rather than a commit.
        source = GATE.read_text(encoding="utf-8")
        for pattern, name, cap in ((ALLOWED_LITERAL, "ALLOWED", 12), (DEFERRED_LITERAL, "DEFERRED", 4)):
            found = pattern.search(source)
            self.assertIsNotNone(found, f"the {name} literal moved")
            entries = found.group(0).count('    (\n')
            self.assertLessEqual(entries, cap, f"{name} has grown; fix the docs instead")


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

    def test_a_directory_citation_with_an_extension_resolves(self):
        # An `.xcodeproj` bundle is a directory whose name carries an
        # extension, so it passes rule 2 and must resolve as a directory
        # rather than be reported as a missing file.
        self.write("apps/Sunrise.xcodeproj/project.pbxproj", "{}\n")
        self.write("docs/a.md", "See `apps/Sunrise.xcodeproj`.\n")
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
        self.write("apps/Sunrise.xcodeproj/project.pbxproj", "{}\n")
        self.write("docs/a.md", "See `apps/Sunrise.xcodeproj:12`.\n")
        self.assert_code(
            self.run_gate(), DANGLING, "cites a line, but `apps/Sunrise.xcodeproj` is a directory."
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

    def test_a_relative_path_out_of_a_document_is_not_checked(self):
        self.near_miss("`../06-server/api.md` and `Views/TaskEditorView.swift:97`")

    def test_unanchored_spans_are_counted_rather_than_dropped(self):
        # Declining to check something is only honest if the run says how
        # much it declined. This is the sentence that makes the hole visible.
        self.write("docs/a.md", "See `recovery.md` and `api/observe.rs`.\n")
        result = self.run_gate("--list-unanchored")
        self.assert_code(
            result,
            CLEAN,
            "2 path-like span(s) are claimed by no anchor and were NOT checked",
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

    def test_a_first_segment_naming_nothing_at_either_anchor_is_unanchored(self):
        # `src/main.rs` is claimed inside a crate and claimed by nothing from
        # a document, so it is counted rather than reported.
        self.write("docs/a.md", "See `src/main.rs`.\n")
        result = self.run_gate()
        self.assert_code(result, CLEAN, "path-like span(s) are claimed by no anchor")

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


class Deferred(GateCase):
    """The baseline of citations that are wrong and recorded rather than lost.

    `file-size-gate.py` carries the same shape: a list that may shrink and
    may not grow, printed on every run. The difference from `ALLOWED` is
    that these sentences are false, so the entry is a debt with an address.
    """

    ENTRY = ("docs/a.md", "crates/gone.rs")
    REASON = "a live citation nobody has repointed yet"

    def test_a_deferred_citation_is_printed_on_every_run(self):
        # Recorded, not hidden. A baseline nobody sees is an exemption.
        self.write("docs/a.md", "Go and read `crates/gone.rs`.\n")
        self.write("crates/c/src/lib.rs", "pub fn f() {}\n")
        self.assert_code(
            self.run_gate(deferred={self.ENTRY: self.REASON}),
            CLEAN,
            "citations: deferred — docs/a.md cites `crates/gone.rs`, which resolves to nothing",
            self.REASON,
            "OK: citations clean.",
        )

    def test_a_deferred_entry_excuses_one_file_and_not_the_spelling_everywhere(self):
        self.write("docs/a.md", "Go and read `crates/gone.rs`.\n")
        self.write("docs/b.md", "So should you: `crates/gone.rs`.\n")
        self.write("crates/c/src/lib.rs", "pub fn f() {}\n")
        self.assert_code(
            self.run_gate(deferred={self.ENTRY: self.REASON}),
            DANGLING,
            "::error file=docs/b.md,line=1",
        )

    def test_a_deferred_entry_deletes_itself_when_the_citation_is_fixed(self):
        # The only thing that removes an entry is fixing what it names, which
        # is what keeps the list shrinking.
        self.write("docs/a.md", "Go and read `crates/c/src/lib.rs`.\n")
        self.write("crates/c/src/lib.rs", "pub fn f() {}\n")
        self.assert_code(
            self.run_gate(deferred={self.ENTRY: self.REASON}),
            COULD_NOT_RUN,
            "no such citation is there any more; delete the entry.",
        )

    def test_a_deferred_entry_whose_path_comes_back_is_a_failed_run(self):
        self.write("docs/a.md", "`crates/gone.rs` is here again.\n")
        self.write("crates/gone.rs", "pub fn f() {}\n")
        self.assert_code(
            self.run_gate(deferred={self.ENTRY: self.REASON}),
            COULD_NOT_RUN,
            "git tracks it now; delete the entry.",
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
