#!/usr/bin/env python3
"""Fail when `sunrise-core` reaches the filesystem outside its injected seam.

Why this gate exists
--------------------

`docs/01-architecture/shared-core.md` §determinism-rules states four rules the
shared core holds to. Rule 2 is "no filesystem access except via
`CoreConfig::storage`", and until this script existed it was enforced by nothing.

The document said otherwise. It named `std::fs::*` as an entry in
`clippy.toml`'s deny list, and that entry has never been there. So the rule read
as gated while being held up by review alone -- the worst of the two states,
because a reader who checks the enforcement story finds a claim rather than a
check.

Why this is not in `clippy.toml`
--------------------------------

Because it cannot be. `clippy.toml` is a single workspace-level file with no
per-crate scoping, and nine of the twenty crates here touch the filesystem on
purpose -- `sunrise-storage` opens the vault, `sunrise-cli` writes the keystore,
`sunrise-server` stores blobs, `sunrise-log` writes logs. A `disallowed-methods`
entry for `std::fs` would fire in eight crates the rule was never about, and the
only way to quiet them is an `#[allow]` at every legitimate call site in all of
them.

`clippy.toml` already records where that road ends: the `rand` rules carry a
comment about warning noise that "reads like the rule is dead". A rule whose
enforcement has to be suppressed in eight of nine places it fires is that rule.

So this gate is scoped the way the rule is scoped: one crate.

The allowlist is a baseline, and it may only shrink
---------------------------------------------------

Two files in `sunrise-core` reach the filesystem directly today. Both are
deliberate, both are documented, and both are named below with the reason. That
list is the debt this gate tolerates, and it is visible here rather than hidden
in a config so that anyone reading the gate reads the exceptions too.

Adding a path to it is not a fix. A new entry means the core grew a filesystem
dependency outside its seam, which is the thing rule 2 exists to prevent.

What counts as a violation
--------------------------

A direct spelling of the standard filesystem surface: `std::fs::…`, a bare
`fs::…` call, `OpenOptions`, or `File::open`/`File::create`. Comment lines are
skipped, so prose that merely *names* one -- `lib.rs`'s module doc explains the
advisory lock in terms of `std::fs::File::try_lock` -- does not red the build.

`#[cfg(test)]` code is skipped too. Rule 2 is about what the core does when it
runs, and a test that writes a fixture into a `tempdir` is not the core reaching
past its storage handle. `vault_lock.rs`'s own tests do exactly that.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

CRATE_SRC = Path("crates/sunrise-core/src")

# path -> why this file is permitted to reach the filesystem directly.
# This list may shrink. It may not grow without the rule itself changing.
ALLOWED: dict[str, str] = {
    "vault_lock.rs": (
        "The OS advisory lock on `<vault>/core.lock`. It is the cross-process "
        "half of the single-writer guarantee, so it cannot go through "
        "`CoreConfig::storage` -- the point is to hold a lock the OS releases "
        "on process death by any means, including a kill. Documented in "
        "shared-core.md §Single-writer guarantee."
    ),
    "config.rs": (
        "Reads `/etc/localtime` to recover the host's IANA zone *name*. "
        "`TimeZone::system()` returns a nameless zero-offset zone on macOS, "
        "where the link points into a versioned tzdb tree, so the previous "
        "fallback made every Mac claim to be in UTC. This reads the zone's "
        "name, not its rules -- jiff's bundled tzdb keeps resolution "
        "deterministic. Documented in docs/implementation/overview.md."
    ),
}

BANNED = re.compile(
    r"""
      \bstd::fs::
    | (?<![.\w])fs::(?:read|write|create_dir|create_dir_all|canonicalize
                      |remove_file|remove_dir|remove_dir_all|rename|copy
                      |metadata|symlink_metadata|read_dir|read_link|
                      read_to_string|set_permissions)\b
    | \bOpenOptions\b
    | (?<![.\w])File::(?:open|create|create_new)\b
    """,
    re.VERBOSE,
)


def production_lines(text: str) -> list[tuple[int, str]]:
    """Lines outside comments and outside the first `#[cfg(test)]` region."""
    out: list[tuple[int, str]] = []
    in_block_comment = False
    for n, raw in enumerate(text.splitlines(), 1):
        line = raw.strip()
        if line.startswith("#[cfg(test)]"):
            break
        if in_block_comment:
            if "*/" in line:
                in_block_comment = False
            continue
        if line.startswith("/*"):
            if "*/" not in line:
                in_block_comment = True
            continue
        if line.startswith("//"):
            continue
        out.append((n, raw))
    return out


def scan(root: Path) -> list[tuple[Path, int, str]]:
    hits: list[tuple[Path, int, str]] = []
    for path in sorted(root.rglob("*.rs")):
        rel = path.relative_to(root).as_posix()
        if rel in ALLOWED:
            continue
        for n, line in production_lines(path.read_text(encoding="utf-8")):
            if BANNED.search(line):
                hits.append((path, n, line.strip()))
    return hits


SELF_TESTS: list[tuple[str, bool]] = [
    ("    std::fs::read(p)?;", True),
    ("    let f = OpenOptions::new().read(true).open(p)?;", True),
    ("    File::open(p)?;", True),
    ("    fs::create_dir_all(d)?;", True),
    ("    fs::read_to_string(p)?;", True),
    ("/// Enforced by `std::fs::File::try_lock`, which is an advisory lock.", False),
    ("// fs::write(p, b) would be a violation, which is why this is a comment", False),
    ("    self.storage.read(p)?;", False),  # the injected seam is the point
    ("    let f = self.fs::read;", False),
    ("    cfg.storage.write(name, bytes)?;", False),
]


def self_test() -> None:
    for src, should_flag in SELF_TESTS:
        got = bool(BANNED.search(src)) and bool(production_lines(src))
        if got != should_flag:
            print(
                f"::error::core-filesystem: self-test failed on {src!r} "
                f"(expected flag={should_flag}, got {got})"
            )
            sys.exit(1)
    # a `#[cfg(test)]` line stops the scan
    assert production_lines("#[cfg(test)]\nstd::fs::read(p);") == []
    print(f"OK: core-filesystem self-test clean ({len(SELF_TESTS) + 1} cases).")


def main() -> int:
    self_test()

    if not CRATE_SRC.is_dir():
        print(f"::error::core-filesystem: {CRATE_SRC} does not exist; gate cannot run.")
        return 1

    for name in ALLOWED:
        if not (CRATE_SRC / name).exists():
            print(
                f"::error::core-filesystem: allowlisted path {name} no longer "
                "exists. Remove it from ALLOWED -- a stale entry silently "
                "widens the gate."
            )
            return 1

    hits = scan(CRATE_SRC)
    if hits:
        print(
            "::error::core-filesystem: sunrise-core reaches the filesystem "
            "outside `CoreConfig::storage`."
        )
        for path, n, line in hits:
            print(f"  {path}:{n}: {line}")
        print()
        print(
            "Rule 2 of docs/01-architecture/shared-core.md: the core reads and "
            "writes through the injected storage handle, so a test can supply "
            "one and a vault is the only thing on disk."
        )
        print(
            "If this really is a new, deliberate exception, it needs the rule "
            "changed and a reason recorded -- not an entry added to ALLOWED."
        )
        return 1

    allowed = ", ".join(sorted(ALLOWED))
    print(
        f"OK: core-filesystem clean — {CRATE_SRC} reaches the filesystem only "
        f"in its {len(ALLOWED)} documented exceptions ({allowed})."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
