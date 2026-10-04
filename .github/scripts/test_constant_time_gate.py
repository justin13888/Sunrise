#!/usr/bin/env python3
"""What `constant-time-gate.sh` reports, and what its allowlist may excuse.

Why this file exists
--------------------

The gate's answer is an exit code, and a grep gate in this repository has
shipped twice before answering 0 for every input (`grep-gate.sh` says how).
So the first case here is the one issue #365 asked for in so many words: a
`==` over a `[u8; 32]` tag planted in `sunrise-crypto` turns the gate red, and
the tree as it stands keeps it green.

The rest pin the allowlist that `grep-gate.sh --allowlist` added for this
gate, because an allowlist is where a check like this quietly stops checking:
an entry that excuses a line that has gone away, an entry with no reason, an
allowlist file that cannot be read, and an allowlist with no entries at all
(which an `NR == FNR` awk idiom would have read as excusing everything).

Fixtures are synthesised in a temp directory holding the four search paths
the gate names, and every subprocess runs with its cwd there and
`CONSTANT_TIME_ALLOWLIST` pointing at a fixture, so no case can read the
repository's own source or allowlist by accident. The one case that does read
them says so.

Run it with `mise run constant-time-gate-test`, or directly:
`python3 .github/scripts/test_constant_time_gate.py`.
"""

from __future__ import annotations

import os
import pathlib
import subprocess
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
GATE = HERE / "constant-time-gate.sh"
REPO = HERE.parent.parent
SEARCHED = (
    "crates/sunrise-crypto/src",
    "crates/sunrise-pairing/src",
    "crates/sunrise-http-sig/src",
    "crates/sunrise-server/src/auth",
)
CRYPTO = "crates/sunrise-crypto/src/lib.rs"

# The shape the issue names: a variable-time comparison over a 32-byte tag.
SEEDED_TAG = (
    "pub fn verify(expected_tag: [u8; 32], tag: [u8; 32]) -> bool {\n"
    "    tag == expected_tag\n"
    "}\n"
)
CLEAN = (
    "use subtle::ConstantTimeEq;\n"
    "pub fn verify(expected_tag: &[u8; 32], tag: &[u8; 32]) -> bool {\n"
    "    // tag == expected_tag would leak the matching prefix length.\n"
    "    tag.ct_eq(expected_tag).into()\n"
    "}\n"
)


class GateCase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.tmp = pathlib.Path(self._tmp.name)
        for directory in SEARCHED:
            (self.tmp / directory).mkdir(parents=True)
        self.allowlist = self.tmp / "allow.tsv"
        self.allowlist.write_text("# no entries\n")

    def write(self, rel: str, text: str) -> None:
        (self.tmp / rel).write_text(text)

    def allow(self, *entries: tuple[str, str, str]) -> None:
        lines = ["# fixture"] + ["\t".join(entry) for entry in entries]
        self.allowlist.write_text("\n".join(lines) + "\n")

    def run_gate(self, cwd: pathlib.Path | None = None, **env: str):
        environ = dict(os.environ)
        environ["CONSTANT_TIME_ALLOWLIST"] = str(self.allowlist)
        environ.update(env)
        return subprocess.run(
            ["bash", str(GATE)],
            cwd=cwd or self.tmp,
            env=environ,
            capture_output=True,
            text=True,
        )

    def assert_exit(self, result, code: int) -> None:
        self.assertEqual(
            result.returncode, code, f"exit {result.returncode}:\n{result.stdout}{result.stderr}"
        )


class Detection(GateCase):
    def test_a_seeded_tag_comparison_in_sunrise_crypto_is_a_violation(self):
        self.write(CRYPTO, SEEDED_TAG)
        result = self.run_gate()
        self.assert_exit(result, 1)
        self.assertIn(f"{CRYPTO}:2:", result.stdout)

    def test_constant_time_eq_and_comments_are_clean(self):
        self.write(CRYPTO, CLEAN)
        result = self.run_gate()
        self.assert_exit(result, 0)
        self.assertIn("constant-time clean, 0 allowlisted", result.stdout)

    def test_each_shape_is_caught(self):
        shapes = {
            "left operand": "if mac != computed {",
            "right operand": "if computed == self.digest {",
            "indexed left operand": "if expected_sig[..] == got {",
            "call on the right": "ok = got == hash(&body);",
            "eq method": "if nonce.eq(&other) {",
            "ne method": "if body_hash().ne(&other) {",
            "screaming constant": "if got != EXPECTED_TAG {",
        }
        for label, line in shapes.items():
            with self.subTest(label):
                self.write(CRYPTO, f"fn f() {{\n    {line}\n}}\n")
                self.assert_exit(self.run_gate(), 1)

    def test_names_without_a_secret_word_are_not_caught(self):
        self.write(
            CRYPTO,
            "fn f() {\n"
            "    if pair[0].0 == pair[1].0 {}\n"
            "    if signing.public_bytes() != body.to_id_s_pub {}\n"
            "    if bytes.len() != 32 {}\n"
            "}\n",
        )
        self.assert_exit(self.run_gate(), 0)

    def test_every_searched_crate_is_covered(self):
        for directory in SEARCHED:
            with self.subTest(directory):
                path = f"{directory}/x.rs"
                self.write(path, "fn f() { if token == presented {} }\n")
                result = self.run_gate()
                self.assert_exit(result, 1)
                self.assertIn(path, result.stdout)
                (self.tmp / path).unlink()


class Allowlist(GateCase):
    def test_an_entry_excuses_its_exact_line(self):
        self.write(CRYPTO, SEEDED_TAG)
        self.allow((CRYPTO, "tag == expected_tag", "fixture: both operands are public"))
        result = self.run_gate()
        self.assert_exit(result, 0)
        self.assertIn("1 allowlisted", result.stdout)

    def test_an_entry_does_not_excuse_the_same_line_in_another_file(self):
        self.write(CRYPTO, SEEDED_TAG)
        other = "crates/sunrise-pairing/src/x.rs"
        self.write(other, SEEDED_TAG)
        self.allow((CRYPTO, "tag == expected_tag", "fixture"))
        result = self.run_gate()
        self.assert_exit(result, 1)
        self.assertIn(other, result.stdout)

    def test_a_stale_entry_is_a_violation(self):
        self.write(CRYPTO, CLEAN)
        self.allow((CRYPTO, "tag == expected_tag", "excuses a line that is gone"))
        result = self.run_gate()
        self.assert_exit(result, 1)
        self.assertIn("match no line", result.stdout)

    def test_an_entry_without_a_reason_is_refused(self):
        self.write(CRYPTO, SEEDED_TAG)
        self.allowlist.write_text(f"{CRYPTO}\ttag == expected_tag\t \n")
        result = self.run_gate()
        self.assert_exit(result, 1)
        self.assertIn("not path<TAB>line<TAB>reason", result.stdout)

    def test_a_missing_allowlist_is_not_a_pass(self):
        self.write(CRYPTO, CLEAN)
        self.allowlist.unlink()
        result = self.run_gate()
        self.assert_exit(result, 1)
        self.assertIn("does not exist", result.stdout)

    def test_an_allowlist_with_no_entries_excuses_nothing(self):
        # A wholly empty file is the case an `NR == FNR` split gets wrong: the
        # first file contributes no records, so every hit reads as an entry.
        self.write(CRYPTO, SEEDED_TAG)
        self.allowlist.write_text("")
        self.assert_exit(self.run_gate(), 1)


class Refusals(GateCase):
    def test_no_search_path_is_not_a_pass(self):
        empty = self.tmp / "elsewhere"
        empty.mkdir()
        result = self.run_gate(cwd=empty)
        self.assert_exit(result, 1)
        self.assertIn("none of the search paths exist", result.stdout)


class TheTree(unittest.TestCase):
    """The repository's own source against its own allowlist."""

    def test_the_tree_passes(self):
        result = subprocess.run(
            ["bash", str(GATE)], cwd=REPO, capture_output=True, text=True
        )
        self.assertEqual(
            result.returncode, 0, f"the tree fails its own gate:\n{result.stdout}{result.stderr}"
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
