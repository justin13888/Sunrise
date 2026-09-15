#!/usr/bin/env python3
"""What `core-filesystem-gate.py` reports, and where it stops reading.

Why this file exists
--------------------

The gate's answer is an exit code: 0 means "the core reaches the filesystem
only through `CoreConfig::storage`", 1 means it does not. Nothing but this
file has ever asserted that it can still say 1.

That mattered immediately. The gate's first `production_lines` stopped
reading a file at the first line beginning `#[cfg(test)]`, which held while
`sunrise-core` was a handful of large modules with their tests in one block
at the bottom. The engine and keychain splits landed `#[cfg(test)] use ...`
item attributes near the *top* of the new files, and the scan then ended in
the imports: 97% of `engine/oplog.rs`, 95% of `keychain/mod.rs` and 83% of
`engine/mod.rs` went unread, and the gate reported OK for all three. A green
check that reads nothing is worse than no check, because the enforcement row
in shared-core.md goes on claiming it.

So the cases below are about coverage as much as detection: for every shape
of `#[cfg(test)]` that appears in the crate, a violation is planted *after*
it and the gate has to still find it. `SELF_TESTS` inside the gate covers
the same ground against string fixtures; this file covers it against real
files on disk, through the process boundary CI actually uses, and pins the
exit codes CI reads.

Fixtures are synthesised in a temp directory: a minimal
`crates/sunrise-core/src` holding the two allowlisted files the gate
requires plus whatever the case is about. Every subprocess runs with its cwd
inside that directory, so the gate's relative `CRATE_SRC` cannot resolve to
the repository's own source even if a case forgets to arrange one.

Run it with `mise run core-filesystem-gate-test`, or directly.
"""

from __future__ import annotations

import contextlib
import io
import pathlib
import subprocess
import sys
import tempfile
import unittest

GATE = pathlib.Path(__file__).resolve().parent / "core-filesystem-gate.py"

# The gate refuses to run if an allowlisted path has gone missing, so every
# fixture needs both of them. Their contents are deliberately a violation:
# being allowlisted is the only reason the gate stays quiet about them, which
# means every case also asserts the allowlist is still doing its job.
VAULT_LOCK = 'fn lock(p: &Path) -> Result<File> {\n    OpenOptions::new().open(p)\n}\n'
CONFIG = 'fn zone() -> String {\n    std::fs::read_link(LOCALTIME)\n}\n'


class GateCase(unittest.TestCase):
    """One temp crate per test; the gate always runs inside it."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = pathlib.Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)
        self.src = self.tmp / "crates" / "sunrise-core" / "src"
        self.src.mkdir(parents=True)
        self.write("vault_lock.rs", VAULT_LOCK)
        self.write("config.rs", CONFIG)

    def write(self, rel: str, text: str) -> pathlib.Path:
        path = self.src / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        return path

    def run_gate(self) -> subprocess.CompletedProcess:
        return subprocess.run(
            [sys.executable, str(GATE)],
            cwd=self.tmp,
            capture_output=True,
            text=True,
        )

    def assert_clean(self) -> subprocess.CompletedProcess:
        result = self.run_gate()
        self.assertEqual(
            result.returncode, 0, f"expected clean:\n{result.stdout}{result.stderr}"
        )
        self.assertIn("core-filesystem clean", result.stdout)
        return result

    def assert_flags(self, *expected: str) -> subprocess.CompletedProcess:
        result = self.run_gate()
        self.assertEqual(
            result.returncode, 1, f"expected a violation:\n{result.stdout}{result.stderr}"
        )
        self.assertIn("reaches the filesystem outside", result.stdout)
        for line in expected:
            self.assertIn(line, result.stdout)
        return result


class ExitCodes(GateCase):
    """The two answers CI reads, and the two refusals to answer."""

    def test_a_core_that_only_uses_the_seam_is_clean(self):
        self.write("engine/oplog.rs", "fn save(&self) {\n    self.storage.write(k, v);\n}\n")
        result = self.assert_clean()
        self.assertIn("2 documented exceptions", result.stdout)

    def test_a_direct_filesystem_call_is_a_violation(self):
        self.write("engine/oplog.rs", 'fn save() {\n    std::fs::write(p, b"x");\n}\n')
        self.assert_flags("engine/oplog.rs:2")

    def test_a_missing_crate_is_not_a_pass(self):
        # The difference between "scanned and clean" and "there was nothing to
        # scan". A crate rename that silently turned the gate into a no-op is
        # the failure this is here for.
        for leftover in self.src.rglob("*.rs"):
            leftover.unlink()
        for directory in sorted(self.src.glob("**/"), reverse=True):
            directory.rmdir()
        result = self.run_gate()
        self.assertEqual(result.returncode, 1)
        self.assertIn("does not exist; gate cannot run", result.stdout)

    def test_a_stale_allowlist_entry_is_a_violation(self):
        # An allowlist entry for a file that no longer exists widens the gate
        # by exactly the path it names, and nothing else would notice.
        (self.src / "config.rs").unlink()
        result = self.run_gate()
        self.assertEqual(result.returncode, 1)
        self.assertIn("no longer exists", result.stdout)

    def test_the_allowlist_is_matched_on_the_whole_relative_path(self):
        # `config.rs` is allowlisted; `engine/config.rs` is a different file
        # and must not inherit the exception.
        self.write("engine/config.rs", "fn zone() {\n    std::fs::read_link(p);\n}\n")
        self.assert_flags("engine/config.rs:2")


class CfgTestBoundaries(GateCase):
    """Where a `#[cfg(test)]` region ends. The bug this gate shipped with.

    Each case plants the violation *below* a `#[cfg(test)]` construct, so a
    scanner that treats the attribute as end-of-file passes them all.
    """

    def test_a_cfg_test_use_skips_only_that_import(self):
        self.write(
            "engine/oplog.rs",
            "use super::Engine;\n"
            "#[cfg(test)]\n"
            "use super::ids::hex_short;\n"
            "\n"
            "impl Engine {\n"
            '    fn save() { std::fs::write(p, b"x"); }\n'
            "}\n",
        )
        self.assert_flags("engine/oplog.rs:6")

    def test_a_cfg_test_fn_skips_only_that_function(self):
        self.write(
            "engine/oplog.rs",
            "#[cfg(test)]\n"
            "fn helper() {\n"
            "    let _ = 1;\n"
            "}\n"
            'fn save() { std::fs::write(p, b"x"); }\n',
        )
        self.assert_flags("engine/oplog.rs:5")

    def test_an_attribute_stack_belongs_to_one_item(self):
        self.write(
            "engine/oplog.rs",
            "#[cfg(test)]\n"
            "#[allow(dead_code)]\n"
            "use std::fs;\n"
            'fn save() { std::fs::write(p, b"x"); }\n',
        )
        self.assert_flags("engine/oplog.rs:4")

    def test_an_inline_test_module_is_skipped_but_does_not_end_the_file(self):
        self.write(
            "engine/oplog.rs",
            "#[cfg(test)]\n"
            "mod tests {\n"
            '    fn t() { std::fs::write(p, b"x"); }\n'
            "}\n"
            'fn save() { std::fs::read(p); }\n',
        )
        result = self.assert_flags("engine/oplog.rs:5")
        # and the test module's own filesystem use is still not a violation
        self.assertNotIn("engine/oplog.rs:3", result.stdout)

    def test_braces_inside_strings_and_comments_do_not_end_the_module(self):
        # A miscounted brace hands the rest of a test module back to the
        # scanner as production code -- a false positive, which is the one
        # failure that gets a gate deleted.
        self.write(
            "engine/oplog.rs",
            "#[cfg(test)]\n"
            "mod tests {\n"
            '    let sql = "SELECT {";\n'
            '    let raw = r#"}"#;\n'
            "    // }\n"
            '    fn t() { std::fs::write(p, b"x"); }\n'
            "}\n",
        )
        self.assert_clean()

    def test_a_cfg_test_module_in_another_file_is_skipped_whole(self):
        # What the engine split produced: `engine/tests.rs` is 10k lines of
        # test code carrying no marker of its own.
        self.write("engine/mod.rs", "mod oplog;\n#[cfg(test)]\nmod tests;\n")
        self.write("engine/tests.rs", 'fn t() { std::fs::write(p, b"x"); }\n')
        self.assert_clean()

    def test_that_skip_does_not_extend_to_a_module_declared_normally(self):
        # Only the `#[cfg(test)]` declaration buys the skip. A sibling
        # declared without it is production and is read.
        self.write("engine/mod.rs", "mod oplog;\n#[cfg(test)]\nmod tests;\n")
        self.write("engine/tests.rs", "fn t() {}\n")
        self.write("engine/oplog.rs", 'fn save() { std::fs::write(p, b"x"); }\n')
        self.assert_flags("engine/oplog.rs:1")

    def test_the_declaring_file_keeps_being_scanned_after_the_mod_line(self):
        self.write(
            "engine/mod.rs",
            "#[cfg(test)]\nmod tests;\n\nfn save() { std::fs::read(p); }\n",
        )
        self.write("engine/tests.rs", "fn t() {}\n")
        self.assert_flags("engine/mod.rs:4")

    def test_a_comment_naming_the_attribute_is_not_the_attribute(self):
        self.write(
            "engine/oplog.rs",
            "// `open_op_row` is the only user and is `#[cfg(test)]`.\n"
            'fn save() { std::fs::write(p, b"x"); }\n',
        )
        self.assert_flags("engine/oplog.rs:2")


class BannedSpellings(GateCase):
    """Which calls count, through the process boundary rather than the regex."""

    def flag_case(self, body: str) -> subprocess.CompletedProcess:
        self.write("engine/oplog.rs", f"fn f() {{\n    {body}\n}}\n")
        return self.run_gate()

    def assert_banned(self, body: str) -> None:
        self.assertEqual(
            self.flag_case(body).returncode, 1, f"{body!r} should be a violation"
        )

    def assert_allowed(self, body: str) -> None:
        self.assertEqual(
            self.flag_case(body).returncode, 0, f"{body!r} should not be a violation"
        )

    def test_std_fs(self):
        self.assert_banned("std::fs::read(p);")

    def test_tokio_fs(self):
        # Rule 2 is about filesystem access, not about which runtime performs
        # it. `sunrise-core` owns a tokio runtime, so this spelling is reachable.
        self.assert_banned("tokio::fs::read(p).await;")

    def test_a_bare_fs_call_behind_a_use(self):
        self.assert_banned("fs::create_dir_all(d);")

    def test_open_options_and_file_constructors(self):
        self.assert_banned("OpenOptions::new().read(true).open(p);")
        self.assert_banned("File::create(p);")

    def test_the_method_spellings_of_the_same_syscalls(self):
        # `p.read_dir()` is `fs::read_dir(p)`. The lookbehind on the `fs::`
        # rule exists to not match method calls, so these need naming.
        self.assert_banned("for e in dir.read_dir() {}")
        self.assert_banned("let real = p.canonicalize();")
        self.assert_banned("let m = p.symlink_metadata();")

    def test_the_injected_seam_is_the_point(self):
        self.assert_allowed("self.storage.read(p);")
        self.assert_allowed("cfg.storage.write(name, bytes);")

    def test_a_comment_naming_a_banned_call_is_not_a_call(self):
        self.write(
            "engine/oplog.rs",
            "/// Enforced by `std::fs::File::try_lock`, an advisory lock.\n"
            "//! fs::write(p, b) would be a violation.\n"
            "/*\n * std::fs::read(p);\n */\n"
            "fn f() {}\n",
        )
        self.assert_clean()

    def test_what_rule_2_deliberately_does_not_cover(self):
        # Neither of these touches the filesystem. Banning them here would be
        # widening rule 2 without changing it -- see the note above BANNED.
        self.assert_allowed("let base = std::env::temp_dir();")
        self.assert_allowed("std::process::Command::new(prog).status();")


class SelfTest(GateCase):
    """The gate's own `SELF_TESTS`, which run before every scan."""

    def test_the_self_test_runs_and_reports_its_case_count(self):
        self.write("engine/oplog.rs", "fn f() {}\n")
        result = self.assert_clean()
        self.assertIn("core-filesystem self-test clean", result.stdout)

    def test_the_self_test_is_not_elided_under_python_o(self):
        # It used to end in a bare `assert`, which `python -O` removes. The
        # workflow does not pass -O today; nothing stops it from doing so, and
        # a check that can be compiled out is not a check.
        self.write("engine/oplog.rs", "fn f() {}\n")
        result = subprocess.run(
            [sys.executable, "-O", str(GATE)],
            cwd=self.tmp,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("core-filesystem self-test clean", result.stdout)

    def test_a_broken_scanner_fails_the_self_test_before_it_can_report_ok(self):
        # Import the gate and blunt `production_lines` back to its original
        # `break`-on-first-`#[cfg(test)]` behaviour. Every multi-line case in
        # SELF_TESTS exists to make that change visible, so the self-test has
        # to reject it -- otherwise the regression that motivated all of this
        # could land again with a green check.
        import importlib.util

        spec = importlib.util.spec_from_file_location("gate", GATE)
        gate = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(gate)

        def legacy(text: str):
            out = []
            for n, raw in enumerate(text.splitlines(), 1):
                line = raw.strip()
                if line.startswith("#[cfg(test)]"):
                    break
                if line.startswith("//"):
                    continue
                out.append((n, raw))
            return out

        gate.production_lines = legacy
        # self_test prints one ::error:: line per case it rejects; the point
        # here is the exit code, so the diagnosis is swallowed.
        with contextlib.redirect_stdout(io.StringIO()) as noise:
            with self.assertRaises(SystemExit) as raised:
                gate.self_test()
        self.assertEqual(raised.exception.code, 1)
        self.assertIn("self-test failed", noise.getvalue())


if __name__ == "__main__":
    unittest.main(verbosity=2)
