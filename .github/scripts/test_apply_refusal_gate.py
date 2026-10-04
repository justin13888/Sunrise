#!/usr/bin/env python3
"""What `apply-refusal-gate.py` reports, through the process boundary CI uses.

Why this file exists
--------------------

The gate's answer is an exit code, and on the real tree today it is 0. A gate
that only ever says 0 is indistinguishable from one that reads nothing, so
this file plants the refusal issue #298 describes -- a `?` or a
`return Err(..)` on a policy branch below the idempotence gate -- and asserts
the gate says 1, and that the shapes the house rule depends on (a caught
`Err(e)` pattern, a `debug_assert!`, a `?` on a classified storage call,
prose and strings naming `Err(`) stay 0.

Fixtures are synthesised in a temp directory holding a minimal
`crates/sunrise-core/src/engine` with the three files the gate reads. Every
subprocess runs with its cwd there, so the gate's relative paths cannot reach
the repository's own source.

Run it with `mise run apply-refusal-gate-test`, or directly.
"""

from __future__ import annotations

import pathlib
import subprocess
import sys
import tempfile
import unittest

GATE = pathlib.Path(__file__).resolve().parent / "apply-refusal-gate.py"

SIG = (
    "fn apply_control_op(&self, tx: &Transaction<'_>, inner: &InnerOp)"
    " -> rusqlite::Result<Vec<([u8; 16], u32)>> {\n"
)
CLEAN_CONTROL = SIG + (
    "    match inner {\n"
    "        InnerOp::KeyEnvelope(p) => {\n"
    "            let live = self.keychain.max_epoch_tx(tx, &p.stream_id)?.unwrap_or(0);\n"
    "            if p.epoch > live + 8 {\n"
    '                tracing::warn!(ev = "core.key.refused", "a leap Err(x)");\n'
    "                return Ok(Vec::new());\n"
    "            }\n"
    "            if let Err(e) = self.backfill(tx) {\n"
    '                tracing::warn!(cause = %e, "logged, not raised");\n'
    "            }\n"
    "            Ok(Vec::new())\n"
    "        }\n"
    "        InnerOp::DeviceRevoke(p) => self.apply_device_revoke(tx, p),\n"
    "    }\n"
    "}\n"
)
CLEAN_REVOKE = (
    "impl Engine {\n"
    "    pub(super) fn apply_device_revoke(&self, tx: &Transaction<'_>, p: &P)"
    " -> rusqlite::Result<Vec<([u8; 16], u32)>> {\n"
    "        // A self-revoke is refused: return Err(..) would lose the op row.\n"
    "        if p.revoked == p.sender {\n"
    "            return Ok(Vec::new());\n"
    "        }\n"
    '        tx.execute("INSERT INTO r VALUES (?1)", params![1])?;\n'
    "        match tx.query_row(\"SELECT 1\", [], |r| r.get::<_, i64>(0)) {\n"
    "            Err(rusqlite::Error::QueryReturnedNoRows) => {}\n"
    "            Err(e) if p.quiet => drop(e),\n"
    "            _ => {}\n"
    "        }\n"
    "        Ok(Vec::new())\n"
    "    }\n"
    "}\n"
)
CLEAN_LWW = (
    "pub(super) fn materialize_remote(tx: &Transaction<'_>, inner: &InnerOp)"
    " -> rusqlite::Result<()> {\n"
    "    if inner.is_control() {\n"
    '        debug_assert!(false, "a control op reached the entity materializer");\n'
    "        return Ok(());\n"
    "    }\n"
    "    let existing = read_row_lww(tx, \"tasks\", \"id\", b\"x\")?;\n"
    "    if existing.is_some() { return Ok(()); }\n"
    "    let c = '?';\n"
    "    match tx.query_row(\"SELECT 1\", [], |r| r.get::<_, i64>(0)) {\n"
    "        Err(a) | Ok(a) => drop(a),\n"
    "    }\n"
    '    let s = r#"return Err(x)?"#;\n'
    "    Ok(())\n"
    "}\n"
)


class GateCase(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = pathlib.Path(tmp.name)
        self.engine = self.root / "crates/sunrise-core/src/engine"
        self.engine.mkdir(parents=True)
        self.write("sync.rs", CLEAN_CONTROL)
        self.write("revocation.rs", CLEAN_REVOKE)
        self.write("lww.rs", CLEAN_LWW)

    def write(self, name: str, text: str) -> None:
        (self.engine / name).write_text(text)

    def run_gate(self) -> subprocess.CompletedProcess:
        return subprocess.run(
            [sys.executable, str(GATE)],
            cwd=self.root,
            capture_output=True,
            text=True,
            check=False,
        )

    def assert_clean(self) -> None:
        r = self.run_gate()
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("OK: apply-refusal clean", r.stdout)

    def assert_flags(self, *expected: str) -> str:
        r = self.run_gate()
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        for text in expected:
            self.assertIn(text, r.stdout)
        return r.stdout

    def plant_control(self, line: str) -> None:
        body = CLEAN_CONTROL.replace(
            "            Ok(Vec::new())\n        }\n",
            f"            {line}\n            Ok(Vec::new())\n        }}\n",
            1,
        )
        self.assertNotEqual(body, CLEAN_CONTROL, "the plant must land")
        self.write("sync.rs", body)

    # --- The house rule stays green ---

    def test_the_clean_fixture_is_clean(self):
        self.assert_clean()

    # --- The refusal #298 describes goes red ---

    def test_a_question_mark_on_a_policy_check_is_a_violation(self):
        self.plant_control("self.refuse_if_stale(tx, p)?;")
        self.assert_flags("`refuse_if_stale`, which PROPAGATES does not classify", "sync.rs:")

    def test_return_err_is_a_violation(self):
        self.plant_control("return Err(rusqlite::Error::QueryReturnedNoRows);")
        out = self.assert_flags("Err(..)", "Error::")
        self.assertIn("The op row went in at the idempotence gate", out)

    def test_a_tail_err_in_a_match_arm_is_a_violation(self):
        self.write(
            "sync.rs",
            CLEAN_CONTROL.replace(
                "self.apply_device_revoke(tx, p),",
                "Err(rusqlite::Error::InvalidQuery),",
            ),
        )
        self.assert_flags("Err(..)")

    def test_ok_or_and_map_err_are_violations(self):
        for plant, shown in (
            ("let k = key.ok_or(e)?;", ".ok_or("),
            ("let k = key.ok_or_else(|| e)?;", ".ok_or_else("),
            ("let k = decode(b).map_err(|_| e)?;", ".map_err("),
            ('bail!("stale");', "minted: bail!"),
            ('ensure!(fresh, "stale");', "minted: ensure!"),
            ('let e = anyhow!("stale");', "minted: anyhow!"),
        ):
            with self.subTest(plant=plant):
                self.plant_control(plant)
                self.assert_flags(shown)

    def test_a_panic_is_a_violation(self):
        # Each plant is asserted by the token the gate names, so dropping any
        # one alternative from MINTED turns its subtest red.
        for plant, shown in (
            ('panic!("no");', "minted: panic!"),
            ("unreachable!();", "minted: unreachable!"),
            ("todo!();", "minted: todo!"),
            ("unimplemented!();", "minted: unimplemented!"),
            ("assert!(ok);", "minted: assert!"),
            ("assert_eq!(a, b);", "minted: assert_eq!"),
            ("assert_ne!(a, b);", "minted: assert_ne!"),
            ("let x = y.unwrap();", "minted: .unwrap()"),
            ('y.expect("z");', "minted: .expect("),
        ):
            with self.subTest(plant=plant):
                self.plant_control(plant)
                self.assert_flags(shown)

    def test_a_question_mark_on_a_non_call_is_a_violation(self):
        self.plant_control("let v = pending?;")
        self.assert_flags("not a call")

    def test_the_revoke_span_is_read(self):
        self.write(
            "revocation.rs",
            CLEAN_REVOKE.replace("return Ok(Vec::new());", "return Err(e);", 1),
        )
        self.assert_flags("revocation.rs#apply_device_revoke")

    def test_the_whole_lww_file_is_read(self):
        self.write("lww.rs", CLEAN_LWW + "fn helper() -> R<()> { check()?; Ok(()) }\n")
        self.assert_flags("`check`", "lww.rs:")

    def test_a_turbofish_callee_is_resolved(self):
        self.write("lww.rs", CLEAN_LWW + "fn helper() -> R<()> { x.collect::<Vec<_>>()?; Ok(()) }\n")
        self.assert_flags("`collect`")

    def test_code_after_a_lifetime_is_still_read(self):
        # A lifetime is not a char literal: misreading `'_>` as one would
        # swallow the rest of the line and hide what follows it.
        self.write("lww.rs", CLEAN_LWW + "fn h(t: &T<'_>) -> R<()> { refuse()?; Ok(()) }\n")
        self.assert_flags("`refuse`")

    # --- It refuses to read nothing ---

    def test_a_renamed_span_is_not_a_pass(self):
        self.write("sync.rs", CLEAN_CONTROL.replace("fn apply_control_op", "fn apply_control"))
        self.assert_flags("no single `fn apply_control_op`")

    def test_a_missing_file_is_not_a_pass(self):
        (self.engine / "lww.rs").unlink()
        self.assert_flags("lww.rs does not exist")

    def test_a_missing_engine_is_not_a_pass(self):
        for name in ("sync.rs", "revocation.rs", "lww.rs"):
            (self.engine / name).unlink()
        self.engine.rmdir()
        self.assert_flags("does not exist; gate cannot run")


if __name__ == "__main__":
    unittest.main(verbosity=2)
