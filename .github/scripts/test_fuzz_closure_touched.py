#!/usr/bin/env python3
"""What `fuzz-closure-touched.py` decides the pre-commit hook lints, as assertions.

The script's failure mode is silent: a rule that reaches too little makes the
hook skip the `fuzz/` clippy on a commit that broke a harness, and the commit
goes through green. So every route to `run` is asserted to reach it, and the
closure is asserted to follow normal and build dependencies but not dev ones.

The cases build their workspaces as dicts shaped like `cargo metadata --no-deps`
output, so nothing here reads this repository. Run it with
`mise run fuzz-closure-touched-test`, or directly:
`python3 .github/scripts/test_fuzz_closure_touched.py`.
"""

from __future__ import annotations

import importlib.util
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SPEC = importlib.util.spec_from_file_location("fuzz_closure_touched", os.path.join(HERE, "fuzz-closure-touched.py"))
touched = importlib.util.module_from_spec(SPEC)
sys.modules["fuzz_closure_touched"] = touched
SPEC.loader.exec_module(touched)

REPO = "/w"


def dep(name: str, kind: str | None = None) -> dict:
    return {"name": name, "kind": kind}


def workspace(**deps: list[dict]) -> dict:
    """A workspace `cargo metadata --no-deps` document, one crate per keyword."""
    return {
        "workspace_members": [f"{name} 0.1.0" for name in deps],
        "packages": [
            {
                "id": f"{name} 0.1.0",
                "name": name,
                "manifest_path": f"{REPO}/crates/{name}/Cargo.toml",
                "dependencies": on + [dep("serde")],
            }
            for name, on in deps.items()
        ],
    }


def fuzz(*path_deps: str) -> dict:
    """The fuzz workspace's `cargo metadata --no-deps`: its path deps under crates/."""
    return {
        "packages": [
            {
                "name": "sunrise-fuzz",
                "dependencies": [dep("libfuzzer-sys")]
                + [{"name": n, "kind": None, "path": f"{REPO}/crates/{n}"} for n in path_deps],
            }
        ]
    }


# fuzz -> server -> core -> log; server builds with gen; server's dev dep is
# testkit; cli depends on server, and nothing the fuzz package compiles
# depends on cli.
WS = workspace(
    log=[],
    gen=[],
    testkit=[],
    core=[dep("log")],
    server=[dep("core"), dep("gen", "build"), dep("testkit", "dev")],
    cli=[dep("server")],
)
DIRS = touched.closure_dirs(fuzz("server"), WS, REPO)


class Closure(unittest.TestCase):
    def test_a_path_dependency_and_its_normal_dependencies_are_reached(self):
        self.assertEqual(set(DIRS.values()), {"server", "core", "log", "gen"})

    def test_a_dev_dependency_is_not_compiled_for_the_fuzz_package(self):
        self.assertNotIn("testkit", DIRS.values())

    def test_a_dependent_of_a_reached_crate_is_not_reached(self):
        self.assertNotIn("cli", DIRS.values())

    def test_directories_are_repository_relative(self):
        self.assertIn("crates/log", DIRS)


class Decide(unittest.TestCase):
    def lints(self, *changed: str) -> bool:
        return touched.decide(list(changed), DIRS)[0]

    def test_a_harness_reaches_it(self):
        self.assertTrue(self.lints("fuzz/fuzz_targets/op_envelope.rs"))

    def test_the_fuzz_lockfile_reaches_it(self):
        self.assertTrue(self.lints("fuzz/Cargo.lock"))

    def test_the_workspace_wide_inputs_reach_it(self):
        for path in ("Cargo.toml", "Cargo.lock", "clippy.toml", ".cargo/config.toml", "rust-toolchain.toml"):
            with self.subTest(path=path):
                self.assertTrue(self.lints(path))

    def test_a_crate_manifest_nested_under_a_crate_is_not_the_root_manifest(self):
        self.assertFalse(self.lints("crates/cli/Cargo.toml"))

    def test_any_file_in_a_transitively_reached_crate_reaches_it(self):
        self.assertTrue(self.lints("crates/log/src/lib.rs"))
        self.assertTrue(self.lints("crates/gen/build.rs"))
        self.assertTrue(self.lints("crates/core/fixtures/x.json"))

    def test_a_crate_outside_the_closure_does_not(self):
        self.assertFalse(self.lints("crates/cli/src/main.rs"))
        self.assertFalse(self.lints("crates/testkit/src/lib.rs"))

    def test_a_sibling_whose_name_extends_a_reached_crate_does_not(self):
        self.assertFalse(self.lints("crates/log-extra/src/lib.rs"))

    def test_non_rust_changes_do_not(self):
        self.assertFalse(self.lints("docs/x.md", "apps/web/src/a.ts"))

    def test_nothing_staged_does_not(self):
        self.assertFalse(self.lints())

    def test_an_unknown_closure_lints(self):
        self.assertTrue(touched.decide(["docs/x.md"], None)[0])

    def test_one_reaching_path_among_many_is_enough(self):
        self.assertTrue(self.lints("docs/x.md", "crates/core/src/lib.rs"))


if __name__ == "__main__":
    unittest.main()
