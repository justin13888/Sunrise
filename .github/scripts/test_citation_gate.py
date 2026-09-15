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
