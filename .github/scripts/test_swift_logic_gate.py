#!/usr/bin/env python3
"""The exit-code contract of `swift-logic-gate.py`, as assertions.

Why this file exists
--------------------

The gate is a ratchet: Swift under apps/apple that holds logic must be on its
allowlist, and the allowlist may only shrink. Both halves can rot quietly. A
classifier that stopped seeing a `class` would let new logic in with the gate
green; a staleness check that stopped firing would let the list stop
shrinking. Each case here builds a small apps/apple tree in a temp directory
and drives a copy of the gate at it with `--root`, with ALLOWED rewritten to
the case's own entries — the way `test_orphan_crate_gate.py` rewrites its
lists — so the logic under test is the shipped logic, read from the shipped
file at run time. `ShippedList` then runs the gate unmodified against a tree
built from the entries it declares.

Run it with `mise run swift-logic-gate-test`, or directly.
"""

from __future__ import annotations

import importlib.util
import pathlib
import re
import subprocess
import sys
import tempfile
import unittest

GATE = pathlib.Path(__file__).resolve().parent / "swift-logic-gate.py"

OK = 0
VIOLATION = 1
# `raise SystemExit(str)` exits 1 with the message on stderr.
COULD_NOT_RUN = 1

VIEW = "import SwiftUI\n\nstruct {name}: View {{\n    var body: some View {{ Text(\"x\") }}\n}}\n"
MODEL = "import Foundation\n\nfinal class {name} {{\n    var count = 0\n}}\n"


def _load_gate_module():
    spec = importlib.util.spec_from_file_location("swift_logic_gate", GATE)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class GateCase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = pathlib.Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)
        self.root = self.tmp / "repo"

    def file(self, rel: str, text: str) -> str:
        path = self.root / "apps/apple" / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        return f"apps/apple/{rel}"

    def source(self, allowed: dict) -> pathlib.Path:
        text = GATE.read_text()
        pattern = re.compile(r"^ALLOWED: dict\[str, str\] = \{.*?^\}$", re.S | re.M)
        self.assertRegex(text, pattern, "ALLOWED table not found in the gate")
        text = pattern.sub(lambda _m: f"ALLOWED: dict = {allowed!r}", text, count=1)
        copy = self.tmp / "gate.py"
        copy.write_text(text)
        return copy

    def run_gate(self, allowed: dict | None = None, *, shipped: bool = False, extra=()):
        gate = GATE if shipped else self.source(allowed or {})
        return subprocess.run(
            [sys.executable, str(gate), "--root", str(self.root), *extra],
            capture_output=True,
            text=True,
            check=False,
        )

    def assert_code(self, result, expected: int, *fragments: str) -> None:
        output = result.stdout + result.stderr
        self.assertEqual(result.returncode, expected, f"exit {result.returncode}:\n{output}")
        for fragment in fragments:
            self.assertIn(fragment, output)


class Views(GateCase):
    """Trees that hold nothing but presentation."""

    def test_views_and_their_extensions_are_clean(self):
        self.file("Sunrise/Views/A.swift", VIEW.format(name="AView"))
        self.file(
            "Sunrise/Views/B.swift",
            "import SwiftUI\n\n@MainActor\nstruct BView: View {\n    var body: some View { EmptyView() }\n}\n\n"
            "extension AView {\n    func helper() {}\n}\n\nextension View {\n    func card() -> some View { self }\n}\n",
        )
        self.file(
            "Sunrise/App.swift",
            "import SwiftUI\n\n@main\nstruct TheApp: App {\n    var body: some Scene { WindowGroup { AView() } }\n}\n",
        )
        result = self.run_gate()
        self.assert_code(result, OK, "3 Swift file(s) scanned; 0 hold logic", "OK: swift-logic clean.")

    def test_tests_and_ui_test_support_are_not_scanned(self):
        self.file("Sunrise/Views/A.swift", VIEW.format(name="AView"))
        for directory in ("SunriseTests", "SunriseUITests", "SunriseiOSUITests", "UITestSupport"):
            self.file(f"{directory}/Thing.swift", MODEL.format(name="Thing"))
        result = self.run_gate()
        self.assert_code(result, OK, "1 Swift file(s) scanned")


class Logic(GateCase):
    """Each shape the heuristic must call logic."""

    def test_a_model_not_on_the_list_fails_and_names_the_file(self):
        path = self.file("Sunrise/Tasks/Model.swift", MODEL.format(name="Model"))
        result = self.run_gate()
        self.assert_code(result, VIOLATION, f"`{path}` holds logic that is not a view's")

    def test_every_logic_shape_is_seen(self):
        shapes = {
            "Struct": "struct Plan {\n    let a: Int\n}\n",
            "Enum": "enum Mode {\n    case on\n}\n",
            "Actor": "actor Bridge {}\n",
            "Protocol": "protocol Store {}\n",
            "Free": "func decide() -> Bool { true }\n",
            "Global": "let budgetMs: UInt64 = 1\n",
            "ModelExtension": "extension Model {\n    func more() {}\n}\n",
            "Attributed": "@MainActor\n@Observable\nfinal class Watched {}\n",
            "Modified": "private nonisolated enum Hidden {}\n",
        }
        paths = [self.file(f"Sunrise/{name}.swift", text) for name, text in shapes.items()]
        result = self.run_gate(extra=["--list"])
        self.assert_code(result, OK)
        listed = [line.split("\t")[0] for line in result.stdout.splitlines()]
        self.assertEqual(listed, sorted(paths))

    def test_a_view_file_with_a_helper_type_is_logic(self):
        path = self.file("Sunrise/Views/A.swift", VIEW.format(name="AView") + "\nenum Choice {\n    case a\n}\n")
        result = self.run_gate(extra=["--list"])
        self.assertIn(f"{path}\tenum Choice", result.stdout)

    def test_an_allowed_file_passes(self):
        path = self.file("Sunrise/Tasks/Model.swift", MODEL.format(name="Model"))
        result = self.run_gate({path: "platform IO"})
        self.assert_code(result, OK, "1 hold logic; 1 allowed", "OK: swift-logic clean.")


class Shrinks(GateCase):
    """The list may only shrink."""

    def test_an_allowed_file_that_holds_no_logic_now_is_stale(self):
        path = self.file("Sunrise/Views/A.swift", VIEW.format(name="AView"))
        result = self.run_gate({path: "moving"})
        self.assert_code(result, VIOLATION, f"`{path}` is allowed to hold logic but holds no logic now")

    def test_an_allowed_file_that_is_gone_is_stale(self):
        self.file("Sunrise/Views/A.swift", VIEW.format(name="AView"))
        result = self.run_gate({"apps/apple/Sunrise/Gone.swift": "moving"})
        self.assert_code(result, VIOLATION, "is allowed to hold logic but no longer exists")


class ShippedList(GateCase):
    """The unmodified gate, against a tree built from its own entries."""

    def setUp(self) -> None:
        super().setUp()
        self.listed = sorted(_load_gate_module().ALLOWED)
        self.assertTrue(self.listed, "the gate allows nothing at all")

    def test_a_tree_holding_logic_in_every_listed_file_is_clean(self):
        for i, path in enumerate(self.listed):
            self.file(path.removeprefix("apps/apple/"), MODEL.format(name=f"Model{i}"))
        result = self.run_gate(shipped=True)
        self.assert_code(result, OK, "OK: swift-logic clean.")

    def test_every_reason_is_one_of_the_named_ones(self):
        module = _load_gate_module()
        named = {module.PLATFORM, module.SEAM, module.SHELL, module.PRESENTATION}
        for path, reason in module.ALLOWED.items():
            self.assertTrue(
                reason in named or reason.startswith("moving to sunrise_client_core::"),
                f"{path}: {reason}",
            )


class RefusesToGuess(GateCase):
    def test_a_tree_with_no_swift_is_red_not_green(self):
        (self.root / "apps/apple").mkdir(parents=True)
        result = self.run_gate()
        self.assert_code(result, COULD_NOT_RUN, "no Swift under apps/apple; the gate could not run.")


if __name__ == "__main__":
    unittest.main()
