#!/usr/bin/env python3
"""Reject any waiver that would make the OpenAPI document non-authoritative.

ADR-0021 adopts `kynos` on one premise: *a handler which cannot be described
does not compile*. The framework keeps that promise by having no `Request`,
`Body` or `HeaderMap` extractor, and by putting every escape hatch behind one
feature — `unchecked` — whose items each stamp an operation with an
`OpaqueReason` in the emitted document.

So the gate is the feature, not the call. If `unchecked` is off, then
`upgrade_unchecked`, `route_unchecked`, `layer_unchecked` and
`into_tower_unchecked` do not exist to be called, and no operation in the
document can be flagged opaque. That is a stronger and simpler assertion than
inspecting `unchecked_reasons()` at runtime — which is what ADR-0021 originally
proposed, before the addendum below.

## Why this is not the gate ADR-0021 specified

The ADR's addendum planned to assert that `ProtocolUpgrade` on `/sync` was the
*only* waiver taken, because it expected `/sync` to stay a WebSocket carried by
`Router::upgrade_unchecked` for the length of the migration. Two things turned
out to be true when that was checked against the crate rather than its README:

  1. `upgrade_unchecked` lives behind the `unchecked` feature, which this
     workspace does not enable, so `unchecked_reasons()` does not exist to call.
  2. Even with the feature on, `kynos::server` calls hyper's
     `serve_connection` rather than `serve_connection_with_upgrades`, so the
     handshake could not have completed. kynos's own `examples/unchecked.rs`
     answers 501 rather than upgrading, and says so.

ADR-0023 removed the need for either: `/sync` is describable now, so no waiver
is taken anywhere and the strict gate — the one the ADR called "unholdable for a
project with a legitimate upgrade route" — is exactly the one that holds.

Exit 0 when no waiver is reachable, 1 when one is, 2 when the gate cannot run.
"""

from __future__ import annotations

import json
import subprocess
import sys

# The feature that gates every documented escape hatch.
WAIVER_FEATURE = "unchecked"

# The package whose features are read.
FRAMEWORK = "kynos"


def resolved_features() -> dict[str, list[str]]:
    """Every feature Cargo actually resolved, per package name."""
    try:
        raw = subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--all-features"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"::error::kynos-waiver: cargo metadata failed: {error}")
        raise SystemExit(2) from error

    metadata = json.loads(raw)
    # `--all-features` resolves the *workspace's* features, which is the
    # question being asked: could anything here turn the hatch on. A dependency
    # feature nobody enables is not reachable, and Cargo's own resolution is a
    # better authority on that than a grep over manifests.
    #
    # Package ids are opaque in newer Cargo, so the name comes from the
    # `packages` table rather than from parsing the id.
    by_id = {package["id"]: package["name"] for package in metadata.get("packages", [])}
    named: dict[str, list[str]] = {}
    for node in metadata.get("resolve", {}).get("nodes", []):
        name = by_id.get(node["id"])
        if name:
            named.setdefault(name, []).extend(node.get("features", []))
    return named


def main() -> int:
    features = resolved_features()
    if FRAMEWORK not in features:
        print(
            f"::error::kynos-waiver: `{FRAMEWORK}` is not in the dependency graph; "
            "the gate has nothing to check and is probably stale."
        )
        return 2

    enabled = sorted(set(features[FRAMEWORK]))
    if WAIVER_FEATURE in enabled:
        print(
            f"::error::kynos-waiver: `{FRAMEWORK}/{WAIVER_FEATURE}` is enabled. "
            "That feature is what makes `upgrade_unchecked`, `route_unchecked`, "
            "`layer_unchecked` and `into_tower_unchecked` exist, and each of them "
            "stamps an operation with an `OpaqueReason` — the document stops being "
            "authoritative about whatever it covers. If a waiver is genuinely "
            "needed, ADR-0021 is the place to record which one and why, and this "
            "gate should be narrowed to that reason rather than removed."
        )
        return 1

    print(f"kynos-waiver: features = {', '.join(enabled)}")
    print(
        "OK: kynos-waiver clean — no escape hatch is reachable, so no operation "
        "in the description can be flagged opaque."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
