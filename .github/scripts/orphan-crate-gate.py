#!/usr/bin/env python3
"""Fail when a workspace crate is unreachable from every shipping artifact.

Why this gate exists
--------------------

The v1 rewrite kept producing the same defect: a library seam that is correct,
documented and well covered by its own unit tests, but that no shipping binary
ever calls. `cargo test --workspace` is green, clippy is green, coverage looks
fine — and the feature does not exist for a user, because nothing links it.
Five-plus instances of this landed and had to be found by hand.

Every one of them is visible in the dependency graph as a crate with no path to
a binary. So: walk the graph, and fail the build when a crate has no such path.

What "reachable" means here
---------------------------

Roots are the workspace members that produce something a user runs: a `bin`
target, or a `cdylib`/`staticlib` (the UniFFI surface the macOS app links, which
Cargo cannot see anyone depending on because the consumer is Xcode).

Edges are intra-workspace `dependencies` and `build-dependencies`.
**`dev-dependencies` are deliberately excluded** — "only the test suite uses it"
is precisely the condition this gate is looking for, so counting a dev-dep as
reachability would make the check pass for the exact shape it exists to catch.

Optional dependencies *are* counted. A feature-gated seam is reachable by
someone turning the feature on; that is a different problem from a seam nothing
names at all, and conflating them would make the gate argue about feature
resolution, which is where determinism goes to die.

Determinism
-----------

`cargo metadata --no-deps` reads the workspace manifests and nothing else: no
registry access, no resolution, no build. The traversal is a plain BFS and every
list printed is sorted. Same tree in, same verdict out — a gate that flaps gets
disabled within a week and then protects nothing.

Usage: orphan-crate-gate.py [--manifest-path PATH]
Exit 0 clean, 1 on a violation or a stale list entry, 2 if the gate could not
run at all (which is a failure, not a pass).
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from collections import deque

# --------------------------------------------------------------------------
# Exemptions. Both lists are checked for staleness on every run: naming a crate
# that no longer exists, or a quarantined crate that has since been wired up,
# fails the gate. An exemption list that nobody is forced to prune is just a
# slower way of deleting the check.
# --------------------------------------------------------------------------

# Permanent. These are structurally dependent-free and always will be.
EXEMPT: dict[str, str] = {
    # Criterion harness plus the `baseline` bin that merges its output into
    # bench/baseline.json. Nothing ships it; CI runs it directly by name.
    "sunrise-bench": "benchmark harness, invoked as `cargo bench -p sunrise-bench`",
    # Cross-crate integration tests. A test harness having no dependents is the
    # correct shape — if a shipping crate depended on it, that would be the bug.
    "sunrise-e2e": "end-to-end test harness, invoked as `cargo test -p sunrise-e2e`",
    # Known-answer vectors for the crypto suite, consumed only as a
    # dev-dependency of sunrise-crypto. Dev edges are not traversed (see above),
    # so this needs saying out loud rather than being silently reachable.
    "sunrise-crypto-test-vectors": "test fixture, dev-dependency of sunrise-crypto only",
}

# Temporary. A real orphan with an owner and an issue number. The gate reports
# these as warnings so a pre-existing defect does not block unrelated work, but
# the list must shrink to empty — and the staleness check above means an entry
# cannot be left behind once it is fixed.
QUARANTINE: dict[str, str] = {
    # gcal.rs + ical.rs implement docs/09-integrations/ against the
    # IntegrationProvider trait, with tests. No crate depends on the package:
    # it is declared in [workspace.dependencies] and named by nobody. This is
    # the defect class this gate exists for, caught by its first run.
    # Wiring it into sunrise-server (or deleting it) is issue #4.
    "sunrise-integrations": "unwired external-calendar seam; tracked by issue #4",
}

# Target kinds that mean "a user can run or link this directly".
SHIPPING_KINDS = frozenset({"bin", "cdylib", "staticlib"})

# Dependency kinds traversed. `cargo metadata` spells a normal dependency as
# a null kind.
TRAVERSED_KINDS = frozenset({None, "build"})


def load_metadata(manifest_path: str | None) -> dict:
    cmd = ["cargo", "metadata", "--format-version", "1", "--no-deps"]
    if manifest_path:
        cmd += ["--manifest-path", manifest_path]
    proc = subprocess.run(cmd, capture_output=True, text=True, check=False)
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr)
        raise SystemExit(f"::error::orphan-crates: `cargo metadata` exited {proc.returncode}; the gate could not run.")
    return json.loads(proc.stdout)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--manifest-path", default=None)
    args = ap.parse_args()

    meta = load_metadata(args.manifest_path)
    packages = {p["name"]: p for p in meta["packages"]}
    members = set(packages)
    if not members:
        raise SystemExit("::error::orphan-crates: the workspace reports no members; the gate could not run.")

    # Stale-list check first: a wrong list makes every later verdict suspect.
    stale = sorted((EXEMPT.keys() | QUARANTINE.keys()) - members)
    if stale:
        for name in stale:
            print(f"::error::orphan-crates: `{name}` is listed as exempt/quarantined but is not a workspace member.")
        print("::error::orphan-crates: prune the lists in .github/scripts/orphan-crate-gate.py.")
        return 1

    edges: dict[str, set[str]] = {name: set() for name in members}
    for name, pkg in packages.items():
        for dep in pkg["dependencies"]:
            if dep["kind"] in TRAVERSED_KINDS and dep["name"] in members and dep["name"] != name:
                edges[name].add(dep["name"])

    # An exempt crate is never a root: `sunrise-bench` has a `bin`, and letting
    # it seed the traversal would launder its dependencies into "reachable".
    roots = sorted(
        name
        for name, pkg in packages.items()
        if name not in EXEMPT
        and any(k in SHIPPING_KINDS for t in pkg["targets"] for k in t["kind"])
    )
    if not roots:
        raise SystemExit("::error::orphan-crates: no crate produces a bin/cdylib/staticlib; the gate could not run.")

    reachable: set[str] = set()
    queue = deque(roots)
    while queue:
        name = queue.popleft()
        if name in reachable:
            continue
        reachable.add(name)
        queue.extend(sorted(edges[name] - reachable))

    print(f"orphan-crates: roots = {', '.join(roots)}")
    print(f"orphan-crates: {len(reachable)}/{len(members)} crates reachable from a shipping artifact.")

    failed = False

    revived = sorted(name for name in QUARANTINE if name in reachable)
    for name in revived:
        print(f"::error::orphan-crates: `{name}` is quarantined but is now reachable — remove it from QUARANTINE.")
        failed = True

    for name in sorted(name for name in EXEMPT if name in reachable):
        print(f"::warning::orphan-crates: exempt crate `{name}` is now reachable; the exemption may be obsolete.")

    for name in sorted(name for name in QUARANTINE if name not in reachable):
        print(f"::warning::orphan-crates: `{name}` is orphaned (quarantined: {QUARANTINE[name]}).")

    orphans = sorted(members - reachable - set(EXEMPT) - set(QUARANTINE))
    for name in orphans:
        print(
            f"::error::orphan-crates: `{name}` is not reachable from any shipping binary. "
            "Wire it into a binary, delete it, or add it to EXEMPT/QUARANTINE with a reason."
        )
        failed = True

    if failed:
        return 1

    print("OK: orphan-crates clean.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
