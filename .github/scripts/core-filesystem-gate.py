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

How `#[cfg(test)]` is skipped, and why it is not a `break`
---------------------------------------------------------

The first version of this scanner stopped reading a file at the first line
beginning `#[cfg(test)]`. That held on one big `engine.rs` with its tests in a
single block at the bottom, and stopped holding the moment those files were
split: the split modules carry `#[cfg(test)] use ...` item attributes near the
*top*, so the scan ended in the imports and the gate shipped green while reading
almost nothing. Measured on the split tree it stopped at line 19 of a 603-line
`engine/oplog.rs`, at 107 of a 2401-line `keychain/mod.rs`, and at 93 of a
555-line `engine/mod.rs` — 97%, 95% and 83% of those files unread.

So the attribute is resolved against the item it is attached to:

* `#[cfg(test)] use ...;`, `#[cfg(test)] fn helper() { ... }` — one item is
  skipped and the scan continues after it.
* `#[cfg(test)] mod tests { ... }` — the braced body is skipped and the scan
  continues after its closing brace. An inline test module is not the end of a
  file; it is just the largest item.
* `#[cfg(test)] mod tests;` — the body is another file. That file is test code
  in its entirety and carries no marker of its own, so `scan` resolves the
  declaration to a path and skips the file. After the engine split this is how
  `engine/tests.rs` — 10,539 lines of it — reaches the scanner.

Nothing here is a heuristic about where tests "usually" live. Every rule above
is the meaning of the attribute.

What checks this gate
---------------------

`SELF_TESTS` below runs before every scan and rejects a scanner that has gone
blind again. `.github/scripts/test_core_filesystem_gate.py` is the rest of it:
the exit codes CI reads, asserted against real files in a temp crate, run by
the `core-filesystem-gate-contract` job ("Core filesystem gate contract") on
every trigger this workflow has -- and by `mise run core-filesystem-gate-test`.
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

# What rule 2 covers, and what it does not
# ----------------------------------------
#
# Rule 2 is "no filesystem access except via `CoreConfig::storage`". Everything
# below performs a filesystem operation. Three things that look adjacent are
# deliberately absent, because the rule is about *access*:
#
# * `std::env::temp_dir` reads `TMPDIR`; it touches no filesystem. Code that
#   goes on to use the path it returns is caught by the `fs::`/`File::` rules
#   on the line that does the accessing, which is the line worth naming.
# * `std::process::Command` spawns a process. That is not filesystem access,
#   and rule 4 is about threads, not subprocesses; `std::process::exit` is
#   already a `clippy.toml` entry. Banning it here would be widening rule 2 by
#   the back door rather than changing the rule.
# * `Path::exists`/`is_file`/`is_dir` do stat the filesystem, but the names are
#   common enough on non-path receivers to make a line-based gate noisy, and
#   nothing they gate can be acted on without one of the calls below.
BANNED = re.compile(
    r"""
      \b(?:std|tokio)::fs::
    | (?<![.\w])fs::(?:read|write|create_dir|create_dir_all|canonicalize
                      |remove_file|remove_dir|remove_dir_all|rename|copy
                      |metadata|symlink_metadata|read_dir|read_link|
                      read_to_string|set_permissions)\b
    | \bOpenOptions\b
    | (?<![.\w])File::(?:open|create|create_new)\b
    # The method spellings of the free functions above. `p.read_dir()` is the
    # same syscall as `fs::read_dir(p)` and the `(?<![.\w])` guard on the rule
    # above exists precisely to *not* match it, so it needs naming here.
    | \.(?:read_dir|canonicalize|metadata|symlink_metadata|read_link)\s*\(
    """,
    re.VERBOSE,
)

CFG_TEST = re.compile(r"^#\[cfg\(test\)\]")

# `#[cfg(test)] mod <name>;` — a test module whose body is a separate file.
CFG_TEST_MOD_FILE = re.compile(r"^(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_]\w*)\s*;")


def code_only(raw: str) -> str:
    """`raw` with line comments and string literals blanked out.

    Only used for brace counting, where a `{` inside a SQL string or a `//`
    comment would end an item early and hand the rest of a test module back to
    the scanner as production code.
    """
    out: list[str] = []
    i, n = 0, len(raw)
    while i < n:
        c = raw[i]
        if c == "/" and raw.startswith("//", i):
            break
        if c == "r" and (m := re.match(r'r(#*)"', raw[i:])):
            close = '"' + m.group(1)
            end = raw.find(close, i + m.end())
            if end == -1:
                return "".join(out)  # raw string runs past this line
            i = end + len(close)
            continue
        if c == '"':
            i += 1
            while i < n:
                if raw[i] == "\\":
                    i += 2
                    continue
                if raw[i] == '"':
                    i += 1
                    break
                i += 1
            continue
        out.append(c)
        i += 1
    return "".join(out)


def skip_item(lines: list[str], start: int) -> int:
    """Index of the first line after the item beginning at `lines[start]`.

    An item ends either at the `}` closing its body or at the `;` of a bodyless
    one (`use ...;`, `mod tests;`). Both forms appear under `#[cfg(test)]` in
    this crate, and the difference is the whole point of the distinction this
    gate needs to draw.
    """
    depth = 0
    opened = False
    i = start
    while i < len(lines):
        code = code_only(lines[i])
        i += 1
        for ch in code:
            if ch == "{":
                depth += 1
                opened = True
            elif ch == "}":
                depth -= 1
        if depth > 0:
            continue
        if opened or ";" in code:
            return i
    return i


def production_lines(text: str) -> list[tuple[int, str]]:
    """Lines outside comments and outside every `#[cfg(test)]` item.

    A `#[cfg(test)]` attribute applies to the one item that follows it, so that
    item is what gets skipped -- not the remainder of the file. See the module
    docstring for what that distinction cost while it was missing.
    """
    out: list[tuple[int, str]] = []
    lines = text.splitlines()
    in_block_comment = False
    pending_cfg_test = False
    i = 0
    while i < len(lines):
        raw = lines[i]
        line = raw.strip()
        n = i + 1
        i += 1

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

        if CFG_TEST.match(line):
            pending_cfg_test = True
            continue

        if pending_cfg_test:
            # Blank lines and further attributes still belong to the same item.
            if not line or line.startswith("#["):
                continue
            pending_cfg_test = False
            i = skip_item(lines, i - 1)
            continue

        out.append((n, raw))
    return out


def cfg_test_module_files(path: Path) -> set[Path]:
    """Files declared by `#[cfg(test)] mod <name>;` in `path`.

    Resolved the way rustc resolves them: `<name>.rs` beside a `mod.rs`, or in
    the directory named after a non-`mod.rs` parent.
    """
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines()
    parent = path.parent if path.name == "mod.rs" else path.parent / path.stem
    found: set[Path] = set()
    pending = False
    for raw in lines:
        line = raw.strip()
        if CFG_TEST.match(line):
            pending = True
            continue
        if not pending or not line or line.startswith(("//", "#[")):
            continue
        pending = False
        if m := CFG_TEST_MOD_FILE.match(line):
            for candidate in (parent / f"{m.group(1)}.rs", parent / m.group(1) / "mod.rs"):
                if candidate.is_file():
                    found.add(candidate)
    return found


def scan(root: Path) -> list[tuple[Path, int, str]]:
    files = sorted(root.rglob("*.rs"))
    # A file that exists only as the body of a `#[cfg(test)] mod x;` is test
    # code from its first line and says so nowhere inside itself.
    test_only: set[Path] = set()
    for path in files:
        test_only |= cfg_test_module_files(path)

    hits: list[tuple[Path, int, str]] = []
    for path in files:
        rel = path.relative_to(root).as_posix()
        if rel in ALLOWED or path in test_only:
            continue
        for n, line in production_lines(path.read_text(encoding="utf-8")):
            if BANNED.search(line):
                hits.append((path, n, line.strip()))
    return hits


# (source, whether the gate must flag a *production* line in it).
#
# Multi-line entries exist because the interesting cases are all about where a
# `#[cfg(test)]` item ends -- a one-line fixture cannot express that.
SELF_TESTS: list[tuple[str, bool]] = [
    ("    std::fs::read(p)?;", True),
    ("    let f = OpenOptions::new().read(true).open(p)?;", True),
    ("    File::open(p)?;", True),
    ("    fs::create_dir_all(d)?;", True),
    ("    fs::read_to_string(p)?;", True),
    ("    tokio::fs::read(p).await?;", True),
    ("    for e in dir.read_dir()? {}", True),
    ("    let real = p.canonicalize()?;", True),
    ("/// Enforced by `std::fs::File::try_lock`, which is an advisory lock.", False),
    ("// fs::write(p, b) would be a violation, which is why this is a comment", False),
    ("    self.storage.read(p)?;", False),  # the injected seam is the point
    ("    let f = self.fs::read;", False),
    ("    cfg.storage.write(name, bytes)?;", False),
    # Out of scope on purpose -- see the note above BANNED.
    ("    let base = std::env::temp_dir();", False),
    ("    std::process::Command::new(prog).status()?;", False),
    # A `#[cfg(test)]` on one item skips that item and nothing more. Each of
    # these would have been a false negative under the old `break`.
    ("#[cfg(test)]\nuse super::ids::hex_short;\nstd::fs::read(p)?;", True),
    ("#[cfg(test)]\nmod tests;\nstd::fs::read(p)?;", True),
    ("#[cfg(test)]\nfn helper() {\n    std::fs::read(p)?;\n}", False),
    ("#[cfg(test)]\nfn helper() {\n    let _ = 1;\n}\nstd::fs::read(p)?;", True),
    ("#[cfg(test)]\nmod tests {\n    std::fs::read(p)?;\n}", False),
    ("#[cfg(test)]\nmod tests {\n    let _ = 1;\n}\nstd::fs::read(p)?;", True),
    # An attribute stack between the cfg and its item.
    ("#[cfg(test)]\n#[allow(dead_code)]\nuse std::fs;\nstd::fs::read(p)?;", True),
    # Braces the brace counter must not be fooled by.
    ('#[cfg(test)]\nmod tests {\n    let q = "{";\n}\nstd::fs::read(p)?;', True),
    ('#[cfg(test)]\nmod tests {\n    let q = r#"{"#;\n}\nstd::fs::read(p)?;', True),
    ("#[cfg(test)]\nmod tests {\n    // }\n}\nstd::fs::read(p)?;", True),
    # A comment naming the attribute is not the attribute.
    ("// `open_op_row` is `#[cfg(test)]`.\nstd::fs::read(p)?;", True),
]


def flags(src: str) -> bool:
    """Whether the gate reports `src` -- the question `scan` actually asks.

    It is not `BANNED.search(src) and production_lines(src)`: that says "there
    is a banned spelling somewhere, and some line survived the comment filter",
    which is true of a test module with any ordinary `use` above it. The line
    that matches has to be one of the lines that survived.
    """
    return any(BANNED.search(line) for _, line in production_lines(src))


def self_test() -> None:
    failures = 0
    for src, should_flag in SELF_TESTS:
        got = flags(src)
        if got != should_flag:
            failures += 1
            print(
                f"::error::core-filesystem: self-test failed on {src!r} "
                f"(expected flag={should_flag}, got {got})"
            )
    if failures:
        sys.exit(1)
    print(f"OK: core-filesystem self-test clean ({len(SELF_TESTS)} cases).")


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
