#!/usr/bin/env python3
"""Fail when a proptest under a crate's `tests/` does not set `failure_persistence`.

Why this gate exists
--------------------

#118 settled the counterexample convention. A shrunken counterexample is the one
output of a property test that cannot be regenerated on demand, so the files are
**committed**; they live at `<crate>/proptest-regressions/<source path>.txt`;
and every proptest under a crate's `tests/` sets

    FileFailurePersistence::Direct("proptest-regressions/tests/<name>.txt")

explicitly, because proptest's default — `SourceParallel` — walks up from the
source looking for a directory holding `lib.rs` or `main.rs`, an integration
test has none above it, and proptest then prints `failed to find lib.rs or
main.rs` and falls back to a flat `<name>.proptest-regressions` beside the test.

`.gitignore` deliberately carries no rule for either shape, so that the flat
file shows up in `git status` as the symptom. Nothing asserted the setting
itself. A new proptest added to a `tests/` file without it writes the flat file,
and the only thing between that and a silent divergence is somebody noticing an
untracked file during review — which is the "a human will spot it" enforcement
this repository has replaced with a gate everywhere else it has written one.

What this checks
----------------

For every `.rs` file under a workspace crate's `tests/` directory that invokes
`proptest!`:

* **`config`** — every `proptest!` block carries a `#![proptest_config(…)]`
  inner attribute. This is the shape a new block is added in, and the shape that
  goes missing.
* **`direct`** — the file names
  `FileFailurePersistence::Direct("proptest-regressions/tests/<name>.txt")`,
  with `<name>` derived from the file's own path rather than matched loosely, so
  a copy-pasted block that still names the file it came from is a violation
  rather than a pass.

And, for every such file, whether or not it invokes `proptest!`:

* **`ignored`** — git does not ignore the expected persistence path. The
  convention is that these files are tracked; a `.gitignore` rule would hide
  them and quietly reintroduce the thing #118 removed.
* **`untracked`** — a persistence file that exists is tracked. One sitting
  untracked is a counterexample the suite found and nobody committed.
* **`flat`** — no `<name>.proptest-regressions` sits beside a test file. That is
  proptest's fallback output, and its presence means the setting was missing
  when the suite last failed.

Scope
-----

**In:** `.rs` files directly under a workspace crate's `tests/` directory, which
is where cargo puts integration-test targets and where the `SourceParallel`
default misbehaves.

**Out, and deliberately:**

* **Proptests under `src/`.** The default works there — `crates/sunrise-core`'s
  `proptest-regressions/engine.txt` is what it produces — so requiring an
  explicit setting would be requiring a workaround for a problem that does not
  exist.
* **What a `#![proptest_config(f())]` helper actually returns.** The `direct`
  rule reads the file, not the call graph: `op_envelope_proptest.rs` builds its
  config in a `fn config()` beside the block, which is a reasonable thing to do
  and not something this script follows. So the pair of rules says "every block
  is configured, and this file names the right persistence path" rather than
  "every block persists to the right path". Following the call would mean
  evaluating Rust, and a gate that half-followed it would report coverage it
  does not have.
* **A `TestRunner` constructed by hand.** No test in this workspace does it. One
  that did would set persistence through the same `ProptestConfig` and would be
  unscanned here, which is worth knowing.

Usage: proptest-persistence-gate.py [--root PATH] [--self-test]
Exit 0 clean, 1 on a violation, 2 if the gate could not run at all.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from dataclasses import dataclass

PROPTEST_BANG = re.compile(r"(?<![A-Za-z0-9_])proptest\s*!")
PROPTEST_CONFIG = re.compile(r"#!\s*\[\s*proptest_config\s*\(")
DIRECT = re.compile(r'FileFailurePersistence\s*::\s*Direct\s*\(\s*"(?P<path>[^"]*)"')


@dataclass(frozen=True)
class Finding:
    path: str
    line: int
    code: str
    message: str


def blank_comments_and_strings(text: str) -> str:
    """Blank comments and string contents, preserving every byte offset.

    Brace matching and macro search both need this: a `proptest!` inside a doc
    comment is prose, and a `{` inside a string literal is a character. Newlines
    survive so a line number still comes out of an offset.
    """
    out: list[str] = []
    i = 0
    n = len(text)
    while i < n:
        ch = text[i]
        # Raw string: `r`, zero or more `#`, then a quote.
        if ch == "r":
            j = i + 1
            while j < n and text[j] == "#":
                j += 1
            if j < n and text[j] == '"':
                hashes = j - i - 1
                out.append(text[i : j + 1])
                i = j + 1
                closer = '"' + "#" * hashes
                end = text.find(closer, i)
                end = n if end < 0 else end
                out.append("".join("\n" if c == "\n" else " " for c in text[i:end]))
                if end < n:
                    out.append(closer)
                i = end + len(closer)
                continue
        if ch == '"':
            out.append('"')
            i += 1
            while i < n:
                if text[i] == "\\" and i + 1 < n:
                    out.append("  " if text[i + 1] != "\n" else " \n")
                    i += 2
                    continue
                if text[i] == '"':
                    out.append('"')
                    i += 1
                    break
                out.append("\n" if text[i] == "\n" else " ")
                i += 1
            continue
        if ch == "'":
            # A char literal or a lifetime. Only the literal has a closing quote
            # within three characters, and blanking a lifetime would be harmless
            # anyway since neither carries a brace.
            out.append("'")
            i += 1
            continue
        if text.startswith("//", i):
            end = text.find("\n", i)
            end = n if end < 0 else end
            out.append(" " * (end - i))
            i = end
            continue
        if text.startswith("/*", i):
            depth = 1
            j = i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth += 1
                    j += 2
                elif text.startswith("*/", j):
                    depth -= 1
                    j += 2
                else:
                    j += 1
            out.append("".join("\n" if c == "\n" else " " for c in text[i:j]))
            i = j
            continue
        out.append(ch)
        i += 1
    return "".join(out)


def block_span(masked: str, start: int) -> tuple[int, int] | None:
    """The `{ … }` following `start`, as offsets, or None if it never closes."""
    open_at = masked.find("{", start)
    if open_at < 0:
        return None
    depth = 0
    for index in range(open_at, len(masked)):
        if masked[index] == "{":
            depth += 1
        elif masked[index] == "}":
            depth -= 1
            if depth == 0:
                return (open_at, index)
    return None


def expected_persistence(rel_path: str, crate_dir: str) -> str:
    """`proptest-regressions/<source path>.txt`, relative to the crate root."""
    inside = os.path.relpath(rel_path, crate_dir)
    return f"proptest-regressions/{os.path.splitext(inside)[0]}.txt"


def scan_source(rel_path: str, crate_dir: str, text: str) -> list[Finding]:
    """Every `config` and `direct` finding in one test file."""
    masked = blank_comments_and_strings(text)
    found: list[Finding] = []
    want = expected_persistence(rel_path, crate_dir)

    blocks = list(PROPTEST_BANG.finditer(masked))
    if not blocks:
        return found

    for match in blocks:
        line = masked.count("\n", 0, match.start()) + 1
        span = block_span(masked, match.end())
        if span is None:
            found.append(
                Finding(
                    rel_path,
                    line,
                    "config",
                    "this `proptest!` invocation's braces never close, so the gate cannot "
                    "read it. Use the braced form.",
                )
            )
            continue
        body = masked[span[0] : span[1]]
        if not PROPTEST_CONFIG.search(body):
            found.append(
                Finding(
                    rel_path,
                    line,
                    "config",
                    "this `proptest!` block sets no `#![proptest_config(…)]`, so proptest "
                    "falls back to `SourceParallel`, fails to find a `lib.rs` above a "
                    f"`tests/` file, and writes a flat `<name>.proptest-regressions` "
                    f'beside it. Add `failure_persistence: Some(Box::new(FileFailurePersistence'
                    f'::Direct("{want}")))`.',
                )
            )

    hits = [m.group("path") for m in DIRECT.finditer(text)]
    if not hits:
        found.append(
            Finding(
                rel_path,
                blocks[0].start() and masked.count("\n", 0, blocks[0].start()) + 1,
                "direct",
                "this file invokes `proptest!` and names no "
                f'`FileFailurePersistence::Direct("{want}")`. See '
                "docs/10-cross-cutting/testing.md section 2 for why the default does not "
                "work under `tests/`.",
            )
        )
    elif want not in hits:
        found.append(
            Finding(
                rel_path,
                text.count("\n", 0, DIRECT.search(text).start()) + 1,
                "direct",
                f"persists to {hits[0]!r}, but this file's counterexamples belong at "
                f"{want!r}. The path is relative to the crate root, which is the working "
                "directory cargo gives a test binary — a copied block that still names "
                "the file it came from silently shares its counterexamples.",
            )
        )
    return found


def git_lines(root: str, *args: str) -> list[str]:
    out = subprocess.run(
        ["git", "-C", root, *args],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    return [line for line in out.split("\0") if line]


def check(root: str) -> int:
    try:
        tracked = git_lines(root, "ls-files", "-z")
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"::error::proptest-persistence: could not list tracked files: {error}")
        return 2

    tracked_set = set(tracked)
    # Crate directories, from the tree rather than a hardcoded list: any tracked
    # `<dir>/tests/<name>.rs` makes `<dir>` a crate with integration tests.
    test_files = sorted(
        f for f in tracked if f.endswith(".rs") and len(f.split("/")) >= 3 and f.split("/")[-2] == "tests"
    )
    if not test_files:
        print("::error::proptest-persistence: no `<crate>/tests/*.rs` found; the scan is broken.")
        return 2

    findings: list[Finding] = []
    with_proptests = 0
    for rel_path in test_files:
        crate_dir = os.path.dirname(os.path.dirname(rel_path))
        try:
            with open(os.path.join(root, rel_path), encoding="utf-8") as handle:
                text = handle.read()
        except (OSError, UnicodeDecodeError) as error:
            print(f"::error::proptest-persistence: {rel_path}: unreadable ({error})")
            return 2

        found = scan_source(rel_path, crate_dir, text)
        uses_proptest = bool(PROPTEST_BANG.search(blank_comments_and_strings(text)))
        if uses_proptest:
            with_proptests += 1
        findings += found

        want = expected_persistence(rel_path, crate_dir)
        want_rel = f"{crate_dir}/{want}"
        ignored = subprocess.run(
            ["git", "-C", root, "check-ignore", "-q", "--no-index", want_rel],
            capture_output=True,
            check=False,
        )
        if ignored.returncode == 0:
            findings.append(
                Finding(
                    want_rel,
                    1,
                    "ignored",
                    "git ignores this path. Counterexample files are tracked on purpose — a "
                    "shrunken case is the one test input the suite cannot regenerate. Remove "
                    "the `.gitignore` rule rather than the file.",
                )
            )
        if os.path.exists(os.path.join(root, want_rel)) and want_rel not in tracked_set:
            findings.append(
                Finding(
                    want_rel,
                    1,
                    "untracked",
                    "this counterexample file exists and git does not track it. Commit it; it "
                    "is replayed before anything new is generated on every later run.",
                )
            )

        flat = f"{os.path.splitext(rel_path)[0]}.proptest-regressions"
        if os.path.exists(os.path.join(root, flat)):
            findings.append(
                Finding(
                    flat,
                    1,
                    "flat",
                    "proptest's fallback output, which means a block in the test beside it was "
                    "missing `failure_persistence` when the suite last failed. Fix the setting, "
                    f"then move the case into {want_rel}.",
                )
            )

    for finding in sorted(findings, key=lambda f: (f.path, f.line, f.code)):
        print(
            f"::error file={finding.path},line={finding.line}::"
            f"proptest-persistence[{finding.code}]: {finding.path}:{finding.line}: {finding.message}"
        )
    if findings:
        print(f"::error::proptest-persistence: {len(findings)} problem(s).")
        return 1
    print(
        f"OK: proptest-persistence clean ({with_proptests} proptest file(s) of "
        f"{len(test_files)} integration tests)."
    )
    return 0


# --------------------------------------------------------------------------
# Self-test. Runs as a precondition of every check. Most of these fixtures are
# shapes the gate must *reject*: a gate nobody has watched fail is a gate nobody
# knows works.
# --------------------------------------------------------------------------

CLEAN = '''
//! A test file. This comment says `proptest!` and must not count as one.

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/redaction.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]

    #[test]
    fn holds(s in "[a-z]{1,4}") {
        prop_assert!(!s.is_empty(), "a brace { in a string is not a brace");
    }
}
'''

# The convention's own alternative: the config comes from a helper beside the
# block, which is what `op_envelope_proptest.rs` does.
CLEAN_HELPER = '''
fn config() -> ProptestConfig {
    ProptestConfig {
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/redaction.txt",
            ),
        )),
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(config())]
    #[test]
    fn holds(x in 0u8..4) { prop_assert!(x < 4); }
}
'''

NO_CONFIG = '''
proptest! {
    #[test]
    fn holds(x in 0u8..4) { prop_assert!(x < 4); }
}
'''

WRONG_PATH = '''
proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/somewhere_else.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]
    #[test]
    fn holds(x in 0u8..4) { prop_assert!(x < 4); }
}
'''

# Two blocks, the second added later without a config: the shape #127 is about.
SECOND_BLOCK = CLEAN + NO_CONFIG

NOT_A_PROPTEST = '''
//! Mentions proptest! only in prose, and has no block.

#[test]
fn ordinary() {}
'''


def self_test() -> int:
    failures = 0

    def expect(name: str, text: str, codes: list[str]) -> None:
        nonlocal failures
        actual = sorted(f.code for f in scan_source("crates/c/tests/redaction.rs", "crates/c", text))
        if actual != sorted(codes):
            print(f"::error::proptest-persistence self-test: {name} yielded {actual}, expected {sorted(codes)}")
            failures += 1

    expect("CLEAN", CLEAN, [])
    expect("CLEAN_HELPER", CLEAN_HELPER, [])
    expect("NO_CONFIG", NO_CONFIG, ["config", "direct"])
    expect("WRONG_PATH", WRONG_PATH, ["direct"])
    expect("SECOND_BLOCK", SECOND_BLOCK, ["config"])
    expect("NOT_A_PROPTEST", NOT_A_PROPTEST, [])

    # The expected path is derived from the file, not matched loosely.
    if expected_persistence("crates/c/tests/redaction.rs", "crates/c") != "proptest-regressions/tests/redaction.txt":
        print("::error::proptest-persistence self-test: the expected path is wrong")
        failures += 1
    if expected_persistence("crates/c/tests/deep/x.rs", "crates/c") != "proptest-regressions/tests/deep/x.txt":
        print("::error::proptest-persistence self-test: a nested test path is wrong")
        failures += 1

    # The masker is what keeps prose and string contents out of the scan.
    masked = blank_comments_and_strings('let s = "} proptest! {"; // proptest! {\nlet t = 1;')
    if "proptest" in masked or masked.count("\n") != 1 or "let t = 1;" not in masked:
        print(f"::error::proptest-persistence self-test: masking produced {masked!r}")
        failures += 1
    if len(masked) != len('let s = "} proptest! {"; // proptest! {\nlet t = 1;'):
        print("::error::proptest-persistence self-test: masking moved an offset")
        failures += 1

    # A block whose braces never close is reported rather than silently passed.
    unclosed = [f.code for f in scan_source("crates/c/tests/redaction.rs", "crates/c", "proptest! {\n#[test]\n")]
    if unclosed != ["config", "direct"]:
        print(f"::error::proptest-persistence self-test: an unclosed block read as {unclosed}")
        failures += 1

    if failures:
        return 1
    print("OK: proptest-persistence self-test clean (11 cases).")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Check proptest counterexample persistence.")
    parser.add_argument("--root", default=None, help="repository root (default: the working tree this script is in)")
    parser.add_argument("--self-test", action="store_true", help="assert the rules and exit")
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    if args.root:
        root = args.root
    else:
        try:
            root = subprocess.run(
                ["git", "rev-parse", "--show-toplevel"],
                capture_output=True,
                text=True,
                check=True,
            ).stdout.strip()
        except (OSError, subprocess.CalledProcessError) as error:
            print(f"::error::proptest-persistence: not inside a git work tree: {error}")
            return 2

    failed = self_test()
    if failed:
        return failed
    return check(root)


if __name__ == "__main__":
    sys.exit(main())
