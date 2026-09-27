#!/usr/bin/env python3
"""What `reverses-gate.py` reports, through the process boundary CI uses.

Why this file exists
--------------------

The gate exists for #300: two pull requests merged bodies whose `Reverses:`
line said what undoing a decision would do - "close #282 as wontfix", "close
#297 as delivered here" - and GitHub closed both issues on merge. The two
bodies' lines are planted below verbatim and must stay red.

A gate over prose goes quietly green the moment its reading of the prose is
wrong, and quietly red on a body that was fine, so both directions are
asserted: the shapes that closed issues (a bold `**Reverses.**`, a line
backticked end to end, a field that wraps before the number) exit 1, and the
shapes #300 names as safe (a fenced record, a markdown link, the number away
from the keyword, an intended `Closes #N` on its own line) exit 0.

Each case runs the gate as a subprocess, either on a body file or on a
synthesised event payload through `GITHUB_EVENT_PATH`, the two ways it reads a
body.

Run it with `mise run reverses-gate-test`, or directly:
`python3 .github/scripts/test_reverses_gate.py`.
"""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest

GATE = pathlib.Path(__file__).resolve().parent / "reverses-gate.py"

CLEAN = 0
CLOSES_AN_ISSUE = 1
CANNOT_RUN = 2

# PR #240's body, line 114.
PR_240 = "**Reverses.** Revert decision 11's wording and close #282 as wontfix.\n"
# PR #293's body, line 432.
PR_293 = "`Reverses: fold #297's five sites into this branch and close #297 as delivered here.`\n"


def record(reverses: str, *, taken: str = "keep it - fine", rejected: str = "drop it - no") -> str:
    """A body with one decision-record entry in the house shape."""
    return (
        "Closes #300\n"
        "\n"
        "## Decisions taken\n"
        "\n"
        "1. Whether to keep the thing.\n"
        f"   Taken:    {taken}\n"
        f"   Rejected: {rejected}\n"
        f"   Reverses: {reverses}\n"
    )


class GateCase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = pathlib.Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)

    def run_gate(self, *args: str, env: dict[str, str] | None = None, stdin: str | None = None):
        base = {k: v for k, v in os.environ.items() if k != "GITHUB_EVENT_PATH"}
        base.update(env or {})
        return subprocess.run(
            [sys.executable, str(GATE), *args],
            capture_output=True,
            text=True,
            env=base,
            input=stdin,
            cwd=self.tmp,
        )

    def on_body(self, body: str):
        path = self.tmp / "body.md"
        path.write_text(body, encoding="utf-8")
        return self.run_gate(str(path))

    def assertExit(self, result, code: int) -> None:
        self.assertEqual(
            result.returncode,
            code,
            f"expected exit {code}, got {result.returncode}\n{result.stdout}{result.stderr}",
        )


class TheTwoCasualties(GateCase):
    def test_pr_240_bold_reverses_with_a_period(self) -> None:
        result = self.on_body("Closes #230\n\n" + PR_240)
        self.assertExit(result, CLOSES_AN_ISSUE)
        self.assertIn("#282", result.stdout)
        self.assertIn("body line 3", result.stdout)

    def test_pr_293_backticked_line_is_still_read(self) -> None:
        result = self.on_body("Closes #274\n\n" + PR_293)
        self.assertExit(result, CLOSES_AN_ISSUE)
        self.assertIn("close #297", result.stdout)


class ClosingShapes(GateCase):
    def test_every_keyword(self) -> None:
        for keyword in ("close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved"):
            for spelled in (keyword, keyword.upper(), keyword.capitalize()):
                with self.subTest(keyword=spelled):
                    self.assertExit(self.on_body(record(f"revert it and {spelled} #12")), CLOSES_AN_ISSUE)

    def test_each_field_label(self) -> None:
        self.assertExit(self.on_body(record("revert", taken="close #9 now")), CLOSES_AN_ISSUE)
        self.assertExit(self.on_body(record("revert", rejected="fixes #9 - no")), CLOSES_AN_ISSUE)

    def test_label_spellings(self) -> None:
        for line in (
            "Reverses: close #5",
            "reverses: close #5",
            "- Reverses: close #5",
            "3. Reverses: close #5",
            "> Reverses: close #5",
            "**Reverses**: close #5",
            "**Reverses:** close #5",
            "_Reverses._ close #5",
            "`Reverses`: close #5",
        ):
            with self.subTest(line=line):
                self.assertExit(self.on_body(line + "\n"), CLOSES_AN_ISSUE)

    def test_reference_spellings(self) -> None:
        for ref in (
            "#5",
            "justin13888/Sunrise#5",
            "https://github.com/justin13888/Sunrise/issues/5",
            "**#5**",
            "`#5`",
        ):
            with self.subTest(ref=ref):
                self.assertExit(self.on_body(record(f"revert and close {ref}")), CLOSES_AN_ISSUE)

    def test_colon_after_keyword(self) -> None:
        self.assertExit(self.on_body(record("revert, Closes: #5")), CLOSES_AN_ISSUE)

    def test_hyphenated_keyword_counts(self) -> None:
        # #300 offers `re-close #N` as safe; nothing documents that GitHub
        # agrees, so the gate does not rely on it.
        self.assertExit(self.on_body(record("revert and re-close #5")), CLOSES_AN_ISSUE)

    def test_field_wrapping_before_the_number(self) -> None:
        body = record("revert the wording and close") + "   #282 as wontfix.\n"
        result = self.on_body(body)
        self.assertExit(result, CLOSES_AN_ISSUE)
        self.assertIn("body line 8", result.stdout)

    def test_every_finding_is_reported(self) -> None:
        body = record("close #1 and fix #2", rejected="resolves #3")
        result = self.on_body(body)
        self.assertExit(result, CLOSES_AN_ISSUE)
        self.assertIn("3 closing reference(s)", result.stdout)

    def test_after_a_closed_fence_the_record_is_read_again(self) -> None:
        body = "```\nReverses: close #1\n```\n\nReverses: close #2\n"
        result = self.on_body(body)
        self.assertExit(result, CLOSES_AN_ISSUE)
        self.assertNotIn("#1", result.stdout)
        self.assertIn("#2", result.stdout)

    def test_a_short_fence_does_not_close_a_long_one(self) -> None:
        body = "````\n```\n````\nReverses: close #2\n"
        self.assertExit(self.on_body(body), CLOSES_AN_ISSUE)


class SafeShapes(GateCase):
    def test_empty_body(self) -> None:
        self.assertExit(self.on_body(""), CLEAN)

    def test_intended_close_on_its_own_line(self) -> None:
        self.assertExit(self.on_body("Closes #300\n\nFixes #301.\n"), CLEAN)

    def test_record_without_a_closing_reference(self) -> None:
        self.assertExit(self.on_body(record("reopen #282 and revert decision 11")), CLEAN)

    def test_fenced_record(self) -> None:
        for fence in ("```", "~~~", "````text"):
            closer = fence.rstrip("text")
            with self.subTest(fence=fence):
                body = f"Closes #1\n\n{fence}\n{PR_240}{PR_293}{closer}\n"
                self.assertExit(self.on_body(body), CLEAN)

    def test_indented_fence_inside_a_list_item(self) -> None:
        body = "1. Fork.\n   ```\n   Reverses: close #5\n   ```\n"
        self.assertExit(self.on_body(body), CLEAN)

    def test_unclosed_fence_runs_to_the_end(self) -> None:
        self.assertExit(self.on_body("```\nReverses: close #5\n"), CLEAN)

    def test_markdown_link(self) -> None:
        body = record("revert and close [#297](https://github.com/justin13888/Sunrise/issues/297)")
        self.assertExit(self.on_body(body), CLEAN)

    def test_number_away_from_the_keyword(self) -> None:
        self.assertExit(self.on_body(record("revert, and close it (#297) as delivered")), CLEAN)

    def test_words_that_merely_contain_a_keyword(self) -> None:
        self.assertExit(self.on_body(record("undo the hotfix #5, prefix #6, disclosed #7")), CLEAN)

    def test_prose_outside_the_record(self) -> None:
        body = "This reverses nothing; it would close #5 if the record said so.\n"
        self.assertExit(self.on_body(body), CLEAN)

    def test_blank_line_ends_a_field(self) -> None:
        body = record("revert the wording") + "\nThe tracking issue is closed #5 elsewhere.\n"
        self.assertExit(self.on_body(body), CLEAN)

    def test_next_entry_ends_a_field(self) -> None:
        body = "1. Fork.\n   Reverses: revert\n2. Close #5 separately.\n"
        self.assertExit(self.on_body(body), CLEAN)

    def test_stdin(self) -> None:
        self.assertExit(self.run_gate("-", stdin=PR_240), CLOSES_AN_ISSUE)
        self.assertExit(self.run_gate("-", stdin="Closes #1\n"), CLEAN)


class EventPayload(GateCase):
    def payload(self, event: object) -> dict[str, str]:
        path = self.tmp / "event.json"
        path.write_text(json.dumps(event), encoding="utf-8")
        return {"GITHUB_EVENT_PATH": str(path)}

    def test_body_from_the_pull_request_event(self) -> None:
        env = self.payload({"action": "edited", "pull_request": {"number": 1, "body": PR_293}})
        self.assertExit(self.run_gate(env=env), CLOSES_AN_ISSUE)
        env = self.payload({"action": "opened", "pull_request": {"number": 1, "body": "Closes #1"}})
        self.assertExit(self.run_gate(env=env), CLEAN)

    def test_null_body_is_empty(self) -> None:
        env = self.payload({"pull_request": {"number": 1, "body": None}})
        self.assertExit(self.run_gate(env=env), CLEAN)

    def test_not_a_pull_request_event(self) -> None:
        self.assertExit(self.run_gate(env=self.payload({"ref": "refs/heads/master"})), CANNOT_RUN)
        self.assertExit(self.run_gate(env=self.payload(["not", "an", "object"])), CANNOT_RUN)

    def test_no_payload(self) -> None:
        self.assertExit(self.run_gate(), CANNOT_RUN)

    def test_unreadable_payload(self) -> None:
        self.assertExit(self.run_gate(env={"GITHUB_EVENT_PATH": str(self.tmp / "missing.json")}), CANNOT_RUN)
        bad = self.tmp / "bad.json"
        bad.write_text("{not json", encoding="utf-8")
        self.assertExit(self.run_gate(env={"GITHUB_EVENT_PATH": str(bad)}), CANNOT_RUN)

    def test_unreadable_body_file_and_extra_arguments(self) -> None:
        self.assertExit(self.run_gate(str(self.tmp / "missing.md")), CANNOT_RUN)
        self.assertExit(self.run_gate("a", "b"), CANNOT_RUN)


if __name__ == "__main__":
    unittest.main(verbosity=2)
