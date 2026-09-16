#!/usr/bin/env python3
"""The exit-code contract of `doc-comment-gate.py`, as assertions.

Why this file exists
--------------------

The gate carries its own `--self-test`, and that self-test runs `scan_text`
over six string literals. It is a good check of the five rules and no check
at all of everything around them: the `git ls-files` listing, the 50-file
floor that says "this scan is broken" rather than "this tree is clean", the
exit codes those two produce, and the annotation format a reader follows
back to a line. Those are what `ci.yml` and a person reading a red check
depend on, and nothing exercised them.

Both directions are here. `Rejects` pins each of the five shapes the gate
exists for, and `Accepts` pins the shapes a plausible implementation of
those five rules gets wrong — a list continuation indented past its marker,
a lazy paragraph wrap, four columns inside a fence, a doc comment under an
*inner* attribute. A gate that fires on those is one people route around.

The masking contract
--------------------

`InlineCodeIsBlanked` is not about this gate alone. `doc-comment-gate.py`
blanks inline code before it looks for a stray `///`, and
`docs-link-gate.py` does the same before it extracts a link. That is what
leaves backticked `path:line` citations unchecked by either, which is why
`citation-gate.py` exists and reads exactly what these two mask. The three
only stay complementary while the masking stays put, so it is asserted here
rather than assumed — a change that made this gate read inside code spans
would start reporting every doc comment that quotes a marker, and would
also make two gates fight over the same span.

Fixtures are synthesised crates
-------------------------------

Every case builds a git repository in a temp directory and drives the gate
with `--root`. Nothing here reads this repository's own `.rs` files: the
`doc-comments` job does that, and a contract test that also did would go
red for whatever module somebody edited this week. The gate refuses to run
on fewer than 50 tracked `.rs` files, so each fixture is padded up to it —
and that floor is itself the subject of a case, because it is a check and
not a formality.

Run it with `python3 .github/scripts/test_doc_comment_gate.py`.
"""

from __future__ import annotations

import pathlib
import subprocess
import sys
import tempfile
import unittest

GATE = pathlib.Path(__file__).resolve().parent / "doc-comment-gate.py"

CLEAN = 0
VIOLATION = 1
COULD_NOT_RUN = 2

# The gate needs 50 tracked `.rs` files before it will believe a listing.
FLOOR = 50


class GateCase(unittest.TestCase):
    """One temp repository per test, padded past the gate's file floor."""

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

    def write(self, rel: str, text: str) -> None:
        path = self.repo / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        self._git("add", "--", rel)

    def pad(self, count: int = FLOOR) -> None:
        """Enough clean `.rs` files that the gate will read the listing.

        Staged in one `git add`, because fifty of them in fifty processes is
        most of this file's runtime.
        """
        directory = self.repo / "crates" / "pad" / "src"
        directory.mkdir(parents=True, exist_ok=True)
        for index in range(count):
            (directory / f"f{index}.rs").write_text("pub fn f() {}\n", encoding="utf-8")
        self._git("add", "--", "crates/pad")

    def source(self, text: str, name: str = "crates/c/src/lib.rs") -> None:
        self.pad()
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
    """The gate's five rules, asserted before it reads a tree."""

    def test_the_self_test_passes_alone(self):
        result = subprocess.run(
            [sys.executable, str(GATE), "--self-test"], capture_output=True, text=True
        )
        self.assertEqual(result.returncode, CLEAN, result.stdout + result.stderr)
        self.assertIn("OK: doc-comments self-test clean", result.stdout)

    def test_the_self_test_runs_as_a_precondition_of_a_check(self):
        self.source("pub fn f() {}\n")
        self.assertIn("OK: doc-comments self-test clean", self.run_gate().stdout)


class Clean(GateCase):
    """What a passing run says, and that it says how much it read."""

    def test_a_tree_of_well_shaped_comments_exits_zero(self):
        self.source("/// Documented, and shaped correctly.\npub fn f() {}\n")
        self.assert_code(self.run_gate(), CLEAN, f"OK: doc-comments clean ({FLOOR + 1} files).")


class Rejects(GateCase):
    """Each of the five shapes the gate exists for, by its code."""

    def test_two_markers_folded_onto_one_line(self):
        # The #77 shape: it renders as one run-on line and `cargo fmt` is
        # clean, because formatting comments is not rustfmt's job.
        self.source("/// A member cannot republish.    /// A member cannot republish.\npub fn f() {}\n")
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            "doc-comments[collapsed]: crates/c/src/lib.rs:1:",
            "a second `///` at column",
            "1 mangled doc comment(s)",
        )

    def test_a_fence_that_never_closes(self):
        self.source("/// Example:\n///\n/// ```rust\n/// let x = 1;\n///\n/// And the prose resumes.\npub fn f() {}\n")
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            "doc-comments[unclosed-fence]",
            "is never closed before the doc comment ends",
        )

    def test_prose_indented_until_rustdoc_compiles_it(self):
        # The #58/#60 defect: CommonMark reads four columns as a code block
        # and `cargo test --doc` then tries to compile the English.
        self.source("//! Returns a relative URI.\n//!\n//!     The path is relative to the server root.\npub fn f() {}\n")
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            "doc-comments[indented-code]",
            "rustdoc compiles it as Rust",
        )

    def test_a_comment_stranded_below_an_attribute(self):
        # The #63 shape: an edit inserted an item between a doc comment and
        # the thing it documented.
        self.source("#[test]\n/// What this test proves.\nfn t() {}\n")
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            "doc-comments[detached]",
            "a doc comment directly below the attribute `#[test]`",
        )

    def test_one_comment_split_in_two_by_a_blank_line(self):
        self.source("/// First half of a sentence\n\n/// second half of the same sentence.\npub fn f() {}\n")
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            "doc-comments[split]",
            "Rust joins them into one doc string",
        )

    def test_every_violation_is_reported_not_only_the_first(self):
        self.pad()
        self.write("crates/c/src/a.rs", "#[test]\n/// Detached.\nfn t() {}\n")
        self.write("crates/c/src/b.rs", "/// One.    /// Two.\npub fn f() {}\n")
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            "crates/c/src/a.rs,line=2",
            "crates/c/src/b.rs,line=1",
            "2 mangled doc comment(s)",
        )


class Accepts(GateCase):
    """The near-misses. Each is a shape a plausible rule gets wrong."""

    def accept(self, text: str) -> None:
        self.source(text)
        self.assert_code(self.run_gate(), CLEAN, "OK: doc-comments clean")

    def test_a_list_continuation_indented_to_its_own_text(self):
        # Four columns past the *page*, but not past its container, so it is
        # prose. An absolute indent rule fires here and is wrong.
        self.accept(
            "//! - A list item whose continuation is indented to line up\n"
            "//!   with its text, which is prose and not a code block.\n"
            "//!\n"
            "//!   A second paragraph inside that same item.\n"
        )

    def test_a_lazy_paragraph_continuation(self):
        # CommonMark keeps an over-indented second line as prose, because the
        # paragraph was already open.
        self.accept(
            "//! A paragraph whose second line\n"
            "//!     is over-indented as a lazy continuation, which is still\n"
            "//! prose.\n"
        )

    def test_indentation_inside_a_fence(self):
        self.accept(
            "//! ```rust\n"
            "//! let x = 1;\n"
            "//!     // four more columns, still inside the fence\n"
            "//! ```\n"
        )

    def test_an_inner_attribute_does_not_detach_the_comment_below_it(self):
        # `#![…]` applies to the item containing it and is written as the
        # first line of a body, so the comment below it starts fresh. Two
        # functions in `sunrise-cli` are exactly this shape, correctly.
        self.accept(
            "pub fn h() -> u32 {\n"
            "    #![allow(clippy::print_stdout)]\n"
            "    /// Enough to choose from without becoming a list.\n"
            "    const PICKS: u32 = 5;\n"
            "    PICKS\n"
            "}\n"
        )

    def test_attributes_after_the_comment_are_the_right_order(self):
        self.accept("/// Documented, attributes after it.\n#[must_use]\npub fn f() -> u8 {\n    0\n}\n")

    def test_an_outer_comment_below_an_inner_one_is_two_blocks(self):
        # Different markers, so no `split` finding: Rust does not join them.
        self.accept("//! Inner.\n\n/// Outer.\npub fn g() {}\n")

    def test_four_slashes_is_an_ordinary_comment(self):
        self.accept("//// Not a doc comment at all: /// ///\npub fn f() {}\n")

    def test_a_blank_line_between_two_different_items_is_not_a_split(self):
        self.accept("/// One item.\npub fn f() {}\n\n/// Another item.\npub fn g() {}\n")


class InlineCodeIsBlanked(GateCase):
    """The masking this gate shares with `docs-link-gate.py`, pinned.

    Both blank inline code before they read a line, deliberately, so a
    comment that *documents* markup is not read as carrying it. That is also
    why neither checks a backticked path citation, and why
    `citation-gate.py` reads exactly what they mask. Asserted here so the
    three do not start fighting over the same span.
    """

    def test_a_quoted_marker_inside_a_code_span_is_not_a_second_marker(self):
        self.source(
            "//! Use `///` for an outer doc comment and `//!` for an inner one.\npub fn f() {}\n"
        )
        self.assert_code(self.run_gate(), CLEAN, "OK: doc-comments clean")

    def test_an_unquoted_second_marker_still_fires(self):
        # The other half of the same rule: masking must blank code spans and
        # nothing else. Without this, "blank everything" also passes.
        self.source("//! Use /// for an outer doc comment.\npub fn f() {}\n")
        self.assert_code(self.run_gate(), VIOLATION, "doc-comments[collapsed]")

    def test_a_backticked_path_citation_is_not_this_gate_s_business(self):
        # It is masked here and checked by `citation-gate.py`. A dangling one
        # must leave this gate green, or the two gates are duplicating a
        # judgement they could disagree about.
        self.source("//! See `crates/sunrise-core/src/nonexistent.rs:1`.\npub fn f() {}\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: doc-comments clean")


class CouldNotRun(GateCase):
    """Exit 2 is "nothing was scanned", and must not read as a verdict."""

    def test_a_listing_below_the_floor_is_a_broken_scan_not_a_clean_tree(self):
        # The floor is the difference between "this workspace has no mangled
        # comments" and "the listing stopped finding files". Reporting the
        # first when the second happened is how a gate dies quietly.
        self.pad(10)
        self.assert_code(
            self.run_gate(),
            COULD_NOT_RUN,
            "only 10 tracked `.rs` file(s) found; the scan is broken.",
        )

    def test_an_empty_tree_is_a_broken_scan(self):
        self.assert_code(self.run_gate(), COULD_NOT_RUN, "the scan is broken.")

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
