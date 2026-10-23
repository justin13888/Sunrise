#!/usr/bin/env python3
"""Fail when anything in the tree could be published, or the app train disagrees.

Why this gate exists
--------------------

Nothing in this repository publishes to a package registry. `release.yml`
says so in its closing note -- crates.io, npm and Google Play are "publish
targets deliberately absent" -- and ADR-0064 makes it a decision: every crate
and every package is private, and the two things that are released (the core
train and the app train) ship as release artifacts, not as packages. A
decision nobody checks is a default somebody overrides by accident. A new
crate copied from a template without `publish = false`, or a new package.json
without `"private": true`, is one `cargo publish` or `npm publish` away from
pushing AGPL source somewhere it cannot be withdrawn from, under a name and a
version nobody chose.

The same ADR gives the app train one version, the root `package.json`'s, which
every app-train manifest has to repeat because Bun, npm and Xcode each read
their own file. A repeated number drifts. So the gate checks the copies too.

The three rules
---------------

1. **Every tracked `Cargo.toml` with a `[package]` table says `publish =
   false` itself.** Inheriting it (`publish.workspace = true`) is refused:
   `tools/uniffi-bindgen`, `fuzz/` and `crates/sunrise-e2e/baseline-driver`
   are not members of the root workspace, so what they would inherit -- if
   anything -- depends on which workspace a manifest lands in, and the
   baseline driver is copied into a *different* commit's workspace by
   design. Explicit is the only spelling whose meaning does not move.
2. **Every tracked `package.json` says `"private": true`.** The boolean, not
   the string: npm treats `"private": "true"` as truthy and Bun's workspace
   tooling does not promise to.
3. **Every app-train version equals the root `package.json`'s `version`.**
   That is `apps/*/package.json`, `packages/*/package.json` and every
   `MARKETING_VERSION` in `apps/apple/project.yml`. A missing `version` in one
   of those package.json files is a disagreement, not an exemption.

The core train is not checked here: every crate inherits `[workspace.package]
version`, so there is one copy and nothing to disagree with it.

Determinism
-----------

The file list is `git ls-files`, so an untracked `node_modules/*/package.json`
or a scratch manifest under `target/` cannot change the verdict, and the
verdict is the same on a laptop as on the runner. TOML is read with `tomllib`
and JSON with `json`; `project.yml` is read with one anchored pattern rather
than a YAML library, because the runner's Python has none and the setting is
one scalar per line. Every list printed is sorted.

Usage: publish-gate.py [--root PATH]
Exit 0 clean, 1 on a violation. A gate that could not run at all -- not a git
work tree, no root `package.json`, no root `version`, no tracked manifest of
either kind -- is *also* 1, through `raise SystemExit(str)`, which prints the
message to stderr. The same convention as `orphan-crate-gate.py`, for the same
reason: what both callers depend on is that a gate which measured nothing is
red rather than green.

Every route above is asserted in `test_publish_gate.py` beside this file, run
by the `publish-gate-contract` job in ci.yml. It synthesises its own git
repositories and drives this script with `--root`, so it never reads this one.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import posixpath
import re
import subprocess
import sys
import tomllib

#: Where the app train's single source of truth lives.
ROOT_PACKAGE = "package.json"

#: The Xcode project spec whose `MARKETING_VERSION` is the app train's.
APPLE_PROJECT = "apps/apple/project.yml"

#: One `MARKETING_VERSION:` setting per line, quoted or not, with an optional
#: trailing comment. Anchored at the start of the line so a comment that
#: merely mentions the key is not read as a setting.
MARKETING_VERSION = re.compile(
    r"""^\s*MARKETING_VERSION:\s*(?:"([^"]*)"|'([^']*)'|([^\s#]+))\s*(?:\#.*)?$"""
)


def git_tracked(root: str) -> list[str]:
    """Every path git tracks under `root`, sorted."""
    try:
        listed = subprocess.run(
            ["git", "-C", root, "ls-files", "-z"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as error:
        raise SystemExit(f"::error::publish: `git ls-files` failed in {root}: {error}; the gate could not run.") from error
    return sorted(name for name in listed.split("\0") if name)


def is_app_train_package(name: str) -> bool:
    """`apps/<x>/package.json` or `packages/<x>/package.json`, one level down."""
    parts = name.split("/")
    return len(parts) == 3 and parts[0] in ("apps", "packages") and parts[2] == "package.json"


def check_cargo(root: pathlib.Path, name: str) -> tuple[bool, str | None]:
    """`(is_a_package, error)` for one tracked Cargo.toml."""
    try:
        manifest = tomllib.loads((root / name).read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        return True, f"`{name}` could not be read as TOML: {error}"
    package = manifest.get("package")
    if not isinstance(package, dict):
        # A virtual workspace manifest: nothing here can be published.
        return False, None
    publish = package.get("publish", None)
    if publish is False:
        return True, None
    if isinstance(publish, dict) and publish.get("workspace") is True:
        return True, (
            f"`{name}` inherits `publish` from a workspace. Say `publish = false` "
            "in its own [package]: what a manifest inherits depends on which "
            "workspace it lands in."
        )
    if publish is None:
        return True, f"`{name}` has a [package] with no `publish = false`."
    return True, f"`{name}` sets `publish = {publish!r}`; it must be `publish = false`."


def read_json(root: pathlib.Path, name: str) -> tuple[dict | None, str | None]:
    try:
        parsed = json.loads((root / name).read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        return None, f"`{name}` could not be read as JSON: {error}"
    if not isinstance(parsed, dict):
        return None, f"`{name}` is not a JSON object."
    return parsed, None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "--root",
        default=None,
        help="repository root (default: the repository this script is in)",
    )
    args = ap.parse_args()
    root = pathlib.Path(args.root) if args.root else pathlib.Path(__file__).resolve().parents[2]

    tracked = git_tracked(str(root))
    cargo = [n for n in tracked if posixpath.basename(n) == "Cargo.toml"]
    packages = [n for n in tracked if posixpath.basename(n) == "package.json"]
    if not cargo and not packages:
        raise SystemExit("::error::publish: git tracks no Cargo.toml and no package.json; the gate could not run.")
    if ROOT_PACKAGE not in packages:
        raise SystemExit(f"::error::publish: no tracked root `{ROOT_PACKAGE}`, which holds the app train version; the gate could not run.")

    errors: list[str] = []

    # -- rule 1 ---------------------------------------------------------------
    crates = 0
    for name in cargo:
        is_package, error = check_cargo(root, name)
        crates += is_package
        if error:
            errors.append(error)

    # -- rule 2, and the parsed bodies rule 3 reads ----------------------------
    bodies: dict[str, dict] = {}
    for name in packages:
        body, error = read_json(root, name)
        if error:
            errors.append(error)
            continue
        bodies[name] = body
        if body.get("private") is not True:
            errors.append(f'`{name}` does not set `"private": true`.')

    # -- rule 3 ---------------------------------------------------------------
    root_body = bodies.get(ROOT_PACKAGE)
    if root_body is None:
        # Unreadable: already reported above, and nothing to compare against.
        for error in errors:
            print(f"::error::publish: {error}")
        raise SystemExit(f"::error::publish: the root `{ROOT_PACKAGE}` could not be read, so the app train has no version; the gate could not run.")
    train = root_body.get("version")
    if not isinstance(train, str) or not train:
        raise SystemExit(f"::error::publish: the root `{ROOT_PACKAGE}` has no `version`, which is the app train version; the gate could not run.")

    copies = 0
    for name in sorted(n for n in bodies if is_app_train_package(n)):
        copies += 1
        version = bodies[name].get("version")
        if version != train:
            shown = "no `version`" if version is None else f"version `{version}`"
            errors.append(f"`{name}` has {shown}; the app train is `{train}` (root `{ROOT_PACKAGE}`).")

    if APPLE_PROJECT in tracked:
        try:
            lines = (root / APPLE_PROJECT).read_text(encoding="utf-8").splitlines()
        except (OSError, UnicodeDecodeError) as error:
            errors.append(f"`{APPLE_PROJECT}` could not be read: {error}")
            lines = []
        settings = []
        for number, line in enumerate(lines, start=1):
            match = MARKETING_VERSION.match(line)
            if match:
                settings.append((number, next(g for g in match.groups() if g is not None)))
        if lines and not settings:
            errors.append(f"`{APPLE_PROJECT}` sets no `MARKETING_VERSION`; it must be `{train}`.")
        for number, version in settings:
            copies += 1
            if version != train:
                errors.append(
                    f"`{APPLE_PROJECT}:{number}` sets `MARKETING_VERSION` `{version}`; "
                    f"the app train is `{train}` (root `{ROOT_PACKAGE}`)."
                )

    print(f"publish: {crates} crate manifest(s) and {len(packages)} package.json file(s) checked.")
    print(f"publish: app train `{train}`; {copies} app-train version(s) compared against the root `{ROOT_PACKAGE}`.")

    if errors:
        for error in errors:
            print(f"::error::publish: {error}")
        return 1

    print("OK: publish clean.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
