#!/usr/bin/env python3
"""The exit-code contract of `docs-link-gate.py`, as assertions.

Why this file exists
--------------------

The gate's own `--self-test` is a very good test of `slugify`: 23 cases,
including a derivation of github-slugger's Unicode deletion ranges. It is
not a test of anything else. `check()` — the listing, the resolution of a
relative destination against the citing file's directory, the difference
between a file link and a directory link, the two exit codes and the
annotation a reader follows back to a line — is exercised by nothing but
the repository's own 1,239 links, which is a fixture that changes every
week and can only ever prove the gate is green.

Both directions are here. `Rejects` pins the four ways a link can resolve
to nothing; `Accepts` pins the shapes a plausible tightening would start
reporting — an external URL, a directory link, a `#L42` line anchor on a
source file, an autolink, a link inside a fenced block.

The masking contract
--------------------

`InlineCodeIsBlanked` is not about this gate alone. It blanks inline code
before extracting links, deliberately, so a document *about* markdown is
not read as containing the markup it quotes — and `doc-comment-gate.py`
does the same before looking for a stray marker. The consequence is that
neither checks a backticked `path:line` citation, which is why
`citation-gate.py` exists and reads exactly what these two mask. The three
stay complementary only while the masking stays put, so it is asserted here
rather than assumed.

Fixtures are synthesised repositories
-------------------------------------

Every case builds a git repository in a temp directory, writes markdown
into it and drives the gate with `--root`. `git init` plus `git add` is all
the setup needed: the gate compares against the index, which is what makes
"this file exists" mean "a reader on github.com can open it" and makes the
comparison exact-case on a case-insensitive checkout.

Run it with `python3 .github/scripts/test_docs_link_gate.py`.
"""

from __future__ import annotations

import pathlib
import subprocess
import sys
import tempfile
import unittest

GATE = pathlib.Path(__file__).resolve().parent / "docs-link-gate.py"

CLEAN = 0
BROKEN = 1
COULD_NOT_RUN = 2


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

    def write(self, rel: str, text: str) -> None:
        path = self.repo / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        self._git("add", "--", rel)

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

    def assert_absent(self, result: subprocess.CompletedProcess, fragment: str) -> None:
        self.assertNotIn(fragment, result.stdout + result.stderr)


class SelfTest(GateCase):
    """The slug and parsing rules, asserted before the gate reads a tree."""

    def test_the_self_test_passes_alone(self):
        result = subprocess.run(
            [sys.executable, str(GATE), "--self-test"], capture_output=True, text=True
        )
        self.assertEqual(result.returncode, CLEAN, result.stdout + result.stderr)
        self.assertIn("OK: docs-links self-test clean", result.stdout)

    def test_the_self_test_runs_as_a_precondition_of_a_check(self):
        self.write("README.md", "Nothing here.\n")
        self.assertIn("OK: docs-links self-test clean", self.run_gate().stdout)


class Clean(GateCase):
    """What a passing run says, and that it says how much it read."""

    def test_a_tree_of_resolving_links_exits_zero(self):
        self.write("docs/a.md", "# Title\n\n[to b](./b.md) and [back up](../README.md).\n")
        self.write("docs/b.md", "# Other\n")
        self.write("README.md", "# Root\n")
        self.assert_code(
            self.run_gate(), CLEAN, "docs-links: 2 links across 3 markdown files.", "OK: docs-links clean."
        )

    def test_an_anchor_into_another_file_resolves_against_that_file(self):
        self.write("docs/a.md", "[there](./b.md#some-heading)\n")
        self.write("docs/b.md", "## Some Heading\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: docs-links clean.")

    def test_an_in_file_anchor_resolves_against_its_own_headings(self):
        self.write("docs/a.md", "## Some Heading\n\n[here](#some-heading)\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: docs-links clean.")


class Rejects(GateCase):
    """Each way a link can resolve to nothing, with the message it prints."""

    def test_a_relative_file_link_to_nothing(self):
        self.write("docs/a.md", "[gone](./b.md)\n")
        self.assert_code(
            self.run_gate(),
            BROKEN,
            "::error file=docs/a.md,line=1::docs-links: [gone](./b.md) points at no file git tracks.",
            "1 link(s) resolve to nothing.",
        )

    def test_an_in_file_anchor_that_matches_no_heading(self):
        # The live defect: an ADR cross-reference whose slug doubled a hyphen
        # the heading does not. GitHub finds no such fragment and silently
        # leaves the reader at the top of the file.
        self.write("docs/a.md", "## Amendment (2026-08): OR-Set → append-only row\n\n[jump](#amendment-2026-08--or-set--append-only-row)\n")
        self.assert_code(
            self.run_gate(), BROKEN, "has no heading in this file slugging to"
        )

    def test_a_cross_file_anchor_that_matches_no_heading(self):
        self.write("docs/a.md", "[there](./b.md#missing)\n")
        self.write("docs/b.md", "## Present\n")
        self.assert_code(self.run_gate(), BROKEN, "has no heading in docs/b.md slugging to `missing`.")

    def test_a_link_that_climbs_out_of_the_repository(self):
        self.write("docs/a.md", "[out](../../elsewhere.md)\n")
        self.assert_code(self.run_gate(), BROKEN, "escapes the repository.")

    def test_a_reference_definition_is_a_link(self):
        self.write("docs/a.md", "Text.\n\n[label]: ./b.md\n")
        self.assert_code(self.run_gate(), BROKEN, "[label](./b.md) points at no file git tracks.")

    def test_an_image_destination_is_a_link(self):
        self.write("docs/a.md", "![a diagram](./diagram.png)\n")
        self.assert_code(self.run_gate(), BROKEN, "points at no file git tracks.")

    def test_case_is_compared_exactly(self):
        # A `ReadMe.md` for `README.md` passes on a case-insensitive macOS
        # checkout and fails on the Linux runner, which is the drift that
        # comparing against the index rather than the filesystem removes.
        self.write("README.md", "# Root\n")
        self.write("docs/a.md", "[root](../ReadMe.md)\n")
        self.assert_code(self.run_gate(), BROKEN, "points at no file git tracks.")

    def test_every_broken_link_is_reported_not_only_the_first(self):
        self.write("docs/a.md", "[one](./x.md)\n\n[two](./y.md)\n")
        self.write("docs/b.md", "[three](./z.md)\n")
        self.assert_code(
            self.run_gate(),
            BROKEN,
            "docs/a.md,line=1",
            "docs/a.md,line=3",
            "docs/b.md,line=1",
            "3 link(s) resolve to nothing.",
        )


class Accepts(GateCase):
    """The near-misses. A gate that fires on these is one people ignore."""

    def accept(self, *files: tuple[str, str]) -> subprocess.CompletedProcess:
        for name, text in files:
            self.write(name, text)
        result = self.run_gate()
        self.assert_code(result, CLEAN, "OK: docs-links clean.")
        return result

    def test_an_external_url_is_out_of_scope(self):
        # External reachability is flaky by construction, and a gate that
        # goes red on someone else's outage is one people learn to ignore.
        self.accept(("docs/a.md", "[spec](https://example.invalid/nope) and [mail](mailto:a@b.c)\n"))

    def test_a_protocol_relative_url_is_out_of_scope(self):
        self.accept(("docs/a.md", "[cdn](//example.invalid/x.js)\n"))

    def test_a_directory_link_has_no_headings_to_resolve_into(self):
        self.accept(
            ("docs/sub/b.md", "# B\n"),
            ("docs/a.md", "[the folder](./sub)\n"),
        )

    def test_a_line_anchor_on_a_source_file_is_not_a_heading(self):
        # `#L42` is GitHub's line anchor. Only markdown has headings.
        self.accept(
            ("crates/c/src/lib.rs", "pub fn f() {}\n"),
            ("docs/a.md", "[that line](../crates/c/src/lib.rs#L42)\n"),
        )

    def test_a_link_inside_a_fenced_block_is_a_sample(self):
        self.accept(("docs/a.md", "Prose.\n\n```markdown\n[sample](./never.md)\n```\n"))

    def test_a_link_inside_an_indented_code_block_is_a_sample(self):
        self.accept(("docs/a.md", "Prose.\n\n    [sample](./never.md)\n\nMore prose.\n"))

    def test_front_matter_is_not_prose(self):
        self.accept(("docs/a.md", "---\nsee: [x](./never.md)\n---\n\n# Title\n"))

    def test_an_opening_thematic_break_is_not_front_matter(self):
        # The other half of that rule: a document that opens with `---` and
        # never closes it must not lose its body, because silently dropping
        # every link in a file is a green run that checked nothing.
        self.write("docs/a.md", "---\n\n[gone](./never.md)\n")
        self.assert_code(self.run_gate(), BROKEN, "points at no file git tracks.")

    def test_a_duplicate_heading_takes_a_numbered_slug(self):
        self.accept(("docs/a.md", "## Notes\n\n## Notes\n\n[second](#notes-1)\n"))


class AdrLabelAgreesWithTarget(GateCase):
    """A cross-reference's two halves, and the renumbering that splits them.

    The live defect: ADR-0039 landed beside ADR-0038 and six links ended up
    reading `[ADR-0038]` while pointing at `0039-ios-distribution.md`. Every
    gate stayed green — this one because the file exists, `citation-gate.py`
    because a markdown link is not a code span — so the class had no check
    at all until the rule these cases pin.

    Most of what follows must NOT fire. The rule has 1,348 links to stay
    quiet on and six to catch, so the near-misses carry the weight.
    """

    def adr(
        self, *, label: str, target: str, body: str | None = None, adr39: str = "# 0039\n"
    ) -> subprocess.CompletedProcess:
        self.write("docs/11-adr/0038-macos-update-feed.md", "# 0038\n")
        self.write("docs/11-adr/0039-ios-distribution.md", adr39)
        self.write("docs/11-adr/README.md", "# Index\n")
        self.write("docs/07-clients/releasing.md", body or f"See [{label}]({target}).\n")
        return self.run_gate()

    def test_a_label_naming_a_different_adr_than_it_links_to_fails(self):
        result = self.adr(label="ADR-0038", target="../11-adr/0039-ios-distribution.md")
        self.assert_code(
            result,
            BROKEN,
            "::error file=docs/07-clients/releasing.md,line=1::docs-links: "
            "[ADR-0038](../11-adr/0039-ios-distribution.md) says ADR-0038 and links to ADR-0039.",
            "1 link(s) name one ADR and point at another.",
        )

    def test_the_message_names_both_numbers(self):
        # A reader fixes this by deciding which half is wrong, so the finding
        # has to print both halves rather than "mismatch".
        result = self.adr(label="ADR-0038", target="../11-adr/0039-ios-distribution.md")
        self.assertIn("says ADR-0038 and links to ADR-0039.", result.stdout)

    def test_an_adr_citing_itself_wrongly_fails(self):
        self.write("docs/11-adr/0038-macos-update-feed.md", "# 0038\n\nSee [ADR-0038](./0039-ios-distribution.md).\n")
        self.write("docs/11-adr/0039-ios-distribution.md", "# 0039\n")
        self.assert_code(self.run_gate(), BROKEN, "says ADR-0038 and links to ADR-0039.")

    def test_an_agreeing_pair_does_not_fire(self):
        self.assert_code(
            self.adr(label="ADR-0039", target="../11-adr/0039-ios-distribution.md"),
            CLEAN,
            "OK: docs-links clean.",
        )

    def test_a_fragment_does_not_hide_a_disagreement(self):
        # The discriminating case for stripping the fragment. Without it the
        # target no longer ends in `.md`, no ADR number is read out of it, and
        # the rule goes quiet on a real mismatch — which is a hole shaped
        # exactly like the one it was written to close.
        self.assert_code(
            self.adr(
                label="ADR-0038",
                target="../11-adr/0039-ios-distribution.md#decision",
                adr39="# 0039\n\n## Decision\n",
            ),
            BROKEN,
            "says ADR-0038 and links to ADR-0039.",
        )

    def test_a_fragment_on_an_agreeing_target_is_not_a_finding(self):
        self.assert_code(
            self.adr(
                label="ADR-0039",
                target="../11-adr/0039-ios-distribution.md#decision",
                adr39="# 0039\n\n## Decision\n",
            ),
            CLEAN,
            "OK: docs-links clean.",
        )

    def test_a_target_that_names_no_adr_is_not_a_finding(self):
        # The case that would make the rule useless if it fired: a label may
        # name an ADR while pointing at an index, a guide or an anchor.
        self.assert_code(self.adr(label="ADR-0038", target="../11-adr/README.md"), CLEAN, "OK: docs-links clean.")

    def test_an_anchor_only_target_is_not_a_finding(self):
        self.write("docs/07-clients/releasing.md", "## Heading\n\nSee [ADR-0038](#heading).\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: docs-links clean.")

    def test_a_third_party_url_with_no_adr_in_it_is_not_a_finding(self):
        self.assert_code(
            self.adr(label="ADR-0038", target="https://example.invalid/some/page"),
            CLEAN,
            "OK: docs-links clean.",
        )

    def test_a_label_with_no_adr_number_is_not_a_finding(self):
        # 54 links in this repository point at an ADR file under a prose
        # label. None of them is a claim about a number.
        self.assert_code(
            self.adr(label="the iOS distribution decision", target="../11-adr/0039-ios-distribution.md"),
            CLEAN,
            "OK: docs-links clean.",
        )

    def test_a_label_naming_several_adrs_passes_if_one_is_the_target(self):
        self.assert_code(
            self.adr(label="ADR-0038 and ADR-0039", target="../11-adr/0039-ios-distribution.md"),
            CLEAN,
            "OK: docs-links clean.",
        )

    def test_a_label_naming_several_adrs_fails_when_none_is_the_target(self):
        self.assert_code(
            self.adr(label="ADR-0037 and ADR-0038", target="../11-adr/0039-ios-distribution.md"),
            BROKEN,
            "says ADR-0037 and links to ADR-0039.",
        )

    def test_an_absolute_url_into_this_repository_is_still_compared(self):
        # The one rule here that reads an external link. Resolution cannot --
        # that is a network call and a red build for someone else's outage --
        # but the ADR number is in the last path segment, so comparing it is
        # free and offline. `release.yml` carried exactly this shape.
        self.assert_code(
            self.adr(
                label="ADR-0038",
                target="https://github.com/o/r/blob/v1/docs/11-adr/0039-ios-distribution.md",
            ),
            BROKEN,
            "says ADR-0038 and links to ADR-0039.",
        )

    def test_the_rule_reads_rust_doc_comments(self):
        self.write("docs/11-adr/0039-ios-distribution.md", "# 0039\n")
        self.write("docs/a.md", "Prose.\n")
        self.write("crates/c/src/lib.rs", "//! Per [ADR-0038](../../../docs/11-adr/0039-ios-distribution.md).\n")
        self.assert_code(self.run_gate(), BROKEN, "crates/c/src/lib.rs,line=1", "says ADR-0038 and links to ADR-0039.")

    def test_the_rule_reads_yaml(self):
        # `release.yml` carried one of the six in a job comment, which is why
        # this rule reaches past markdown at all.
        self.write("docs/11-adr/0039-ios-distribution.md", "# 0039\n")
        self.write("docs/a.md", "Prose.\n")
        self.write(".github/workflows/release.yml", "# See [ADR-0038](../../docs/11-adr/0039-ios-distribution.md).\non: push\n")
        self.assert_code(self.run_gate(), BROKEN, ".github/workflows/release.yml,line=1")

    def test_an_index_expression_in_rust_is_not_a_link(self):
        # `a[0](b)` matches the raw-text extractor. What keeps the rule off
        # code is the rule: it needs an ADR number on both sides.
        self.write("docs/a.md", "Prose.\n")
        self.write("crates/c/src/lib.rs", "fn f() { let _ = a[0](b); }\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: docs-links clean.")

    def test_legacy_is_not_read_for_this_rule_either(self):
        self.write("docs/11-adr/0039-ios-distribution.md", "# 0039\n")
        self.write("docs/a.md", "Prose.\n")
        self.write("legacy/app/build.rs", "// [ADR-0038](../../docs/11-adr/0039-ios-distribution.md)\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: docs-links clean.")

    def test_the_run_says_how_many_links_it_compared(self):
        result = self.adr(label="ADR-0039", target="../11-adr/0039-ios-distribution.md")
        self.assertIn("link(s) checked for a label that disagrees with its target.", result.stdout)

    def test_a_mislabelled_link_is_not_counted_as_unresolvable(self):
        # Two different defects with two different remedies: "the file is
        # gone" and "the label is wrong". Reporting one as the other sends
        # the reader to fix the wrong half.
        result = self.adr(label="ADR-0038", target="../11-adr/0039-ios-distribution.md")
        self.assertNotIn("resolve to nothing", result.stdout)


class InlineCodeIsBlanked(GateCase):
    """The masking this gate shares with `doc-comment-gate.py`, pinned.

    Both blank inline code before they read a line. That is what leaves a
    backticked `path:line` citation unchecked by either, and what
    `citation-gate.py` exists to read. Asserted here so the three do not
    start fighting over the same span.
    """

    def test_a_documented_link_inside_a_code_span_is_not_a_link(self):
        self.write("docs/a.md", "Write a relative link as `[label](./other.md)`.\n")
        result = self.run_gate()
        self.assert_code(result, CLEAN, "docs-links: 0 links across 1 markdown files.")

    def test_an_unquoted_link_on_the_same_line_still_counts(self):
        # The other half: masking must blank code spans and nothing else.
        # Without this, "blank everything" also passes.
        self.write("docs/a.md", "Write `[label](./x.md)` like [this](./other.md).\n")
        self.assert_code(
            self.run_gate(), BROKEN, "[this](./other.md) points at no file git tracks."
        )
        self.assert_absent(self.run_gate(), "[label]")

    def test_a_backticked_path_citation_is_not_this_gate_s_business(self):
        # It is masked here and checked by `citation-gate.py`. A dangling one
        # must leave this gate green, or the two are duplicating a judgement
        # they could disagree about.
        self.write("docs/a.md", "See `crates/sunrise-core/src/nonexistent.rs:1`.\n")
        self.assert_code(self.run_gate(), CLEAN, "docs-links: 0 links across 1 markdown files.")


class Unreadable(GateCase):
    """A link-shaped construct the scanner could not read is a warning."""

    def test_an_unbalanced_bracket_is_warned_about_and_not_counted(self):
        # A warning rather than a failure, and it has to say it was skipped:
        # silently dropping something link-shaped is the failure mode this
        # gate is for.
        self.write("docs/a.md", "An unreadable ](./b.md) shape.\n")
        result = self.run_gate()
        self.assert_code(
            result,
            CLEAN,
            "::warning file=docs/a.md,line=1::docs-links: could not read a link near",
            "it was NOT checked.",
            "1 link-shaped construct(s) could not be read and were not checked.",
        )


class CouldNotRun(GateCase):
    """Exit 2 is "nothing was scanned", and must not read as a verdict."""

    def test_a_tree_with_no_markdown_cannot_be_scanned(self):
        self.write("Cargo.toml", "[workspace]\n")
        self.assert_code(
            self.run_gate(), COULD_NOT_RUN, "git reports no tracked markdown; the gate could not run."
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
        self.assertIn("`git ls-files` failed", result.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
