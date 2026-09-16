#!/usr/bin/env python3
r"""The exit-code contract of `observability-catalog-gate.py`, as assertions.

Why this file exists
--------------------

This gate is unusual: it does not carry its own pattern, it *reads the
extraction command out of the document* and runs it. That is what keeps the
gate and the document from disagreeing, and it is also a second surface that
can rot — a marker that loses its `grep` line, a path that stops being a
directory, a `Last extracted` ref that names no commit. Each of those has
its own route to a red check, and the gate's own `--self-test` reaches none
of them: it runs `parse_blocks` and `compare` over one string literal and
never touches a tree, a repository or an exit code.

Both directions are here. `Rejects` pins every route to exit 1 —
undocumented name, retired name, wrong count, missing count sentence,
missing provenance, dead provenance, unusable marker, wrong number of
blocks. `Accepts` pins what the comparison must ignore: the block's
two-column layout, its parenthesised asides, prose elsewhere in the
document, and a name that appears in a file the grep does not read. The
asides in particular are load-bearing — `sunrise_push_apns_total` carries
one today — and a gate that read them as names would be red on the tree it
was written against.

Fixtures are synthesised documents
----------------------------------

Every case builds a git repository holding one
`docs/06-server/observability.md` and a small source tree, and drives the
gate with `--root`. The repository gets one real commit, because provenance
resolution asks git whether the `Last extracted` ref names a commit and
skips itself in a shallow clone — a fixture that could not answer would
retire half of what this gate does without failing.

Run it with `python3 .github/scripts/test_observability_catalog_gate.py`.
"""

from __future__ import annotations

import pathlib
import subprocess
import sys
import tempfile
import unittest

GATE = pathlib.Path(__file__).resolve().parent / "observability-catalog-gate.py"

CLEAN = 0
VIOLATION = 1
COULD_NOT_RUN = 2

DOC = "docs/06-server/observability.md"
SOURCE_DIR = "src"

EVENT_MARKER = (
    "<!-- Extracted from the tree; do not edit by hand. Re-run and reconcile:\n"
    "     grep -rhoE 'ev = \"srv\\.[a-z0-9_.]+\"' {path} | sort -u\n"
    "{provenance} -->"
)
METRIC_MARKER = (
    "<!-- Extracted from the tree; do not edit by hand. Re-run and reconcile:\n"
    "     grep -rhoE '\"sunrise_[a-z0-9_]+\"' {path} | sort -u\n"
    "{provenance} -->"
)

# The source the document's own command is run over.
SOURCE = '''
pub fn start() {
    tracing::info!(ev = "srv.start", "up");
    tracing::info!(ev = "srv.stop", "down");
    counter("sunrise_a_total");
    counter("sunrise_b_total");
}
'''


class GateCase(unittest.TestCase):
    """One temp repository per test, holding one document and one source tree."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = pathlib.Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)
        self.repo = self.tmp / "repo"
        self.repo.mkdir()
        self._git("init", "-q")
        self._git("config", "user.email", "gate@example.invalid")
        self._git("config", "user.name", "Gate")
        self.write(f"{SOURCE_DIR}/lib.rs", SOURCE)
        self._git("commit", "-qm", "fixture")
        self.head = subprocess.run(
            ["git", "-C", str(self.repo), "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()[:7]

    def _git(self, *args: str) -> None:
        subprocess.run(
            ["git", "-C", str(self.repo), *args], check=True, capture_output=True, text=True
        )

    def write(self, rel: str, text: str) -> None:
        path = self.repo / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        self._git("add", "--", rel)

    def document(
        self,
        *,
        ev_count: str = "2",
        ev_names: str = "srv.start                        srv.stop",
        ev_marker: str | None = None,
        metric_count: str = "2",
        metric_names: str = "sunrise_a_total\nsunrise_b_total",
        metric_marker: str | None = None,
        path: str = SOURCE_DIR,
        ref: str | None = None,
        tail: str = "",
    ) -> None:
        """Write a document in the shape `observability.md` really has."""
        provenance = "     Last extracted: {ref}".format(ref=self.head if ref is None else ref)
        if ref == "":
            provenance = "     (no provenance)"
        first = EVENT_MARKER if ev_marker is None else ev_marker
        second = METRIC_MARKER if metric_marker is None else metric_marker
        self.write(
            DOC,
            "# Observability\n\n"
            f"The {ev_count} `ev` names the server emits, complete:\n\n"
            + first.format(path=path, provenance=provenance)
            + f"\n\n```\n{ev_names}\n```\n\n"
            f"{metric_count} metric names, and one an earlier revision listed:\n\n"
            + second.format(path=path, provenance=provenance)
            + f"\n\n```\n{metric_names}\n```\n{tail}",
        )

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
    """The parse and compare rules, asserted before the gate reads a tree."""

    def test_the_self_test_passes_alone(self):
        result = subprocess.run(
            [sys.executable, str(GATE), "--self-test"], capture_output=True, text=True
        )
        self.assertEqual(result.returncode, CLEAN, result.stdout + result.stderr)
        self.assertIn("OK: observability-catalog self-test clean", result.stdout)

    def test_the_self_test_runs_as_a_precondition_of_a_check(self):
        self.document()
        self.assertIn("OK: observability-catalog self-test clean", self.run_gate().stdout)


class Clean(GateCase):
    """What a passing run says, and that it says both tallies."""

    def test_two_blocks_that_agree_with_the_tree_exit_zero(self):
        self.document()
        self.assert_code(
            self.run_gate(),
            CLEAN,
            "observability-catalog: event block — 2 listed, 2 in `src`.",
            "observability-catalog: metric block — 2 listed, 2 in `src`.",
            "OK: observability-catalog clean.",
        )


class Rejects(GateCase):
    """Every route to exit 1, each with the sentence it prints."""

    def test_a_name_in_the_tree_and_not_in_the_block(self):
        # The live drift: `sunrise_relay_batch_duplicate_total` reached the
        # code and the catalogue still read complete.
        self.document(metric_count="1", metric_names="sunrise_a_total")
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            "`sunrise_b_total` is in the tree and not in the metric block.",
            "move `Last extracted` to the commit you reconciled against",
        )

    def test_a_name_in_the_block_and_not_in_the_tree(self):
        # The other direction: a catalogue still advertising something the
        # source dropped. One diff, both ways, or half the rot is invisible.
        self.document(metric_count="3", metric_names="sunrise_a_total\nsunrise_b_total\nsunrise_gone_total")
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            "`sunrise_gone_total` is in the metric block and not in the tree.",
        )

    def test_a_count_sentence_that_disagrees_with_its_block(self):
        # What actually rotted: the reader takes the block as complete on the
        # numeral alone, so the numeral is checked.
        self.document(ev_count="24")
        self.assert_code(
            self.run_gate(), VIOLATION, "the prose says 24 event names and the block lists 2."
        )

    def test_a_missing_count_sentence(self):
        self.write(
            DOC,
            "# Observability\n\nNo count here.\n\n"
            + EVENT_MARKER.format(path=SOURCE_DIR, provenance=f"     Last extracted: {self.head}")
            + "\n\n```\nsrv.start\nsrv.stop\n```\n\n"
            "2 metric names, and one more:\n\n"
            + METRIC_MARKER.format(path=SOURCE_DIR, provenance=f"     Last extracted: {self.head}")
            + "\n\n```\nsunrise_a_total\nsunrise_b_total\n```\n",
        )
        self.assert_code(
            self.run_gate(), VIOLATION, "no sentence states how many event names the block holds."
        )

    def test_a_marker_with_no_provenance(self):
        self.document(ref="")
        self.assert_code(
            self.run_gate(), VIOLATION, "the marker carries no `Last extracted:` commit."
        )

    def test_provenance_that_names_no_commit(self):
        # Without this the ref decays into a decorative string, which is what
        # it was before the gate existed.
        self.document(ref="deadbee")
        self.assert_code(
            self.run_gate(), VIOLATION, "`Last extracted: deadbee` names no commit in this repository."
        )

    def test_a_grep_path_that_is_not_a_directory(self):
        self.document(path="nowhere")
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            "the marker greps `nowhere`, which is not a directory.",
        )

    def test_a_marker_that_names_no_extraction_command(self):
        self.document(
            ev_marker="<!-- Extracted from the tree; do not edit by hand.\n{provenance} -->",
        )
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            "expected the two extracted blocks",
            "found 1",
            "names no `grep -rhoE '<pattern>' <path>` command",
        )

    def test_a_marker_with_no_fenced_block_after_it(self):
        self.write(
            DOC,
            "# Observability\n\nThe 2 `ev` names the server emits, complete:\n\n"
            + EVENT_MARKER.format(path=SOURCE_DIR, provenance=f"     Last extracted: {self.head}")
            + "\n\nProse where the block should be.\n",
        )
        self.assert_code(
            self.run_gate(), VIOLATION, "no fenced block follows the extraction marker."
        )

    def test_a_third_marked_block_has_to_be_named_here(self):
        # Two is the number this gate knows how to label. A third arriving
        # unannounced must be a red check rather than an unchecked block.
        self.document(
            tail="\nAnd a third:\n\n"
            + METRIC_MARKER.format(path=SOURCE_DIR, provenance="     Last extracted: 0123abc")
            + "\n\n```\nsunrise_a_total\n```\n"
        )
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            "expected the two extracted blocks",
            "found 3",
            "a third arrived and needs naming here",
        )


class Accepts(GateCase):
    """What the comparison must ignore. A gate red on its own tree is useless."""

    def test_a_parenthesised_aside_beside_a_name(self):
        # `sunrise_push_apns_total` carries one today: defined and never
        # reached, said in the block itself.
        self.document(
            metric_names="sunrise_a_total\nsunrise_b_total          (LoggingProvider; never reached)"
        )
        self.assert_code(self.run_gate(), CLEAN, "OK: observability-catalog clean.")

    def test_the_blocks_two_column_layout(self):
        self.document(metric_names="sunrise_a_total          sunrise_b_total")
        self.assert_code(self.run_gate(), CLEAN, "OK: observability-catalog clean.")

    def test_a_name_in_the_documents_prose_is_not_in_the_block(self):
        # Only the fenced block is the catalogue. Naming a metric in a
        # sentence must not silently enrol it.
        self.document(tail="\nThe `sunrise_gone_total` counter was removed in 2025.\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: observability-catalog clean.")

    def test_a_name_in_a_file_the_grep_does_not_read(self):
        # The extraction reads `.rs` files under the named path, which is
        # what the document's own `grep -r` does.
        self.write(f"{SOURCE_DIR}/notes.md", 'A note about "sunrise_gone_total".\n')
        self.document()
        self.assert_code(self.run_gate(), CLEAN, "OK: observability-catalog clean.")

    def test_a_name_outside_the_path_the_marker_greps(self):
        self.write("elsewhere/other.rs", 'counter("sunrise_gone_total");\n')
        self.document()
        self.assert_code(self.run_gate(), CLEAN, "OK: observability-catalog clean.")


class CouldNotRun(GateCase):
    """Exit 2 is "nothing was read", and must not read as a verdict."""

    def test_a_missing_document_is_a_failed_run(self):
        self.assert_code(
            self.run_gate(),
            COULD_NOT_RUN,
            f"cannot read {DOC}",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
