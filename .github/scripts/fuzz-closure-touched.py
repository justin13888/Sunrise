#!/usr/bin/env python3
"""Decide whether a commit reaches what `fuzz/`'s clippy lints, for pre-commit.

Usage: fuzz-closure-touched.py [FILE...]

Prints `run` or `skip` on stdout and the reason on stderr. With no FILE it
answers for the staged changes (`git diff --cached`); naming files answers for
those paths instead. `mise run rust-clippy-fuzz-staged` reads it.
`mise run rust-clippy`, `mise run pre-commit` and CI never read this script:
they always lint `fuzz/`.

Why the hook asks
-----------------

`fuzz/` is its own workspace (`fuzz/Cargo.toml`), with its own lockfile and its
own `fuzz/target/`, so its clippy rebuilds the shared dependency graph a second
time. That lint exists so a harness cannot quietly stop exercising what its
name says (#167), and only a change to the harnesses or to the code they
compile can make one do that.

What reaches it
---------------

- Anything under `fuzz/`, which takes in `fuzz/Cargo.toml` and `fuzz/Cargo.lock`.
- The root `Cargo.toml`, `Cargo.lock`, `clippy.toml` (its lookup walks up out
  of `fuzz/`), `.cargo/` and `rust-toolchain*`.
- A file under the directory of a workspace crate the fuzz package compiles:
  its path dependencies, read from
  `cargo metadata --no-deps --manifest-path fuzz/Cargo.toml`, and their
  normal and build dependencies among the workspace's packages, transitively,
  read from the workspace's own `cargo metadata --no-deps`. A crate's
  dev-dependencies are not compiled for a dependent, so they are not followed.

Where either `cargo metadata` fails, the answer is `run`: a hook that cannot
tell lints rather than skips.
"""

from __future__ import annotations

import json
import os
import posixpath
import subprocess
import sys

GLOBAL = ("Cargo.toml", "Cargo.lock", "clippy.toml")
GLOBAL_DIRS = ("fuzz", ".cargo")
GLOBAL_PREFIX = ("rust-toolchain",)


def under(path: str, prefix: str) -> bool:
    return path == prefix or path.startswith(prefix + "/")


def rel(path: str, root: str) -> str:
    """`path` relative to `root`, both resolved, so a symlinked checkout
    still compares equal to the paths `cargo metadata` prints."""
    return posixpath.relpath(os.path.realpath(path), os.path.realpath(root))


def closure_dirs(fuzz_meta: dict, ws_meta: dict, repo: str) -> dict[str, str]:
    """Repository-relative directory of every crate the fuzz package compiles,
    outside `fuzz/` itself, mapped to the crate's name."""
    seeds = {
        rel(dep["path"], repo): dep["name"]
        for pkg in fuzz_meta["packages"]
        for dep in pkg["dependencies"]
        if dep.get("path")
    }
    members = set(ws_meta["workspace_members"])
    by_dir: dict[str, dict] = {}
    for pkg in ws_meta["packages"]:
        if pkg["id"] in members:
            by_dir[rel(posixpath.dirname(pkg["manifest_path"]), repo)] = pkg
    by_name = {pkg["name"]: d for d, pkg in by_dir.items()}
    reached = dict(seeds)
    todo = [d for d in seeds if d in by_dir]
    while todo:
        pkg = by_dir[todo.pop()]
        for dep in pkg["dependencies"]:
            if dep.get("kind") == "dev" or dep["name"] not in by_name:
                continue
            d = by_name[dep["name"]]
            if d not in reached:
                reached[d] = dep["name"]
                todo.append(d)
    return reached


def owner(dirs: dict[str, str], path: str) -> str | None:
    """The deepest closure directory holding `path`."""
    best = None
    for d in dirs:
        if under(path, d) and (best is None or len(d) > len(best)):
            best = d
    return best


def decide(changed: list[str], dirs: dict[str, str] | None) -> tuple[bool, str]:
    """Whether to lint `fuzz/`, and why. `dirs` is None when the closure is
    unknown."""
    for path in changed:
        if path in GLOBAL or any(under(path, d) for d in GLOBAL_DIRS) or path.startswith(GLOBAL_PREFIX):
            return True, f"{path} reaches the fuzz workspace"
    if not changed:
        return False, "nothing is staged"
    if dirs is None:
        return True, "the fuzz crate closure is unknown; linting it"
    for path in changed:
        d = owner(dirs, path)
        if d:
            return True, f"{path} is in {dirs[d]}, which the fuzz package compiles"
    return False, f"no change reaches the {len(dirs)} crates the fuzz package compiles"


def run(repo: str, *args: str) -> str | None:
    done = subprocess.run(args, cwd=repo, capture_output=True, text=True)
    return done.stdout if done.returncode == 0 else None


def metadata(repo: str, *manifest: str) -> dict | None:
    out = run(repo, "cargo", "metadata", "--no-deps", "--format-version", "1", *manifest)
    return json.loads(out) if out else None


def main(argv: list[str]) -> int:
    top = run(".", "git", "rev-parse", "--show-toplevel")
    if top is None:
        raise SystemExit("fuzz-closure-touched: not inside a git repository")
    repo = top.strip()
    if len(argv) > 1:
        changed = sorted(set(argv[1:]))
    else:
        out = run(repo, "git", "-c", "core.quotePath=false", "diff", "--cached", "--name-only", "--no-renames")
        if out is None:
            raise SystemExit("fuzz-closure-touched: git diff --cached failed")
        changed = sorted({line for line in out.splitlines() if line})
    fuzz_meta = metadata(repo, "--manifest-path", "fuzz/Cargo.toml")
    ws_meta = metadata(repo)
    dirs = closure_dirs(fuzz_meta, ws_meta, repo) if fuzz_meta and ws_meta else None
    lint, why = decide(changed, dirs)
    print(f"fuzz-closure-touched: {why}", file=sys.stderr)
    print("run" if lint else "skip")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
