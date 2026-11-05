#!/usr/bin/env python3
"""The exit-code contract of `publish-gate.py`, as assertions.

Why this file exists
--------------------

The gate prints "OK: publish clean." for this repository, and that line is
worth exactly as much as the rules behind it. Each rule has a way of passing
for the wrong reason that is invisible from a green run: a `publish` value read
as truthy when it is `false`, an inherited `publish.workspace = true` taken for
an explicit one, a `"private": "true"` string taken for the boolean, a
`MARKETING_VERSION` the pattern stopped matching -- after which the app-train
copies compared drop to fewer than there are, and nothing disagrees with
anything. So every pass case here asserts the counts the gate prints, and every
fail case asserts the file it names.

Fixtures are synthesised repositories
-------------------------------------

Every case builds a git repository in a temp directory, writes a handful of
manifests, stages them, and drives the gate at it with `--root`. Staging is the
whole setup: the gate reads the index through `git ls-files`, so an unstaged
file is also how a case says "untracked". Nothing here reads this repository's
own manifests -- the `publish-gate` job does that, and a contract test that did
too would go red for whatever package somebody added this week rather than for
a change to the contract.

What the exit codes are
-----------------------

0 clean, 1 on a violation, and 1 again for every "could not run" route, which
is `raise SystemExit(str)` exactly as in `orphan-crate-gate.py`. The names
below record what a route means, the values what it does.

Run it with `mise run publish-gate-test`, or directly.
"""

from __future__ import annotations

import json
import pathlib
import subprocess
import sys
import tempfile
import unittest

GATE = pathlib.Path(__file__).resolve().parent / "publish-gate.py"

OK = 0
VIOLATION = 1
COULD_NOT_RUN = 1

PRIVATE_CRATE = '[package]\nname = "{name}"\nversion = "0.1.0"\npublish = false\n'


class GateCase(unittest.TestCase):
    """One temp git repository per test, and one run of the gate over it."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.repo = pathlib.Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)
        self._git("init", "-q")

    def _git(self, *args: str) -> None:
        subprocess.run(
            ["git", "-C", str(self.repo), *args], check=True, capture_output=True, text=True
        )

    def write(self, rel: str, text: str, *, track: bool = True) -> None:
        path = self.repo / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        if track:
            self._git("add", "--", rel)

    def package(self, rel: str, *, track: bool = True, **fields) -> None:
        self.write(rel, json.dumps(fields, indent=2) + "\n", track=track)

    def crate(self, rel: str, name: str = "c") -> None:
        self.write(rel, PRIVATE_CRATE.format(name=name))

    def baseline(self, version: str = "0.3.0") -> None:
        """A clean repository: a root package, one app, one package, one crate,
        and an Apple project, every version agreeing."""
        self.package("package.json", name="root", version=version, private=True)
        self.package("apps/web/package.json", name="@s/web", version=version, private=True)
        self.package("packages/ui/package.json", name="@s/ui", version=version, private=True)
        self.write("Cargo.toml", '[workspace]\nmembers = ["crates/*"]\n')
        self.crate("crates/a/Cargo.toml", "a")
        self.write(
            "apps/apple/project.yml",
            f'settings:\n  base:\n    MARKETING_VERSION: "{version}"\n',
        )

    def run_gate(self, root: pathlib.Path | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            [sys.executable, str(GATE), "--root", str(root or self.repo)],
            capture_output=True,
            text=True,
        )

    def assert_code(self, result, expected: int, *expected_output: str) -> None:
        output = result.stdout + result.stderr
        self.assertEqual(
            result.returncode, expected, f"expected exit {expected}, got {result.returncode}:\n{output}"
        )
        for fragment in expected_output:
            self.assertIn(fragment, output)

    def assert_absent(self, result, *fragments: str) -> None:
        output = result.stdout + result.stderr
        for fragment in fragments:
            self.assertNotIn(fragment, output)


class Clean(GateCase):
    def test_a_private_tree_whose_versions_agree_is_clean(self):
        self.baseline()
        self.assert_code(
            self.run_gate(),
            OK,
            "OK: publish clean.",
            # The virtual root manifest is not a package; the one crate is.
            "publish: 1 crate manifest(s) and 3 package.json file(s) checked.",
            # Two package.json copies and one MARKETING_VERSION. A pattern that
            # stopped matching project.yml would print 2 here and still pass.
            "publish: app train `0.3.0`; 3 app-train version(s) compared",
            "publish: 0 `uniffi` pin(s)",
        )

    def test_an_unquoted_marketing_version_with_a_comment_is_read(self):
        self.baseline()
        self.write("apps/apple/project.yml", "    MARKETING_VERSION: 0.3.0  # the app train\n")
        self.assert_code(self.run_gate(), OK, "3 app-train version(s) compared")

    def test_an_untracked_manifest_is_not_read(self):
        # `node_modules/*/package.json` is never private and never ours; the
        # index is what decides what the repository contains.
        self.baseline()
        self.package("node_modules/dep/package.json", name="dep", version="9.9.9", track=False)
        self.write("target/x/Cargo.toml", '[package]\nname = "x"\n', track=False)
        self.assert_code(self.run_gate(), OK, "1 crate manifest(s) and 3 package.json")

    def test_a_package_json_deeper_than_one_level_is_not_an_app_train_copy(self):
        # Rule 3 covers `apps/<x>/package.json` and `packages/<x>/package.json`.
        # A nested fixture still has to be private, but its version is its own.
        self.baseline()
        self.package("apps/web/fixture/package.json", name="fx", version="1.0.0", private=True)
        self.assert_code(self.run_gate(), OK, "3 app-train version(s) compared")

    def test_a_repository_with_no_apple_project_is_clean(self):
        self.package("package.json", name="root", version="1.0.0", private=True)
        self.crate("Cargo.toml")
        self.assert_code(self.run_gate(), OK, "0 app-train version(s) compared")


class CratesMustSayPublishFalse(GateCase):
    def test_a_crate_with_no_publish_key_fails(self):
        self.baseline()
        self.write("tools/t/Cargo.toml", '[package]\nname = "t"\nversion = "0.1.0"\n')
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            "::error::publish: `tools/t/Cargo.toml` has a [package] with no `publish = false`.",
        )

    def test_an_inherited_publish_is_not_explicit(self):
        self.baseline()
        self.write(
            "crates/b/Cargo.toml",
            '[package]\nname = "b"\nversion.workspace = true\npublish.workspace = true\n',
        )
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            "`crates/b/Cargo.toml` inherits `publish` from a workspace.",
        )

    def test_publish_true_fails(self):
        self.baseline()
        self.write("crates/b/Cargo.toml", '[package]\nname = "b"\npublish = true\n')
        self.assert_code(self.run_gate(), VIOLATION, "sets `publish = True`; it must be `publish = false`.")

    def test_a_registry_list_is_not_false(self):
        # `publish = ["my-registry"]` publishes, to somewhere else.
        self.baseline()
        self.write("crates/b/Cargo.toml", '[package]\nname = "b"\npublish = ["corp"]\n')
        self.assert_code(self.run_gate(), VIOLATION, "`crates/b/Cargo.toml` sets `publish = ['corp']`")

    def test_a_virtual_workspace_manifest_is_not_a_package(self):
        self.baseline()
        self.write("other/Cargo.toml", '[workspace]\nmembers = []\n')
        self.assert_code(self.run_gate(), OK, "1 crate manifest(s)")

    def test_an_unparseable_manifest_is_named(self):
        self.baseline()
        self.write("crates/b/Cargo.toml", "[package\n")
        self.assert_code(self.run_gate(), VIOLATION, "`crates/b/Cargo.toml` could not be read as TOML")


class PackagesMustBePrivate(GateCase):
    def test_a_package_without_private_fails(self):
        self.baseline()
        self.package("packages/i18n/package.json", name="@s/i18n", version="0.3.0")
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            '::error::publish: `packages/i18n/package.json` does not set `"private": true`.',
        )

    def test_the_string_true_is_not_the_boolean(self):
        self.baseline()
        self.package("packages/i18n/package.json", name="@s/i18n", version="0.3.0", private="true")
        self.assert_code(self.run_gate(), VIOLATION, '`packages/i18n/package.json` does not set `"private": true`.')

    def test_the_root_package_must_be_private_too(self):
        self.baseline()
        self.package("package.json", name="root", version="0.3.0")
        self.assert_code(self.run_gate(), VIOLATION, '`package.json` does not set `"private": true`.')

    def test_a_package_outside_apps_and_packages_must_be_private(self):
        self.baseline()
        self.package("tools/x/package.json", name="x")
        self.assert_code(self.run_gate(), VIOLATION, '`tools/x/package.json` does not set `"private": true`.')


class TheAppTrainHasOneVersion(GateCase):
    def test_an_app_package_on_another_version_fails(self):
        self.baseline()
        self.package("apps/web/package.json", name="@s/web", version="0.2.9", private=True)
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            "`apps/web/package.json` has version `0.2.9`; the app train is `0.3.0` (root `package.json`).",
        )

    def test_a_package_with_no_version_is_a_disagreement(self):
        self.baseline()
        self.package("packages/ui/package.json", name="@s/ui", private=True)
        self.assert_code(self.run_gate(), VIOLATION, "`packages/ui/package.json` has no `version`;")

    def test_a_marketing_version_on_another_version_fails_with_its_line(self):
        self.baseline()
        self.write(
            "apps/apple/project.yml",
            'settings:\n  base:\n    MARKETING_VERSION: "0.3.0"\n'
            'targets:\n  X:\n    settings:\n      MARKETING_VERSION: "0.1.0"\n',
        )
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            "`apps/apple/project.yml:7` sets `MARKETING_VERSION` `0.1.0`; the app train is `0.3.0`",
        )

    def test_a_project_yml_with_no_marketing_version_fails(self):
        self.baseline()
        self.write("apps/apple/project.yml", "settings:\n  base: {}\n")
        self.assert_code(self.run_gate(), VIOLATION, "`apps/apple/project.yml` sets no `MARKETING_VERSION`")

    def test_a_commented_out_setting_is_not_read(self):
        self.baseline()
        self.write(
            "apps/apple/project.yml",
            '    # MARKETING_VERSION: "9.9.9"\n    MARKETING_VERSION: "0.3.0"\n',
        )
        self.assert_code(self.run_gate(), OK, "3 app-train version(s) compared")

    def test_every_disagreement_is_named_not_just_the_first(self):
        self.baseline()
        self.package("apps/web/package.json", name="@s/web", version="0.1.0", private=True)
        self.package("packages/ui/package.json", name="@s/ui", version="0.2.0", private=True)
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            "`apps/web/package.json` has version `0.1.0`",
            "`packages/ui/package.json` has version `0.2.0`",
        )


GENERATOR = "tools/uniffi-bindgen/Cargo.toml"
GENERATOR_LOCK = "tools/uniffi-bindgen/Cargo.lock"
RUNTIME = "crates/sunrise-core-bindings/Cargo.toml"
RUNTIME_LOCK = "Cargo.lock"


def uniffi_manifest(name: str, dependency: str) -> str:
    return PRIVATE_CRATE.format(name=name) + f"\n[dependencies]\nuniffi = {dependency}\n"


def uniffi_lock(*versions: str) -> str:
    entries = ['[[package]]\nname = "uniffi_core"\nversion = "9.9.9"\n']
    entries += [f'[[package]]\nname = "uniffi"\nversion = "{v}"\n' for v in versions]
    return "version = 4\n\n" + "\n".join(entries)


class TheBindingGeneratorAndItsRuntimeAgree(GateCase):
    def pins(
        self,
        generator: str = '{ version = "0.32.0", features = ["cli"] }',
        runtime: str = '{ version = "0.32.0", default-features = false }',
        generator_locked: tuple[str, ...] = ("0.32.0",),
        runtime_locked: tuple[str, ...] = ("0.32.0",),
    ) -> None:
        """The baseline plus the four files rule 4 reads, agreeing unless told otherwise."""
        self.baseline()
        self.write(GENERATOR, uniffi_manifest("uniffi-bindgen", generator))
        self.write(GENERATOR_LOCK, uniffi_lock(*generator_locked))
        self.write(RUNTIME, uniffi_manifest("sunrise-core-bindings", runtime))
        self.write(RUNTIME_LOCK, uniffi_lock(*runtime_locked))

    def test_agreeing_pins_are_clean_and_both_are_compared(self):
        self.pins()
        self.assert_code(
            self.run_gate(),
            OK,
            "OK: publish clean.",
            # A rule that silently read nothing would print 0 here and pass.
            "publish: 2 `uniffi` pin(s) (requirement and locked version) compared.",
        )

    def test_a_plain_string_requirement_is_read(self):
        self.pins(generator='"0.32.0"', runtime='"0.32.0"')
        self.assert_code(self.run_gate(), OK, "2 `uniffi` pin(s)")

    def test_without_the_generator_the_rule_does_not_apply(self):
        self.baseline()
        self.assert_code(self.run_gate(), OK, "0 `uniffi` pin(s)")

    def test_requirements_that_disagree_fail(self):
        self.pins(runtime='{ version = "0.31.0" }')
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            f"`{GENERATOR}` requires `uniffi` `0.32.0` but `{RUNTIME}` requires `0.31.0`;",
        )

    def test_locked_versions_that_disagree_fail(self):
        # Equal requirements still resolve apart: `0.32.0` is `^0.32.0`.
        self.pins(runtime_locked=("0.32.1",))
        self.assert_code(
            self.run_gate(),
            VIOLATION,
            f"`{GENERATOR_LOCK}` locks `uniffi` `0.32.0` but `{RUNTIME_LOCK}` locks `0.32.1`;",
        )

    def test_an_inherited_requirement_cannot_be_compared(self):
        self.pins(runtime="{ workspace = true }")
        self.assert_code(self.run_gate(), VIOLATION, f"`{RUNTIME}` has no literal `uniffi` version")

    def test_a_manifest_without_uniffi_cannot_be_compared(self):
        self.pins()
        self.write(RUNTIME, PRIVATE_CRATE.format(name="sunrise-core-bindings"))
        self.assert_code(self.run_gate(), VIOLATION, f"`{RUNTIME}` has no literal `uniffi` version")

    def test_a_lock_with_two_uniffi_versions_fails(self):
        self.pins(runtime_locked=("0.31.0", "0.32.0"))
        self.assert_code(
            self.run_gate(), VIOLATION, f"`{RUNTIME_LOCK}` locks `uniffi` `0.31.0`, `0.32.0`; it must lock exactly one."
        )

    def test_a_lock_with_no_uniffi_fails(self):
        self.pins(generator_locked=())
        self.assert_code(self.run_gate(), VIOLATION, f"`{GENERATOR_LOCK}` locks no `uniffi`; it must lock exactly one.")

    def test_an_untracked_runtime_lock_fails(self):
        self.pins()
        self._git("rm", "-q", "--cached", "--", RUNTIME_LOCK)
        self.assert_code(self.run_gate(), VIOLATION, f"`{RUNTIME_LOCK}` is not tracked")

    def test_an_unparseable_lock_is_named(self):
        self.pins()
        self.write(GENERATOR_LOCK, "[[package\n")
        self.assert_code(self.run_gate(), VIOLATION, f"`{GENERATOR_LOCK}` could not be read as TOML")


class RefusesToGuess(GateCase):
    def test_not_a_git_work_tree_could_not_run(self):
        with tempfile.TemporaryDirectory() as bare:
            result = self.run_gate(pathlib.Path(bare))
        self.assert_code(result, COULD_NOT_RUN, "`git ls-files` failed", "the gate could not run.")
        self.assert_absent(result, "OK: publish clean.")

    def test_an_empty_repository_could_not_run(self):
        self.assert_code(
            self.run_gate(), COULD_NOT_RUN, "git tracks no Cargo.toml and no package.json"
        )

    def test_no_root_package_could_not_run(self):
        self.crate("crates/a/Cargo.toml")
        self.assert_code(self.run_gate(), COULD_NOT_RUN, "no tracked root `package.json`")

    def test_a_root_package_with_no_version_could_not_run(self):
        self.package("package.json", name="root", private=True)
        result = self.run_gate()
        self.assert_code(result, COULD_NOT_RUN, "has no `version`, which is the app train version")
        self.assert_absent(result, "OK: publish clean.")

    def test_an_unreadable_root_package_could_not_run(self):
        self.write("package.json", "{not json\n")
        self.assert_code(
            self.run_gate(),
            COULD_NOT_RUN,
            "`package.json` could not be read as JSON",
            "the app train has no version; the gate could not run.",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
