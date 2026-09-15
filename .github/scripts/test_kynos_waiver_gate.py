#!/usr/bin/env python3
"""The exit-code contract of `kynos-waiver-gate.py`, as assertions.

Why this file exists
--------------------

The gate makes a statement about the published API surface: no escape
hatch is reachable, therefore no operation in the OpenAPI description can
be flagged opaque. It backs that with one question — is
`kynos/unchecked` in the feature set Cargo resolved — and until this file
existed nothing checked that it could still answer the question at all. A
gate that reads a feature set is a gate that goes quietly green when the
shape of what it reads changes, and that shape *has* changed: kynos moved
to a git pin, and package ids became opaque in newer Cargo, which is why
the gate looks names up through the `packages` table instead of parsing
the id.

The failure mode being defended against is specific. If `resolved_features()`
returned an empty mapping, the gate would take the "kynos is not in the
dependency graph" route, which is red — good. But if it returned a mapping
in which kynos's features were merely *incomplete*, the gate would print
"OK: kynos-waiver clean" over a workspace with the hatch wide open. So the
cases below include the hatch being on in three different ways, only one of
which is a direct `features = ["unchecked"]`.

Fixtures are synthesised workspaces
-----------------------------------

The gate takes no arguments and reads no environment variable: it shells
out to `cargo metadata --all-features` and lets it find the manifest.
**The working directory is therefore the only thing that steers it**, so
every case writes a two-crate workspace in a temp directory and runs the
gate with `cwd` inside it. Nothing here reads this repository's own
`Cargo.toml`, which is what the `kynos-waiver` job in ci.yml is for; a
contract test that also did would go red for a dependency bump rather than
for a change to the contract.

The stand-in `kynos` is a path dependency carrying its own `[workspace]`
table, which detaches it from the fixture workspace. That detail is
load-bearing: an attached path dependency is a workspace member, and
`--all-features` would turn on *its* `unchecked` feature directly, so the
clean case could never be built.

Run it with `mise run kynos-waiver-gate-test`, or directly.
"""

from __future__ import annotations

import pathlib
import subprocess
import sys
import tempfile
import unittest

GATE = pathlib.Path(__file__).resolve().parent / "kynos-waiver-gate.py"

CLEAN = 0
WAIVER_REACHABLE = 1
CANNOT_RUN = 2


class GateCase(unittest.TestCase):
    """One synthesised Cargo workspace per test, with the gate run inside it."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = pathlib.Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)

    # -- fixtures ---------------------------------------------------------

    def framework(
        self,
        *,
        name: str = "kynos",
        features: tuple[str, ...] = ("server", "unchecked"),
        default: tuple[str, ...] = ("server",),
    ) -> pathlib.Path:
        """A stand-in for the framework crate, outside the workspace.

        The `[workspace]` table at the top is what keeps it out: a path
        dependency inside the workspace directory is a member, and
        `--all-features` enables a member's every feature, which would make
        the clean case unbuildable.
        """
        directory = self.tmp / "vendor" / name
        (directory / "src").mkdir(parents=True, exist_ok=True)
        (directory / "src/lib.rs").write_text("")
        table = "\n".join(f"{feature} = []" for feature in features)
        (directory / "Cargo.toml").write_text(
            "[workspace]\n\n[package]\n"
            f'name = "{name}"\nversion = "0.1.0"\nedition = "2021"\n\n'
            f"[features]\ndefault = [{', '.join(repr(f) for f in default)}]\n{table}\n"
        )
        return directory

    def app(
        self,
        *,
        dependencies: str = "",
        features: str = "",
        dev_dependencies: str = "",
    ) -> None:
        """The workspace the gate is pointed at, as one package."""
        (self.tmp / "src").mkdir(parents=True, exist_ok=True)
        (self.tmp / "src/main.rs").write_text("fn main() {}\n")
        manifest = (
            '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n'
        )
        if dependencies:
            manifest += f"\n[dependencies]\n{dependencies}"
        if dev_dependencies:
            manifest += f"\n[dev-dependencies]\n{dev_dependencies}"
        if features:
            manifest += f"\n[features]\n{features}"
        (self.tmp / "Cargo.toml").write_text(manifest)

    def with_framework(self, spec: str = "", **kwargs) -> None:
        """The common fixture: an app depending on the stand-in framework."""
        self.framework(**kwargs)
        extra = f", {spec}" if spec else ""
        self.app(dependencies=f'kynos = {{ path = "vendor/kynos"{extra} }}\n')

    # -- the gate ---------------------------------------------------------

    def run_gate(self, cwd: pathlib.Path | None = None):
        return subprocess.run(
            [sys.executable, str(GATE)],
            cwd=str(cwd if cwd is not None else self.tmp),
            capture_output=True,
            text=True,
        )

    def assert_code(self, result, expected: int, *expected_output: str) -> None:
        output = result.stdout + result.stderr
        self.assertEqual(
            result.returncode,
            expected,
            f"expected exit {expected}, got {result.returncode}:\n{output}",
        )
        for fragment in expected_output:
            self.assertIn(fragment, output)

    def assert_absent(self, result, *fragments: str) -> None:
        output = result.stdout + result.stderr
        for fragment in fragments:
            self.assertNotIn(fragment, output)


class NoWaiverReachable(GateCase):
    """The state the gate exists to hold: the hatch exists and is off."""

    def test_a_framework_whose_hatch_feature_is_off_is_clean(self):
        self.with_framework()
        result = self.run_gate()
        self.assert_code(
            result,
            CLEAN,
            "OK: kynos-waiver clean",
            "no escape hatch is reachable",
            "kynos-waiver: features = default, server",
        )

    def test_declaring_the_feature_is_not_enabling_it(self):
        # The distinction the whole gate rests on. `unchecked` is declared in
        # the framework's manifest in every fixture here, including this one;
        # a grep over manifests would call that a waiver. What matters is
        # Cargo's resolution, and Cargo did not turn it on.
        self.with_framework()
        result = self.run_gate()
        self.assert_code(result, CLEAN)
        self.assert_absent(result, "unchecked")

    def test_the_resolved_features_are_reported_in_sorted_order(self):
        # The printed line is the gate's account of what it read. Unsorted it
        # reorders run to run and stops being readable as a diff.
        self.with_framework(
            features=("zulu", "server", "alpha", "unchecked"),
            default=("zulu", "alpha"),
        )
        self.assert_code(
            self.run_gate(), CLEAN, "kynos-waiver: features = alpha, default, zulu"
        )

    def test_another_package_may_have_a_feature_of_the_same_name(self):
        # The gate asks about one package. A feature called `unchecked` on
        # something else is not a kynos escape hatch, and a mutation that
        # dropped the per-package scoping would fail here rather than
        # somewhere a human has to interpret.
        self.framework()
        self.framework(name="lookalike", features=("unchecked",), default=())
        self.app(
            dependencies=(
                'kynos = { path = "vendor/kynos" }\n'
                'lookalike = { path = "vendor/lookalike", features = ["unchecked"] }\n'
            )
        )
        self.assert_code(self.run_gate(), CLEAN, "OK: kynos-waiver clean")


class WaiverReachable(GateCase):
    """Any route by which the hatch could be turned on is a failure."""

    def test_a_direct_feature_request_on_the_dependency_fails(self):
        self.with_framework('features = ["unchecked"]')
        result = self.run_gate()
        self.assert_code(
            result,
            WAIVER_REACHABLE,
            "::error::kynos-waiver: `kynos/unchecked` is enabled.",
            "ADR-0021 is the place to record which one and why",
        )

    def test_the_failure_names_the_items_the_feature_would_create(self):
        # The remedy is unreadable without them: "a feature is enabled" tells
        # a reader nothing about what to go looking for in the handlers.
        self.with_framework('features = ["unchecked"]')
        self.assert_code(
            self.run_gate(),
            WAIVER_REACHABLE,
            "upgrade_unchecked",
            "route_unchecked",
            "layer_unchecked",
            "into_tower_unchecked",
            "OpaqueReason",
        )

    def test_a_workspace_feature_that_forwards_to_the_hatch_fails(self):
        # This is what `--all-features` is in the command for: a feature of
        # *this* workspace that nobody enables by default is still a feature
        # somebody can turn on, so the hatch is reachable.
        self.framework()
        self.app(
            dependencies='kynos = { path = "vendor/kynos" }\n',
            features='hatch = ["kynos/unchecked"]\n',
        )
        self.assert_code(
            self.run_gate(), WAIVER_REACHABLE, "`kynos/unchecked` is enabled."
        )

    def test_the_hatch_arriving_through_the_frameworks_own_defaults_fails(self):
        # Nobody in this workspace asked for it; a version bump of the
        # framework did. That is the drift the gate is most exposed to, and
        # the reason it reads resolution rather than this workspace's asks.
        self.with_framework(default=("server", "unchecked"))
        self.assert_code(
            self.run_gate(), WAIVER_REACHABLE, "`kynos/unchecked` is enabled."
        )

    def test_the_hatch_behind_a_dev_dependency_still_fails(self):
        # Documented here because it is a judgement, not an accident: cargo
        # resolves dev-dependencies into the same feature set, so a test-only
        # `unchecked` is reported. The gate's claim is about what the feature
        # makes exist, and a unified feature set makes it exist everywhere.
        self.framework()
        self.app(
            dev_dependencies=(
                'kynos = { path = "vendor/kynos", features = ["unchecked"] }\n'
            )
        )
        self.assert_code(
            self.run_gate(), WAIVER_REACHABLE, "`kynos/unchecked` is enabled."
        )


class RefusesToGuess(GateCase):
    """Exit 2: nothing was measured, so do not read a verdict into this."""

    def test_a_workspace_without_the_framework_is_not_a_pass(self):
        # The gate's premise is that kynos is what serves the API. If it is
        # not in the graph the gate is asserting nothing, and saying so is the
        # only honest answer — a dependency that got renamed or dropped would
        # otherwise leave a permanently green check behind.
        self.app()
        result = self.run_gate()
        self.assert_code(
            result,
            CANNOT_RUN,
            "::error::kynos-waiver: `kynos` is not in the dependency graph;",
            "the gate has nothing to check and is probably stale.",
        )

    def test_a_directory_with_no_manifest_is_not_a_pass(self):
        # Fail closed: `cargo metadata` exits non-zero and the gate must not
        # translate "I could not look" into "there is nothing there".
        empty = self.tmp / "empty"
        empty.mkdir()
        result = self.run_gate(cwd=empty)
        self.assert_code(result, CANNOT_RUN, "::error::kynos-waiver: cargo metadata failed:")

    def test_an_unparseable_manifest_is_not_a_pass(self):
        # A truncated or half-merged Cargo.toml is the realistic version of
        # the above, and it must be red for the same reason.
        (self.tmp / "Cargo.toml").write_text("[package\nname = ")
        result = self.run_gate()
        self.assert_code(result, CANNOT_RUN, "cargo metadata failed:")

    def test_a_manifest_naming_a_dependency_that_is_not_there_is_not_a_pass(self):
        # Resolution failure rather than parse failure: cargo reads the
        # manifest fine and then cannot build the graph. Same verdict.
        self.app(dependencies='kynos = { path = "vendor/kynos" }\n')
        result = self.run_gate()
        self.assert_code(result, CANNOT_RUN, "cargo metadata failed:")


if __name__ == "__main__":
    unittest.main(verbosity=2)
