#!/usr/bin/env python3
"""Decide which tests a push can affect, for a scoped local test run.

The pre-push hook used to call this; since #1117 it runs no test suite, and
this script serves `mise run rust-test-changed` and `mise run test-changed`,
which a developer runs by hand before pushing.

Usage: prepush-scope.py rust|bun [FILE...]

Naming files answers for those paths instead of for the push. Prints one line on stdout and its reasons on stderr:

- `rust`: `none`, `all`, or the `-p NAME` arguments of the workspace packages
  to test, sorted. `mise run rust-test-changed` reads it.
- `bun`: `none` or `all`. `mise run test-changed` reads it.

`SUNRISE_FULL_PREPUSH=1` makes both print `all`. `mise run pre-push`,
`mise run rust-test` and CI never read this script: they always run
everything.

What a push changes
-------------------

The files that differ between HEAD and its merge base with `@{push}`, or with
the remote's default branch when the branch has no push destination yet. Where
neither resolves, nothing can be told about the push and both answers are
`all`. Renames count as a deletion and an addition, so both paths are seen.

What a change reaches
---------------------

Rust. A file under a workspace package's directory reaches that package and,
through `cargo metadata`, every workspace package that depends on it (normal,
dev or build dependency), transitively. A change to the workspace manifest,
`Cargo.lock`, `.cargo/`, `rust-toolchain*` or `clippy.toml` reaches all of
them. A file a package's sources name in a string literal - an `include_str!`,
a fixture a test opens, a tree a lint test walks (`"crates"`, `"docs/..."`) -
reaches that package, and not its dependents, whose code did not change.

Bun. The vitest suite runs when a changed file is inside a Bun workspace
member (`apps/*` or `packages/*` with a `package.json`), is a root file the
suite is configured or pinned by, is a `*.test.ts`/`*.spec.ts` anywhere outside
`crates/`, or is named in a string literal of a workspace member's TS/JS
sources. That last rule is what keeps the cross-language gates on the hook: the
token TOML (`packages/sunrise-ui-tokens/tokens/`), the string catalog
(`i18n/`, named by `packages/sunrise-i18n/src/catalog.ts`), the Rust files the
token and relay-URL tests read, and `docs/`, whose tree `apps/docs/site.ts`
builds the sidebar from.

A path built at run time from pieces no literal names is invisible to the
literal rule. That is the gap CI's whole-workspace run closes; this script only
decides what the hook runs.
"""

from __future__ import annotations

import json
import os
import posixpath
import re
import subprocess
import sys
from collections.abc import Iterable
from dataclasses import dataclass, field

# Root paths whose change reaches every Rust package.
RUST_GLOBAL = ("Cargo.toml", "Cargo.lock", ".cargo", "clippy.toml")
RUST_GLOBAL_PREFIX = ("rust-toolchain",)

# Root paths the vitest suite is configured, resolved or pinned by. `mise.toml`
# pins the Node vitest runs under.
BUN_GLOBAL = (
    "package.json",
    "bun.lock",
    "bunfig.toml",
    "tsconfig.base.json",
    "vitest.config.ts",
    "mise.toml",
)

TS_SUFFIXES = (".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs")
SKIP_DIRS = {"node_modules", "target", "dist", "build", ".git", ".claude"}

# A quoted string. Rust only quotes strings with `"`; TS also uses `'` and `` ` ``.
RUST_LITERAL = re.compile(r'"((?:[^"\\\n]|\\.)*)"')
TS_LITERAL = re.compile(r"""(["'`])((?:(?!\1)[^\\\n]|\\.)*)\1""")
PATHISH = re.compile(r"^[A-Za-z0-9_.@/-]+$")
# The text before a literal that is an argument of a path join.
JOINED = re.compile(r"\bjoin\([^()]*$")


@dataclass
class Package:
    name: str
    dir: str  # relative to the repository root, no trailing slash
    deps: set[str] = field(default_factory=set)  # workspace packages only


def run(repo: str, *args: str) -> str | None:
    """stdout of a command in `repo`, or None when it fails."""
    done = subprocess.run(args, cwd=repo, capture_output=True, text=True)
    return done.stdout.strip() if done.returncode == 0 else None


def full_requested(env: dict[str, str]) -> bool:
    return env.get("SUNRISE_FULL_PREPUSH", "").strip().lower() not in ("", "0", "false", "no")


def push_base(repo: str) -> tuple[str, str] | None:
    """The commit this push's changes are measured from, and the ref it came from."""
    refs = ["@{push}"]
    head = run(repo, "git", "symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD")
    refs += [head] if head else ["origin/main", "origin/master"]
    for ref in refs:
        sha = run(repo, "git", "rev-parse", "--verify", "-q", f"{ref}^{{commit}}")
        if not sha:
            continue
        base = run(repo, "git", "merge-base", "HEAD", sha)
        if base:
            return base, ref
    return None


def changed_files(repo: str, base: str) -> list[str]:
    out = run(repo, "git", "-c", "core.quotePath=false", "diff", "--name-only", "--no-renames", base, "HEAD")
    if out is None:
        raise SystemExit(f"prepush-scope: git diff {base} HEAD failed")
    return sorted({line for line in out.splitlines() if line})


def under(path: str, prefix: str) -> bool:
    return path == prefix or path.startswith(prefix + "/")


def workspace(repo: str) -> dict[str, Package]:
    out = run(repo, "cargo", "metadata", "--no-deps", "--format-version", "1")
    if out is None:
        raise SystemExit("prepush-scope: cargo metadata failed")
    return packages_from_metadata(json.loads(out))


def packages_from_metadata(meta: dict) -> dict[str, Package]:
    root = meta["workspace_root"].rstrip("/")
    members = set(meta["workspace_members"])
    packages = {}
    for pkg in meta["packages"]:
        if pkg["id"] not in members:
            continue
        directory = posixpath.relpath(posixpath.dirname(pkg["manifest_path"]), root)
        packages[pkg["name"]] = Package(pkg["name"], directory)
    for pkg in meta["packages"]:
        if pkg["name"] in packages:
            packages[pkg["name"]].deps = {
                d["name"] for d in pkg["dependencies"] if d["name"] in packages
            }
    return packages


def dependents(packages: dict[str, Package], seeds: Iterable[str]) -> set[str]:
    """`seeds` and every workspace package that depends on one, transitively."""
    reached = set(seeds)
    grew = True
    while grew:
        grew = False
        for pkg in packages.values():
            if pkg.name not in reached and pkg.deps & reached:
                reached.add(pkg.name)
                grew = True
    return reached


def owner(packages: dict[str, Package], path: str) -> str | None:
    """The package whose directory holds `path`, the deepest one if nested."""
    best = None
    for pkg in packages.values():
        if under(path, pkg.dir) and (best is None or len(pkg.dir) > len(packages[best].dir)):
            best = pkg.name
    return best


def literal_targets(literal: str, file_dir: str, home: str) -> set[str]:
    """Repository paths a string literal in a file under `home` may name.

    A literal is taken as root-relative, and when it starts with `.` also as
    relative to the file and to its package or workspace member (`include_str!`
    resolves from the file, `env!("CARGO_MANIFEST_DIR")` and `new URL(...)`
    from the manifest or the module). The leading `../` and `/` are also
    dropped, which catches a path joined onto a root found at run time.
    """
    if not PATHISH.match(literal):
        return set()
    bare = literal.lstrip("/")
    targets = set()
    if bare.startswith("."):
        for start in (file_dir, home):
            targets.add(posixpath.normpath(posixpath.join(start, bare)))
        while bare.startswith(("./", "../", "/")):
            bare = bare.split("/", 1)[1]
    targets.add(posixpath.normpath(bare) if bare else "")
    return {t for t in targets if t not in ("", ".") and not t.startswith("..")}


def scan(repo: str, roots: Iterable[str], suffixes: tuple[str, ...]) -> Iterable[str]:
    """Repository-relative paths of source files under `roots`."""
    for top in roots:
        for here, dirs, files in os.walk(os.path.join(repo, top)):
            dirs[:] = sorted(d for d in dirs if d not in SKIP_DIRS)
            for name in sorted(files):
                if name.endswith(suffixes):
                    yield os.path.relpath(os.path.join(here, name), repo).replace(os.sep, "/")


def references(text: str, pattern: re.Pattern[str], group: int, path: str, home: str) -> set[str]:
    """Every path the string literals of a source file may name."""
    found: set[str] = set()
    file_dir = posixpath.dirname(path)
    for line in text.splitlines():
        code = line.lstrip()
        if code.startswith(("//", "*", "/*")):
            continue
        for match in pattern.finditer(code):
            literal = match.group(group)
            # A bare word ("docs", "crates") is a path only where it is joined
            # onto one; anywhere else it is far likelier a name or a label.
            if not re.search(r"[./]", literal) and not JOINED.search(code[: match.start()]):
                continue
            found |= literal_targets(literal, file_dir, home)
    return found


def named(changed: Iterable[str], targets: set[str]) -> list[str]:
    """The changed paths a literal names, itself or a directory holding it."""
    hits = []
    for path in changed:
        parts = path.split("/")
        if any("/".join(parts[:i]) in targets for i in range(1, len(parts) + 1)):
            hits.append(path)
    return hits


def rust_scope(
    changed: list[str], packages: dict[str, Package], refs: dict[str, set[str]]
) -> tuple[set[str] | None, list[str]]:
    """The packages to test (None meaning all of them), and why."""
    why = []
    for path in changed:
        if path in RUST_GLOBAL or under(path, ".cargo") or path.startswith(RUST_GLOBAL_PREFIX):
            return None, [f"{path} reaches every crate"]
    owned = {}
    for path in changed:
        pkg = owner(packages, path)
        if pkg:
            owned.setdefault(pkg, path)
    scope = dependents(packages, owned)
    for pkg in sorted(owned):
        why.append(f"{pkg}: changed ({owned[pkg]})")
    for pkg in sorted(scope - set(owned)):
        why.append(f"{pkg}: depends on a changed crate")
    for pkg in sorted(refs):
        hits = named(changed, refs[pkg])
        if hits and pkg not in scope:
            scope.add(pkg)
            why.append(f"{pkg}: its sources name {hits[0]}")
    if scope == set(packages):
        return None, why
    return scope, why


def bun_members(repo: str) -> list[str]:
    return [
        f"{top}/{name}"
        for top in ("apps", "packages")
        if os.path.isdir(os.path.join(repo, top))
        for name in sorted(os.listdir(os.path.join(repo, top)))
        if os.path.isfile(os.path.join(repo, top, name, "package.json"))
    ]


def bun_reason(changed: list[str], members: list[str], refs: set[str]) -> str | None:
    """Why the vitest suite must run, or None when nothing it reads changed."""
    for path in changed:
        if path in BUN_GLOBAL:
            return f"{path} configures the suite"
        member = next((m for m in members if under(path, m)), None)
        if member:
            return f"{path} is in {member}"
        if path.endswith((".test.ts", ".spec.ts")) and not under(path, "crates"):
            return f"{path} is a test file"
    hits = named(changed, refs)
    return f"a TS source names {hits[0]}" if hits else None


def main(argv: list[str], env: dict[str, str]) -> int:
    if len(argv) < 2 or argv[1] not in ("rust", "bun"):
        print(__doc__.split("\n\n")[1], file=sys.stderr)
        return 2
    side = argv[1]
    say = lambda line: print(f"prepush-scope({side}): {line}", file=sys.stderr)  # noqa: E731
    if full_requested(env):
        say("SUNRISE_FULL_PREPUSH is set; running everything")
        print("all")
        return 0
    repo = run(".", "git", "rev-parse", "--show-toplevel")
    if repo is None:
        raise SystemExit("prepush-scope: not inside a git repository")
    if len(argv) > 2:
        changed = sorted(set(argv[2:]))
        say(f"{len(changed)} file(s) named on the command line")
    else:
        found = push_base(repo)
        if found is None:
            say("no @{push} and no origin default branch to compare with; running everything")
            print("all")
            return 0
        base, ref = found
        changed = changed_files(repo, base)
        say(f"{len(changed)} file(s) changed since the merge base with {ref} ({base[:12]})")
    if side == "bun":
        members = bun_members(repo)
        refs: set[str] = set()
        for path in scan(repo, members, TS_SUFFIXES):
            home = next(m for m in members if under(path, m))
            with open(os.path.join(repo, path), encoding="utf-8", errors="replace") as f:
                refs |= references(f.read(), TS_LITERAL, 2, path, home)
        reason = bun_reason(changed, members, refs)
        say(reason or "nothing the vitest suite reads changed")
        print("all" if reason else "none")
        return 0
    packages = workspace(repo)
    crate_refs: dict[str, set[str]] = {}
    for pkg in packages.values():
        for path in scan(repo, [pkg.dir], (".rs",)):
            if owner(packages, path) != pkg.name:
                continue
            with open(os.path.join(repo, path), encoding="utf-8", errors="replace") as f:
                crate_refs.setdefault(pkg.name, set()).update(
                    references(f.read(), RUST_LITERAL, 1, path, pkg.dir)
                )
    scope, why = rust_scope(changed, packages, crate_refs)
    for line in why:
        say(line)
    if scope is None:
        say("testing the whole workspace")
        print("all")
    elif not scope:
        say("no crate is reached; skipping the Rust tests")
        print("none")
    else:
        say(f"testing {len(scope)} of {len(packages)} crates")
        print(" ".join(f"-p {name}" for name in sorted(scope)))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv, dict(os.environ)))
