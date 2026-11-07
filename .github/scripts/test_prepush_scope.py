#!/usr/bin/env python3
"""What `prepush-scope.py` decides a scoped local test run covers, as assertions.

The script's failure mode is silent: a rule that reaches too little makes
`mise run rust-test-changed` or `mise run test-changed` skip a test the push
broke, and the run still goes green. So
every rule that adds something to a scope is asserted to add it, and the
routes that widen to the whole suite are asserted to widen.

The cases build their workspaces as dicts shaped like `cargo metadata` output
and their git histories in temporary repositories, so nothing here reads this
repository. Run it with `mise run prepush-scope-test`, or directly:
`python3 .github/scripts/test_prepush_scope.py`.
"""

from __future__ import annotations

import importlib.util
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SPEC = importlib.util.spec_from_file_location("prepush_scope", os.path.join(HERE, "prepush-scope.py"))
scope = importlib.util.module_from_spec(SPEC)
sys.modules["prepush_scope"] = scope
SPEC.loader.exec_module(scope)


def metadata(**deps: list[str]) -> dict:
    """A `cargo metadata` document: one package per keyword, under crates/."""
    return {
        "workspace_root": "/w",
        "workspace_members": [f"{name} 0.1.0" for name in deps],
        "packages": [
            {
                "id": f"{name} 0.1.0",
                "name": name,
                "manifest_path": f"/w/crates/{name}/Cargo.toml",
                "dependencies": [{"name": d} for d in on] + [{"name": "serde"}],
            }
            for name, on in deps.items()
        ],
    }


# a <- b <- c, and d on its own.
PACKAGES = scope.packages_from_metadata(metadata(a=[], b=["a"], c=["b"], d=[]))


class Literals(unittest.TestCase):
    def test_a_relative_literal_resolves_from_the_file_and_from_its_home(self):
        targets = scope.literal_targets("../../docs/x.md", "crates/a/tests", "crates/a")
        self.assertIn("crates/docs/x.md", targets)
        self.assertIn("docs/x.md", targets)

    def test_a_root_relative_literal_is_taken_as_written(self):
        self.assertEqual(scope.literal_targets("schemas/s.json", "crates/a/src", "crates/a"), {"schemas/s.json"})

    def test_a_literal_joined_onto_the_manifest_dir_drops_its_leading_slash(self):
        self.assertIn("docs", scope.literal_targets("/../../docs/", "crates/a/src", "crates/a"))

    def test_text_that_is_not_a_path_names_nothing(self):
        self.assertEqual(scope.literal_targets("hello world", "crates/a", "crates/a"), set())
        self.assertEqual(scope.literal_targets("../", "crates/a", "crates/a"), {"crates"})
        self.assertEqual(scope.literal_targets("../../../..", "crates/a", "crates/a"), set())

    def test_a_bare_word_counts_only_where_it_is_joined(self):
        rust = scope.RUST_LITERAL
        self.assertEqual(scope.references('let t = task("docs");', rust, 1, "crates/a/src/l.rs", "crates/a"), set())
        self.assertEqual(scope.references('walk(&root.join("docs"));', rust, 1, "crates/a/src/l.rs", "crates/a"), {"docs"})
        self.assertEqual(scope.references("const d = join(root, 'docs');", scope.TS_LITERAL, 2, "apps/x/s.ts", "apps/x"), {"docs"})

    def test_comment_lines_name_nothing(self):
        text = '//! see "docs/a.md"\n/// "docs/b.md"\n * "docs/c.md"\nlet p = "docs/d.md";'
        self.assertEqual(scope.references(text, scope.RUST_LITERAL, 1, "crates/a/src/l.rs", "crates/a"), {"docs/d.md"})

    def test_a_named_directory_names_everything_under_it(self):
        self.assertEqual(scope.named(["docs/a/b.md", "docsx/c.md", "src/d.rs"], {"docs"}), ["docs/a/b.md"])


class Rust(unittest.TestCase):
    def test_a_changed_crate_reaches_its_dependents_transitively(self):
        found, _ = scope.rust_scope(["crates/a/src/lib.rs"], PACKAGES, {})
        self.assertEqual(found, {"a", "b", "c"})

    def test_a_leaf_crate_reaches_only_itself(self):
        found, _ = scope.rust_scope(["crates/c/src/lib.rs"], PACKAGES, {})
        self.assertEqual(found, {"c"})

    def test_a_named_file_reaches_the_naming_crate_and_not_its_dependents(self):
        found, why = scope.rust_scope(["docs/x.md"], PACKAGES, {"a": {"docs/x.md"}})
        self.assertEqual(found, {"a"})
        self.assertIn("a: its sources name docs/x.md", why)

    def test_a_crate_walking_the_tree_is_reached_by_any_crate_change(self):
        found, _ = scope.rust_scope(["crates/c/src/lib.rs"], PACKAGES, {"d": {"crates"}})
        self.assertEqual(found, {"c", "d"})

    def test_a_file_nothing_names_reaches_nothing(self):
        found, _ = scope.rust_scope(["README.md", "apps/web/x.ts"], PACKAGES, {"a": {"docs"}})
        self.assertEqual(found, set())

    def test_workspace_wide_inputs_reach_everything(self):
        for path in ("Cargo.toml", "Cargo.lock", ".cargo/config.toml", "rust-toolchain.toml", "clippy.toml"):
            with self.subTest(path=path):
                self.assertIsNone(scope.rust_scope([path], PACKAGES, {})[0])

    def test_a_crate_manifest_is_not_the_workspace_manifest(self):
        self.assertEqual(scope.rust_scope(["crates/d/Cargo.toml"], PACKAGES, {})[0], {"d"})

    def test_reaching_every_crate_is_reported_as_all(self):
        self.assertIsNone(scope.rust_scope(["crates/a/x", "crates/d/y"], PACKAGES, {})[0])

    def test_only_workspace_packages_are_dependencies(self):
        self.assertEqual(PACKAGES["b"].deps, {"a"})
        self.assertEqual(PACKAGES["a"].dir, "crates/a")


class Bun(unittest.TestCase):
    MEMBERS = ["apps/web", "packages/tokens"]

    def test_a_member_change_runs_the_suite(self):
        self.assertIsNotNone(scope.bun_reason(["packages/tokens/tokens/c.toml"], self.MEMBERS, set()))

    def test_root_configuration_runs_the_suite(self):
        for path in scope.BUN_GLOBAL:
            with self.subTest(path=path):
                self.assertIsNotNone(scope.bun_reason([path], self.MEMBERS, set()))

    def test_a_test_file_outside_the_members_runs_the_suite(self):
        self.assertIsNotNone(scope.bun_reason(["tests/x.test.ts"], self.MEMBERS, set()))
        self.assertIsNone(scope.bun_reason(["crates/a/x.test.ts"], self.MEMBERS, set()))

    def test_a_file_a_ts_source_names_runs_the_suite(self):
        self.assertIsNotNone(scope.bun_reason(["i18n/en.toml"], self.MEMBERS, {"i18n"}))

    def test_rust_and_docs_nothing_names_skip_the_suite(self):
        self.assertIsNone(scope.bun_reason(["crates/a/src/lib.rs", "docs/x.md"], self.MEMBERS, {"i18n"}))


def git(repo: str, *args: str) -> str:
    return subprocess.run(["git", *args], cwd=repo, check=True, capture_output=True, text=True).stdout.strip()


class Push(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.remote = os.path.join(self.tmp.name, "remote.git")
        self.repo = os.path.join(self.tmp.name, "repo")
        subprocess.run(["git", "init", "-q", "--bare", "-b", "master", self.remote], check=True)
        subprocess.run(["git", "init", "-q", "-b", "master", self.repo], check=True)
        git(self.repo, "config", "user.email", "t@example.com")
        git(self.repo, "config", "user.name", "t")
        self.commit("README.md")
        git(self.repo, "remote", "add", "origin", self.remote)
        git(self.repo, "push", "-q", "origin", "master")
        git(self.repo, "remote", "set-head", "origin", "master")

    def tearDown(self):
        self.tmp.cleanup()

    def commit(self, path: str):
        full = os.path.join(self.repo, path)
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "a") as f:
            f.write("x\n")
        git(self.repo, "add", path)
        git(self.repo, "commit", "-q", "-m", path)

    def changed(self) -> list[str]:
        base, _ = scope.push_base(self.repo)
        return scope.changed_files(self.repo, base)

    def test_a_new_branch_is_measured_from_the_default_branch(self):
        git(self.repo, "switch", "-q", "-c", "topic")
        self.commit("docs/a.md")
        self.commit("crates/a/src/lib.rs")
        self.assertEqual(self.changed(), ["crates/a/src/lib.rs", "docs/a.md"])

    def test_a_pushed_branch_is_measured_from_what_was_pushed(self):
        git(self.repo, "switch", "-q", "-c", "topic")
        self.commit("docs/a.md")
        git(self.repo, "push", "-q", "-u", "origin", "topic")
        self.commit("crates/a/src/lib.rs")
        self.assertEqual(self.changed(), ["crates/a/src/lib.rs"])

    def test_a_rename_names_both_paths(self):
        self.commit("crates/a/src/old.rs")
        git(self.repo, "push", "-q", "origin", "master")
        os.makedirs(os.path.join(self.repo, "crates/b/src"))
        git(self.repo, "mv", "crates/a/src/old.rs", "crates/b/src/new.rs")
        git(self.repo, "commit", "-q", "-m", "mv")
        self.assertEqual(self.changed(), ["crates/a/src/old.rs", "crates/b/src/new.rs"])

    def test_a_non_ascii_path_is_named_as_written(self):
        self.commit("docs/résumé.md")
        self.assertEqual(self.changed(), ["docs/résumé.md"])

    def test_no_remote_to_compare_with_is_no_base(self):
        git(self.repo, "remote", "remove", "origin")
        self.assertIsNone(scope.push_base(self.repo))

    def test_the_opt_out_runs_everything_without_reading_the_push(self):
        for value, full in (("1", True), ("true", True), ("0", False), ("", False), ("no", False)):
            with self.subTest(value=value):
                self.assertEqual(scope.full_requested({"SUNRISE_FULL_PREPUSH": value}), full)
        out = subprocess.run(
            [sys.executable, os.path.join(HERE, "prepush-scope.py"), "rust"],
            cwd=self.tmp.name,  # not a repository: only the opt-out can answer
            env={**os.environ, "SUNRISE_FULL_PREPUSH": "1"},
            capture_output=True,
            text=True,
            check=True,
        )
        self.assertEqual(out.stdout.strip(), "all")


if __name__ == "__main__":
    unittest.main()
