#!/usr/bin/env python3
"""The exit-code contract of `orphan-crate-gate.py`, as assertions.

Why this file exists
--------------------

The gate walks a dependency graph and prints a number —
"20/24 crates reachable from a shipping artifact" — which
`docs/implementation/overview.md` cites. Nothing checked the walk. If an
edge stopped being followed, or a root stopped being a root, the number
would come out smaller and the gate would still print OK, because the only
thing that turns the gate red is a crate the walk failed to reach, and a
walk that reaches less is a walk that finds *more* orphans, not fewer —
right up until the mutation is in the other direction. Both directions are
here: every pass case asserts the reachable count, so a walk that quietly
gained or lost an edge fails even when its verdict is still green.

Fixtures are synthesised workspaces
-----------------------------------

Every case builds a Cargo workspace of two to four one-line crates in a
temp directory and drives the gate at it with `--manifest-path`, which is
the flag the gate already carries for exactly this. Nothing here reads
this repository's own `Cargo.toml`: the `orphan-crates` job does that, and
a contract test that also did would go red for whatever crate somebody
added this week rather than for a change to the contract. It does invoke
`cargo metadata --no-deps`, which reads manifests without resolving,
registering or compiling anything, so the cases are offline and take
milliseconds each.

The two lists
-------------

`EXEMPT` and `QUARANTINE` are literals in the gate, and every run checks
them for staleness against the workspace it was pointed at — so a fixture
workspace that does not contain `sunrise-bench` fails on the stale check
before it can be asked anything else. Cases therefore run against a copy
of the gate with those two literals rewritten, the way
`test_file_size_gate.py` rewrites `THRESHOLDS` and `BASELINE`: the logic
under test is still the shipped logic, read from the shipped file at run
time, but a case can describe three crates instead of twenty-three.
`ShippedLists` below runs the gate *unmodified* against a workspace built
from its own declared entries, so the wiring between the real literals and
the staleness check is asserted too.

What the exit codes actually are
--------------------------------

The docstring of the gate says "2 if the gate could not run at all". It is
not: all three could-not-run routes are `raise SystemExit(str)`, which
CPython exits **1** for, printing the message to stderr. The assertions
below pin the behaviour, not the docstring, and say so at each one —
`RefusesToGuess` is the class that matters here, because what a caller
depends on is that a gate which cannot measure anything is red rather than
green, and that part is true.

Run it with `mise run orphan-crate-gate-test`, or directly.
"""

from __future__ import annotations

import importlib.util
import pathlib
import re
import subprocess
import sys
import tempfile
import unittest

GATE = pathlib.Path(__file__).resolve().parent / "orphan-crate-gate.py"

OK = 0
VIOLATION = 1
# Every "the gate could not run" route is `raise SystemExit(str)`. The gate's
# own docstring calls that 2; CPython makes it 1 and puts the string on
# stderr. The name records what the route means, the value what it does.
COULD_NOT_RUN = 1


def _load_gate_module():
    """The gate imported as a module, for reading its literals only.

    `main()` is behind an `if __name__` guard, so importing runs no cargo
    and produces no verdict. `ShippedLists` uses this to build a workspace
    out of whatever the shipped `EXEMPT` happens to name today, rather than
    restating those names here where they would go stale silently.
    """
    spec = importlib.util.spec_from_file_location("orphan_crate_gate", GATE)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class Crate:
    """One crate in a fixture workspace.

    `target` is the only thing the gate reads about a crate other than its
    dependency table: `bin`, `cdylib` and `staticlib` make it a root, `lib`
    does not.
    """

    def __init__(
        self,
        name: str,
        *,
        target: str = "lib",
        deps: tuple[str, ...] = (),
        optional_deps: tuple[str, ...] = (),
        build_deps: tuple[str, ...] = (),
        dev_deps: tuple[str, ...] = (),
        outside_deps: tuple[str, ...] = (),
    ) -> None:
        self.name = name
        self.target = target
        self.deps = deps
        self.optional_deps = optional_deps
        self.build_deps = build_deps
        self.dev_deps = dev_deps
        self.outside_deps = outside_deps


def _dep_table(header: str, entries: list[str]) -> str:
    return f"\n[{header}]\n" + "".join(entries) if entries else ""


class GateCase(unittest.TestCase):
    """One synthesised Cargo workspace per test."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = pathlib.Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)

    # -- fixtures ---------------------------------------------------------

    def workspace(self, *crates: Crate) -> pathlib.Path:
        """Write a Cargo workspace of these crates; return its manifest."""
        root = self.tmp / "ws"
        root.mkdir(parents=True, exist_ok=True)
        members = ", ".join(f'"{crate.name}"' for crate in crates)
        (root / "Cargo.toml").write_text(
            f'[workspace]\nmembers = [{members}]\nresolver = "2"\n'
        )
        for crate in crates:
            self._write_crate(root, crate)
        return root / "Cargo.toml"

    def _write_crate(self, root: pathlib.Path, crate: Crate) -> None:
        directory = root / crate.name
        (directory / "src").mkdir(parents=True, exist_ok=True)
        manifest = (
            f'[package]\nname = "{crate.name}"\nversion = "0.1.0"\n'
            'edition = "2021"\n'
        )
        if crate.target == "bin":
            (directory / "src/main.rs").write_text("fn main() {}\n")
        else:
            (directory / "src/lib.rs").write_text("")
            if crate.target != "lib":
                manifest += f'\n[lib]\ncrate-type = ["{crate.target}"]\n'

        normal = [f'{name} = {{ path = "../{name}" }}\n' for name in crate.deps]
        normal += [
            f'{name} = {{ path = "../{name}", optional = true }}\n'
            for name in crate.optional_deps
        ]
        # A path dependency that is neither a member nor inside the workspace
        # directory: `--no-deps` never lists it, so it exercises the gate's
        # `dep["name"] in members` guard rather than a KeyError.
        normal += [
            f'{name} = {{ path = "../../outside/{name}" }}\n'
            for name in crate.outside_deps
        ]
        for name in crate.outside_deps:
            self._write_outsider(name)

        manifest += _dep_table("dependencies", normal)
        manifest += _dep_table(
            "build-dependencies",
            [f'{name} = {{ path = "../{name}" }}\n' for name in crate.build_deps],
        )
        manifest += _dep_table(
            "dev-dependencies",
            [f'{name} = {{ path = "../{name}" }}\n' for name in crate.dev_deps],
        )
        (directory / "Cargo.toml").write_text(manifest)

    def _write_outsider(self, name: str) -> None:
        """A crate outside the workspace, detached by its own `[workspace]`."""
        directory = self.tmp / "outside" / name
        (directory / "src").mkdir(parents=True, exist_ok=True)
        (directory / "src/lib.rs").write_text("")
        (directory / "Cargo.toml").write_text(
            "[workspace]\n\n[package]\n"
            f'name = "{name}"\nversion = "0.1.0"\nedition = "2021"\n'
        )

    # -- the gate ---------------------------------------------------------

    def source(self, exempt: dict, quarantine: dict) -> pathlib.Path:
        """The gate, with its two lists replaced by this case's.

        Rewriting the literals rather than importing and patching keeps the
        subprocess boundary — which is what CI runs, and where the exit code
        lives — while letting a case describe three crates instead of the
        twenty-three the real workspace has.
        """
        text = GATE.read_text()
        for name, value in (("EXEMPT", exempt), ("QUARANTINE", quarantine)):
            pattern = re.compile(rf"^{name}: dict\[str, str\] = \{{.*?\}}$", re.S | re.M)
            self.assertRegex(text, pattern, f"{name} table not found in the gate")
            literal = f"{name}: dict = {value!r}"
            text = pattern.sub(lambda _m, lit=literal: lit, text, count=1)
        copy = self.tmp / "gate.py"
        copy.write_text(text)
        return copy

    def run_gate(
        self,
        manifest: pathlib.Path | str,
        *,
        exempt: dict | None = None,
        quarantine: dict | None = None,
        shipped: bool = False,
    ) -> subprocess.CompletedProcess:
        gate = GATE if shipped else self.source(exempt or {}, quarantine or {})
        return subprocess.run(
            [sys.executable, str(gate), "--manifest-path", str(manifest)],
            cwd=self.tmp,
            capture_output=True,
            text=True,
        )

    # -- assertions -------------------------------------------------------

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


class Reachable(GateCase):
    """Graphs where every crate has a path to something a user runs."""

    def test_a_binary_and_the_library_it_uses_is_clean(self):
        manifest = self.workspace(
            Crate("app", target="bin", deps=("engine",)),
            Crate("engine"),
        )
        result = self.run_gate(manifest)
        self.assert_code(
            result,
            OK,
            "OK: orphan-crates clean.",
            "orphan-crates: roots = app",
            # The count is the number the overview cites. An edge that stopped
            # being walked would print 1/2 here and still exit 0 without it.
            "orphan-crates: 2/2 crates reachable",
        )

    def test_reachability_is_transitive_through_the_whole_chain(self):
        # BFS, not a one-hop check: the defect this gate exists to catch is
        # usually three crates down from the binary.
        manifest = self.workspace(
            Crate("app", target="bin", deps=("middle",)),
            Crate("middle", deps=("leaf",)),
            Crate("leaf"),
        )
        self.assert_code(self.run_gate(manifest), OK, "3/3 crates reachable")

    def test_a_cdylib_is_a_root_because_xcode_is_the_consumer(self):
        # Nothing in the graph depends on the UniFFI surface — the consumer is
        # an Xcode build. If `cdylib` stopped seeding the traversal, every crate
        # under it would be reported as an orphan.
        manifest = self.workspace(
            Crate("bindings", target="cdylib", deps=("core",)),
            Crate("core"),
        )
        self.assert_code(
            self.run_gate(manifest),
            OK,
            "orphan-crates: roots = bindings",
            "2/2 crates reachable",
        )

    def test_a_staticlib_is_a_root_for_the_same_reason(self):
        manifest = self.workspace(
            Crate("bindings", target="staticlib", deps=("core",)),
            Crate("core"),
        )
        self.assert_code(self.run_gate(manifest), OK, "2/2 crates reachable")

    def test_a_build_dependency_is_a_real_edge(self):
        # A crate that only ever runs in a build script still ships: its output
        # is compiled into the binary.
        manifest = self.workspace(
            Crate("app", target="bin", build_deps=("codegen",)),
            Crate("codegen"),
        )
        self.assert_code(self.run_gate(manifest), OK, "2/2 crates reachable")

    def test_an_optional_dependency_is_a_real_edge(self):
        # Documented on purpose: a feature-gated seam is reachable by turning
        # the feature on, which is a different problem from a seam nothing
        # names. Counting it would make the gate argue about feature
        # resolution.
        manifest = self.workspace(
            Crate("app", target="bin", optional_deps=("extra",)),
            Crate("extra"),
        )
        self.assert_code(self.run_gate(manifest), OK, "2/2 crates reachable")

    def test_several_roots_are_listed_in_sorted_order(self):
        # The printed roots line is the gate's account of what it considered
        # shipping. Unsorted it would reorder run to run and stop being
        # readable as a diff.
        manifest = self.workspace(
            Crate("zeta", target="bin", deps=("shared",)),
            Crate("alpha", target="bin", deps=("shared",)),
            Crate("shared"),
        )
        self.assert_code(
            self.run_gate(manifest), OK, "orphan-crates: roots = alpha, zeta"
        )

    def test_a_dependency_outside_the_workspace_is_not_a_member(self):
        # `--no-deps` lists members only, so an external crate appears in a
        # dependency table and in no package list. The gate must skip it rather
        # than index a dict with it.
        manifest = self.workspace(
            Crate("app", target="bin", outside_deps=("vendored",)),
        )
        self.assert_code(self.run_gate(manifest), OK, "1/1 crates reachable")
        self.assert_absent(self.run_gate(manifest), "vendored")


class Orphans(GateCase):
    """The defect the gate exists for: a crate nothing shipping can reach."""

    def test_a_crate_no_binary_reaches_is_named_and_fails(self):
        manifest = self.workspace(Crate("app", target="bin"), Crate("stranded"))
        result = self.run_gate(manifest)
        self.assert_code(
            result,
            VIOLATION,
            "::error::orphan-crates: `stranded` is not reachable from any "
            "shipping binary.",
            "Wire it into a binary, delete it, or add it to EXEMPT/QUARANTINE",
            "1/2 crates reachable",
        )

    def test_a_dev_dependency_does_not_make_a_crate_reachable(self):
        # The whole point. "Only the test suite uses it" is the condition the
        # gate is looking for, so traversing dev edges would make it pass for
        # the exact shape it exists to catch.
        manifest = self.workspace(
            Crate("app", target="bin", dev_deps=("fixtures",)),
            Crate("fixtures"),
        )
        self.assert_code(
            self.run_gate(manifest),
            VIOLATION,
            "`fixtures` is not reachable",
            "1/2 crates reachable",
        )

    def test_a_crate_reachable_only_through_an_exempt_crate_is_an_orphan(self):
        # An exempt crate is never a root, so it cannot launder its own
        # dependencies into "reachable". `sunrise-bench` has a `bin`.
        manifest = self.workspace(
            Crate("app", target="bin"),
            Crate("bench", target="bin", deps=("helper",)),
            Crate("helper"),
        )
        result = self.run_gate(manifest, exempt={"bench": "benchmark harness"})
        self.assert_code(
            result,
            VIOLATION,
            "`helper` is not reachable",
            "orphan-crates: roots = app",
        )

    def test_every_orphan_is_named_not_just_the_first(self):
        # A reviewer who fixes the one crate the gate named and pushes again
        # should not discover a second one on the next run.
        manifest = self.workspace(
            Crate("app", target="bin"), Crate("beta"), Crate("alpha")
        )
        result = self.run_gate(manifest)
        self.assert_code(
            result, VIOLATION, "`alpha` is not reachable", "`beta` is not reachable"
        )

    def test_a_library_only_crate_is_not_a_root_for_itself(self):
        # A plain `lib` target is not something a user runs. If it seeded the
        # traversal every crate would be trivially reachable and the gate would
        # be incapable of failing.
        manifest = self.workspace(
            Crate("app", target="bin"),
            Crate("lonely", deps=("used-by-lonely",)),
            Crate("used-by-lonely"),
        )
        result = self.run_gate(manifest)
        self.assert_code(
            result,
            VIOLATION,
            "`lonely` is not reachable",
            "`used-by-lonely` is not reachable",
        )


class ExemptionLists(GateCase):
    """An exemption nobody is forced to prune is a slower way of deleting the check."""

    def test_an_exempt_crate_that_is_not_a_member_fails(self):
        manifest = self.workspace(Crate("app", target="bin"))
        result = self.run_gate(manifest, exempt={"deleted-crate": "gone"})
        self.assert_code(
            result,
            VIOLATION,
            "::error::orphan-crates: `deleted-crate` is listed as "
            "exempt/quarantined but is not a workspace member.",
            "prune the lists in .github/scripts/orphan-crate-gate.py",
        )

    def test_a_quarantined_crate_that_is_not_a_member_fails(self):
        manifest = self.workspace(Crate("app", target="bin"))
        result = self.run_gate(manifest, quarantine={"renamed": "#123"})
        self.assert_code(
            result, VIOLATION, "`renamed` is listed as exempt/quarantined"
        )

    def test_the_stale_check_runs_before_any_reachability_verdict(self):
        # "A wrong list makes every later verdict suspect": a stale entry must
        # not be reported alongside an orphan hunt that used the same wrong
        # list.
        manifest = self.workspace(Crate("app", target="bin"), Crate("stranded"))
        result = self.run_gate(manifest, exempt={"deleted-crate": "gone"})
        self.assert_code(result, VIOLATION, "`deleted-crate` is listed as")
        self.assert_absent(result, "`stranded` is not reachable", "crates reachable")

    def test_a_quarantined_crate_that_became_reachable_fails(self):
        # The list must shrink to empty. An entry left behind after the fix is
        # exactly as misleading as an entry that was never added.
        manifest = self.workspace(
            Crate("app", target="bin", deps=("fixed",)), Crate("fixed")
        )
        result = self.run_gate(manifest, quarantine={"fixed": "#99"})
        self.assert_code(
            result,
            VIOLATION,
            "::error::orphan-crates: `fixed` is quarantined but is now "
            "reachable — remove it from QUARANTINE.",
        )

    def test_a_quarantined_orphan_is_a_warning_and_not_a_failure(self):
        # A pre-existing defect with an owner does not block unrelated work.
        manifest = self.workspace(Crate("app", target="bin"), Crate("known"))
        result = self.run_gate(manifest, quarantine={"known": "#42 owner bob"})
        self.assert_code(
            result,
            OK,
            "::warning::orphan-crates: `known` is orphaned (quarantined: "
            "#42 owner bob).",
            "OK: orphan-crates clean.",
        )

    def test_an_exempt_crate_that_became_reachable_is_only_a_warning(self):
        # Deliberately not a failure: the exemption may be obsolete, but the
        # crate is wired in, which is the state the gate wants.
        manifest = self.workspace(
            Crate("app", target="bin", deps=("harness",)), Crate("harness")
        )
        result = self.run_gate(manifest, exempt={"harness": "test harness"})
        self.assert_code(
            result,
            OK,
            "::warning::orphan-crates: exempt crate `harness` is now reachable; "
            "the exemption may be obsolete.",
            "OK: orphan-crates clean.",
        )

    def test_an_exempt_orphan_is_silent(self):
        # The permanent list is the one that carries no noise: these crates are
        # structurally dependent-free and always will be.
        manifest = self.workspace(Crate("app", target="bin"), Crate("harness"))
        result = self.run_gate(manifest, exempt={"harness": "test harness"})
        self.assert_code(result, OK, "OK: orphan-crates clean.")
        self.assert_absent(result, "::error", "::warning")


class ShippedLists(GateCase):
    """The unmodified gate, driven by the entries it actually declares.

    Everything above runs a copy with the two literals rewritten, which
    cannot catch a list that has gone stale in the file itself. These two
    build their workspace out of `EXEMPT`/`QUARANTINE` as shipped, so the
    real literals are exercised without reading this repository's crates.
    """

    def setUp(self) -> None:
        super().setUp()
        module = _load_gate_module()
        self.listed = sorted(set(module.EXEMPT) | set(module.QUARANTINE))
        self.assertTrue(self.listed, "the gate declares no exemptions at all")

    def test_a_workspace_holding_every_listed_crate_is_not_stale(self):
        manifest = self.workspace(
            Crate("shipping-app", target="bin"),
            *(Crate(name) for name in self.listed),
        )
        result = self.run_gate(manifest, shipped=True)
        self.assert_code(result, OK, "OK: orphan-crates clean.")

    def test_a_workspace_missing_one_listed_crate_is_stale(self):
        dropped = self.listed[0]
        manifest = self.workspace(
            Crate("shipping-app", target="bin"),
            *(Crate(name) for name in self.listed[1:]),
        )
        result = self.run_gate(manifest, shipped=True)
        self.assert_code(
            result,
            VIOLATION,
            f"`{dropped}` is listed as exempt/quarantined but is not a "
            "workspace member.",
        )


class RefusesToGuess(GateCase):
    """Neither answer, when there is nothing the gate can measure.

    Each of these is `raise SystemExit(str)` in the gate: the message goes
    to stderr and the process exits 1. The gate's docstring calls this exit
    2, which it is not — what is asserted here is the part a caller
    depends on, that a gate which could not run is red rather than green.
    """

    def test_a_manifest_that_does_not_exist_is_not_a_pass(self):
        result = self.run_gate(self.tmp / "nowhere" / "Cargo.toml")
        self.assert_code(
            result,
            COULD_NOT_RUN,
            "::error::orphan-crates: `cargo metadata` exited",
            "the gate could not run.",
        )

    def test_an_unparseable_manifest_is_not_a_pass(self):
        # Fail closed on a manifest the gate cannot read: a truncated or
        # half-merged Cargo.toml must not be reported as a clean graph.
        broken = self.tmp / "Cargo.toml"
        broken.write_text("[workspace\nmembers = [")
        result = self.run_gate(broken)
        self.assert_code(result, COULD_NOT_RUN, "`cargo metadata` exited")

    def test_a_workspace_with_no_members_is_not_a_pass(self):
        # `members = []` is valid TOML and cargo reports it happily with an
        # empty package list. Zero crates trivially satisfies "every crate is
        # reachable", which is the one green answer that means nothing.
        root = self.tmp / "ws"
        root.mkdir()
        (root / "Cargo.toml").write_text('[workspace]\nmembers = []\nresolver = "2"\n')
        result = self.run_gate(root / "Cargo.toml")
        self.assert_code(
            result,
            COULD_NOT_RUN,
            "::error::orphan-crates: the workspace reports no members; "
            "the gate could not run.",
        )

    def test_a_workspace_with_nothing_shipping_is_not_a_pass(self):
        # Every crate is a library: there is no root to walk from, so every
        # crate would be an orphan. That is a broken gate invocation, not
        # twenty findings.
        manifest = self.workspace(Crate("one"), Crate("two"))
        result = self.run_gate(manifest)
        self.assert_code(
            result,
            COULD_NOT_RUN,
            "::error::orphan-crates: no crate produces a bin/cdylib/staticlib; "
            "the gate could not run.",
        )

    def test_a_workspace_whose_only_binary_is_exempt_is_not_a_pass(self):
        # The exempt-crates-are-not-roots rule can remove the last root. The
        # gate must say it cannot run rather than report the whole workspace
        # as orphaned.
        manifest = self.workspace(Crate("bench", target="bin"), Crate("lib"))
        result = self.run_gate(manifest, exempt={"bench": "benchmark harness"})
        self.assert_code(
            result, COULD_NOT_RUN, "no crate produces a bin/cdylib/staticlib"
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
