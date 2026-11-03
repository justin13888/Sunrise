#!/usr/bin/env python3
"""What `grep-gate.sh` reports, and when it refuses to report anything.

Why this file exists
--------------------

`grep-gate.sh` is the shared runner behind the determinism, log-redaction and
constant-time gates. Its header states its contract precisely: grep's exit 1
("no match") must be told apart from 2 ("bad path or pattern") and 127 ("not
installed"), comment lines never count as hits, and three allowlist states
fail the gate - an entry with an empty field, an entry that matches no hit,
and an allowlist file that does not exist. Every one of those is a way for a
gate to answer 0 without having checked anything, which is how both of this
repository's original grep gates shipped. The gates that call the runner pin
their own patterns; this file pins the runner itself.

Every case runs the script on a source tree synthesised in a temp directory,
with its cwd there, so no case reads the repository's own source.

Run it with `mise run gate-contracts`, or directly:
`python3 .github/scripts/test_grep_gate.py`.
"""

from __future__ import annotations

import pathlib
import shutil
import subprocess
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
GATE = HERE / "grep-gate.sh"
BASH = shutil.which("bash") or "/bin/bash"

NAME = "banned-call"
PATTERN = r"banned\("
SRC = "src"
LIB = "src/lib.rs"

HIT = "fn f() {\n    banned();\n}\n"
CLEAN = "fn f() {\n    allowed();\n}\n"


class GateCase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.tmp = pathlib.Path(self._tmp.name)
        (self.tmp / SRC).mkdir()
        self.allowlist = self.tmp / "allow.tsv"

    def write(self, rel: str, text: str) -> None:
        path = self.tmp / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def allow(self, *entries: tuple[str, str, str]) -> None:
        lines = ["# fixture", ""] + ["\t".join(entry) for entry in entries]
        self.allowlist.write_text("\n".join(lines) + "\n")

    def run_gate(
        self,
        *dirs: str,
        allowlist: bool = False,
        pattern: str = PATTERN,
        env: dict[str, str] | None = None,
    ):
        args = [BASH, str(GATE)]
        if allowlist:
            args += ["--allowlist", str(self.allowlist)]
        args += [NAME, pattern, *(dirs or (SRC,))]
        return subprocess.run(
            args, cwd=self.tmp, env=env, capture_output=True, text=True
        )

    def assert_exit(self, result, code: int) -> None:
        self.assertEqual(
            result.returncode,
            code,
            f"exit {result.returncode}:\n{result.stdout}{result.stderr}",
        )


class Detection(GateCase):
    def test_a_hit_is_a_violation(self):
        self.write(LIB, HIT)
        result = self.run_gate()
        self.assert_exit(result, 1)
        self.assertIn(f"{LIB}:2:    banned();", result.stdout)
        self.assertIn(f"::error::{NAME}: violations listed above.", result.stdout)

    def test_no_hit_is_clean(self):
        self.write(LIB, CLEAN)
        result = self.run_gate()
        self.assert_exit(result, 0)
        self.assertIn(f"OK: {NAME} clean (searched: {SRC}).", result.stdout)

    def test_comment_lines_are_not_hits(self):
        self.write(
            LIB,
            "// banned() is a line comment\n"
            "/* banned() opens a block comment\n"
            " * banned() continues it\n"
            "    /// banned() is a doc comment\n"
            "fn f() {}\n",
        )
        self.assert_exit(self.run_gate(), 0)

    def test_a_trailing_comment_does_not_hide_a_hit(self):
        self.write(LIB, "fn f() { banned(); } // banned() here too\n")
        self.assert_exit(self.run_gate(), 1)

    def test_only_rust_sources_are_searched(self):
        self.write("src/notes.md", "banned()\n")
        self.write(LIB, CLEAN)
        self.assert_exit(self.run_gate(), 0)

    def test_every_named_dir_is_searched(self):
        self.write(LIB, CLEAN)
        self.write("other/x.rs", HIT)
        result = self.run_gate(SRC, "other")
        self.assert_exit(result, 1)
        self.assertIn("other/x.rs:2:", result.stdout)


class Refusals(GateCase):
    def test_no_existing_dir_is_not_a_pass(self):
        result = self.run_gate("missing", "also-missing")
        self.assert_exit(result, 1)
        self.assertIn("none of the search paths exist", result.stdout)

    def test_a_missing_dir_beside_an_existing_one_is_skipped(self):
        self.write(LIB, HIT)
        result = self.run_gate("missing", SRC)
        self.assert_exit(result, 1)
        self.assertIn(f"{LIB}:2:", result.stdout)
        self.write(LIB, CLEAN)
        result = self.run_gate("missing", SRC)
        self.assert_exit(result, 0)
        self.assertIn(f"(searched: {SRC})", result.stdout)

    def test_a_grep_error_is_not_a_pass(self):
        # An unbalanced group is an invalid ERE: grep exits 2, not 1.
        self.write(LIB, CLEAN)
        result = self.run_gate(pattern="banned(")
        self.assert_exit(result, 1)
        self.assertIn("grep exited 2; the gate could not run.", result.stdout)

    def test_grep_not_installed_is_not_a_pass(self):
        # bash is invoked by absolute path; with PATH at an empty directory
        # nothing else resolves, so grep exits 127.
        empty = self.tmp / "empty-bin"
        empty.mkdir()
        self.write(LIB, CLEAN)
        result = self.run_gate(env={"PATH": str(empty)})
        self.assert_exit(result, 1)
        self.assertIn("grep exited 127; the gate could not run.", result.stdout)


class Allowlist(GateCase):
    def test_an_entry_excuses_its_trimmed_line(self):
        self.write(LIB, HIT)
        self.allow((LIB, "banned();", "fixture: the call is the documented exception"))
        result = self.run_gate(allowlist=True)
        self.assert_exit(result, 0)
        self.assertIn(f"OK: {NAME} clean, 1 allowlisted", result.stdout)

    def test_an_entry_does_not_excuse_the_same_line_in_another_file(self):
        self.write(LIB, HIT)
        self.write("src/other.rs", HIT)
        self.allow((LIB, "banned();", "fixture"))
        result = self.run_gate(allowlist=True)
        self.assert_exit(result, 1)
        self.assertIn("src/other.rs:2:", result.stdout)
        self.assertNotIn(f"{LIB}:2:", result.stdout)

    def test_a_stale_entry_is_a_violation(self):
        self.write(LIB, CLEAN)
        self.allow((LIB, "banned();", "excuses a line that is gone"))
        result = self.run_gate(allowlist=True)
        self.assert_exit(result, 1)
        self.assertIn("allowlist entries above match no line; delete them.", result.stdout)

    def test_an_entry_with_an_empty_field_is_refused(self):
        self.write(LIB, HIT)
        entries = {
            "empty path": f"\tbanned();\treason",
            "blank line text": f"{LIB}\t  \treason",
            "empty reason": f"{LIB}\tbanned();\t",
            "blank reason": f"{LIB}\tbanned();\t \t",
            "two fields": f"{LIB}\tbanned();",
            "four fields": f"{LIB}\tbanned();\treason\textra",
        }
        for label, line in entries.items():
            with self.subTest(label):
                self.allowlist.write_text(line + "\n")
                result = self.run_gate(allowlist=True)
                self.assert_exit(result, 1)
                self.assertIn("not path<TAB>line<TAB>reason", result.stdout)

    def test_a_missing_allowlist_is_not_a_pass(self):
        self.write(LIB, CLEAN)
        result = self.run_gate(allowlist=True)
        self.assert_exit(result, 1)
        self.assertIn("does not exist; the gate cannot run.", result.stdout)

    def test_comment_and_blank_lines_are_not_entries(self):
        self.write(LIB, CLEAN)
        self.allowlist.write_text("# a comment\n\n   # an indented comment\n   \n")
        result = self.run_gate(allowlist=True)
        self.assert_exit(result, 0)
        self.assertIn("clean, 0 allowlisted", result.stdout)

    def test_an_allowlist_with_no_entries_excuses_nothing(self):
        # A wholly empty file is the case an `NR == FNR` split gets wrong: the
        # first file contributes no records, so every hit reads as an entry.
        self.write(LIB, HIT)
        self.allowlist.write_text("")
        result = self.run_gate(allowlist=True)
        self.assert_exit(result, 1)
        self.assertIn(f"{LIB}:2:", result.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
