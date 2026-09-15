#!/usr/bin/env python3
"""Fail when a backticked `path:line` citation in prose points at nothing.

Why this gate exists
--------------------

The dominant way this repository cites its own source is an inline code
span — `crates/sunrise-core/src/engine/sync.rs:199`, `docs/03-crypto/recovery.md`,
`crates/sunrise-sync/src/backoff.rs:31-63`. There are close to a thousand of
them, in the docs and in Rust doc comments both, and until this script not one
was checked by anything.

That is not an oversight in the two gates that look closest. `docs-link-gate.py`
blanks inline code before it extracts links, and `doc-comment-gate.py` does the
same before it looks for a stray marker — both deliberately, so a document
*about* markdown is not read as containing the markup it quotes. The consequence
is structural rather than accidental: a code span is exactly the shape those two
agree to ignore, and it is exactly the shape this repository cites paths in. A
file moves, a module is split, a migration is collapsed into a baseline, and
every citation to it rots in silence. A `:199` rots faster still — the file
survives the edit, the line does not, and nothing renders differently.

This gate reads what those two mask. It does not change them; the three are
complementary, and the contract tests beside this one (`test_docs_link_gate.py`,
`test_doc_comment_gate.py`) pin the masking behaviour so it stays that way.

The rule
--------

A **citation** is an inline code span whose *entire* content is a repository
path, optionally followed by `:LINE` or `:LINE-LINE`. Three conditions, each
decidable without an opinion about prose:

1. The span content, after CommonMark's one-space strip, matches
   `<path>(:<start>(-<end>)?)?` and holds nothing else — no spaces, no trailing
   word, no section reference. `` `see crates/foo.rs` `` is not a citation.
2. The final path segment carries an extension beginning with a letter. This is
   what separates `crates/sunrise-core/src/lib.rs` from `Vec<u8>`,
   `cargo test --workspace`, `sunrise_core::engine` and `0.1.0` without the gate
   needing to know anything about Rust, shells or semver.
3. The path is **claimed** by an anchor (below). An unclaimed path is not
   checked, and is counted and reported rather than dropped.

Anchors
-------

A citation is resolved against the repository root first: `docs/…`, `crates/…`,
`.github/…`, `Cargo.toml`. It is claimed by that anchor when its first segment
names a top-level entry git tracks.

A citing file that lives under `crates/<name>/` gets a **second anchor at its
own crate root**, and a path claimed by either is resolved against both. This is
not a guess: Cargo fixes the layout, so `tests/live_sync.rs` written inside
`crates/sunrise-cli` means `crates/sunrise-cli/tests/live_sync.rs` and can mean
nothing else. Thirteen doc comments in this workspace cite that way, and reading
them against the root alone would report every one of them — against a top-level
`tests/` directory that holds a chaos harness and nothing they could mean.

Adding an anchor can only *remove* failures, never invent one, so a second
anchor is safe in a way a looser *shape* rule would not be.

A citation fails when no anchor resolves it, when a cited line is past the end
of the resolved file, when a range is empty (`:50-40`), when line 0 is cited, or
when a line is cited on a directory. Every failure is reported with the citing
file and its line; the gate exits 1 if any failed and 0 with a count when clean.

Ambiguity, and what is deliberately not checked
-----------------------------------------------

Around 1,500 path-shaped spans in this repository are claimed by neither anchor.
`recovery.md` appears 22 times, `main.rs` 15, `Views/TaskEditorView.swift:97`
and `api/observe.rs` in the same style — shorthand for a path the surrounding
paragraph has already established. Resolving those would mean guessing, and a
gate that guesses wrong fails a correct document, which is the one thing a gate
may not do: `recovery.md` in `docs/03-crypto/` is a sibling and `recovery.md` in
`docs/06-server/auth.md` is not. So the rule is the narrow one, and the cases it
declines are **counted and named**, not silently skipped: every run prints how
many spans went unchecked, and `--list-unanchored` prints each one with its file
and line. The hole is visible in the gate's own output rather than implied by
its silence.

Also out, each for a reason:

* **`legacy/`**, its markdown and its Rust. It is the pre-rewrite tree kept
  verbatim, and its READMEs cite `apps/api/src/server.ts` — a path that *does*
  resolve against this repository's `apps/` and means something else entirely
  there. Scanning it would produce confident nonsense.
* **Directory citations with no extension** (`crates/sunrise-core/src/engine`).
  Rule 2 excludes them, so a renamed directory is not caught. A path *with* an
  extension that turns out to be a directory — an `.xcodeproj` bundle, say;
  this tree generates rather than tracks one today — is accepted as a
  directory, and only a line number on one is an error.
* **Fenced code blocks and YAML front matter** in markdown, and fenced blocks
  inside Rust doc comments. Backticks in there are literal, not code spans.
* **Indented code blocks** in markdown are *not* masked. Deciding whether four
  columns is a code block or a list continuation takes the container bookkeeping
  `docs-link-gate.py` carries, and the payoff here is nil: a real path in an
  indented block resolves and passes, so only a fictional one would cost
  anything, and the tree holds none.
* **Spans that pair across a line break.** A path holds no space and CommonMark
  turns a newline inside a code span into one, so a citation is single-line by
  construction. Scanning line by line also stops a stray backtick from pairing
  with one three paragraphs away.
* **Python docstrings under `.github/scripts/`**, which cite paths in this exact
  style — this one included. They are neither markdown nor Rust doc comments,
  and pulling them in would put this file's own prose under its own rule. Worth
  knowing; recorded here rather than left implied.
* **Whether the cited line still says what the citing sentence claims.** No tool
  decides that. This gate answers only "does that line exist".

Two in-file lists, and what separates them
------------------------------------------

`ALLOWED` excuses a citation that is *correct because the path resolves to
nothing* — an ADR naming what its own decision deleted, a page saying in bold
that a specified test does not exist. Repointing one would make the sentence
false.

`DEFERRED` records a citation that is simply wrong and has not been fixed yet.
It is a baseline in the sense `file-size-gate.py` uses the word: it may shrink,
it may not grow, and every run prints what is on it. Both lists are keyed on
`(citing file, cited path)` so an entry excuses one sentence rather than a
spelling everywhere it appears, and both are guarded twice — the gate exits 2 if
a listed path becomes tracked, and exits 2 if a listed citation leaves the file
it names, so an entry deletes itself the moment its reason stops holding.

Usage: citation-gate.py [--root PATH] [--list-unanchored] [--self-test]
Exit 0 clean, 1 on a dangling citation, 2 if the gate could not run at all.
"""

from __future__ import annotations

import argparse
import posixpath
import re
import subprocess
import sys
from dataclasses import dataclass

# A code span, CommonMark's rule: a run of N backticks closed by a run of
# exactly N. Single-line on purpose -- see the docstring.
CODE_SPAN = re.compile(r"(?P<ticks>`+)(?!`)(?P<body>[^\n]+?)(?<!`)(?P=ticks)(?!`)")

# The whole span, or it is not a citation. The extension is a shape rather than
# an enumerated list, so a file type this repository has not used yet is covered
# the day it appears; the anchor rule is what supplies the precision. It must
# start with a letter, which is what keeps `0.1.0` from reading as a path.
CITATION = re.compile(
    r"""
    ^
    (?P<path>
        [A-Za-z0-9_.][A-Za-z0-9_.+-]*
        (?: / [A-Za-z0-9_.+-]+ )*
        \. [A-Za-z] [A-Za-z0-9]{0,11}
    )
    (?: : (?P<start>[0-9]{1,9}) (?: - (?P<end>[0-9]{1,9}) )? )?
    $
    """,
    re.VERBOSE,
)

FENCE = re.compile(r"^[ \t]{0,3}(?P<char>`{3,}|~{3,})(?P<info>.*)$")
# `////` is an ordinary comment, not a doc comment, so the lookahead matters.
# The same MARKER `doc-comment-gate.py` uses, which is this repository's
# enforced definition of a doc comment.
MARKER = re.compile(r"^(?P<indent>[ \t]*)(?P<marker>///(?!/)|//!)(?P<body>.*)$")

CRATE_PREFIX = re.compile(r"^(crates/[^/]+)/")

# Citations this gate accepts against a path git does not track, keyed by
# `(citing file, cited path)` so an entry excuses one sentence rather than a
# spelling everywhere it appears.
#
# The admission test is narrow, because an allowlist is how a gate turns into
# something people argue with instead of fix: the citing prose must *assert the
# absence*. An ADR recording what its own decision deleted, and a status page
# saying in bold that a specified test does not exist, are citations that are
# correct precisely because the path resolves to nothing — repointing them would
# make the sentence false. A citation that merely rotted is not admissible here;
# that one gets fixed.
#
# Two staleness guards make an entry self-deleting: the gate exits 2 if a listed
# path becomes tracked, and exits 2 if a listed citation is no longer in the
# file it names. Neither the tree nor the document can drift out from under this
# list unnoticed.
ALLOWED: dict[tuple[str, str], str] = {
    (
        "docs/05-sync/transports.md",
        "crates/sunrise-server/src/ws.rs",
    ): "the sentence is that this module was deleted with the WebSocket; it names what is gone",
    (
        "docs/06-server/observability.md",
        "crates/sunrise-server/tests/metric-label-safety.rs",
    ): "the page says in bold that this specified CI test does not exist",
    (
        "docs/06-server/observability.md",
        "tests/span-redaction.rs",
    ): "same paragraph shape: the specified tracing test was never written",
    (
        "docs/11-adr/0019-swiftui-macos-client.md",
        "docs/07-clients/tui.md",
    ): "ADR-0019 deletes this spec; both of its citations record what it removed",
    (
        "docs/11-adr/0013-focus-session-op-representation.md",
        "crates/sunrise-storage/migrations/0010_focus_sessions.sql",
    ): "the migration ADR-0013 added, collapsed into 0013_baseline.sql by ADR-0018",
    (
        "docs/11-adr/0014-entity-level-lww-merge.md",
        "crates/sunrise-storage/migrations/0006_lww_metadata.sql",
    ): "likewise, and the entry itself notes the collapse into the baseline",
    (
        "docs/11-adr/0022-device-signature-canonical-json.md",
        "crates/sunrise-server/src/auth/device_sig.rs",
    ): "ADR-0022's Context describes the v1 module its own decision removed",
    (
        "docs/implementation/overview.md",
        "tests/ws_cursors.rs",
    ): "the sentence is 'they were this file until ADR-0023'; the marker survives, the file does not",
}

# Citations that are simply **wrong** and have not been fixed yet, keyed the
# same way. This is a baseline in the sense `file-size-gate.py` uses the word:
# it may shrink and it may not grow, every run prints what is on it, and the
# staleness guards below delete an entry the moment the citation it names is
# corrected.
#
# It exists so a dangling citation found by a change set that may not edit the
# file it lives in is *recorded in the gate* rather than lost in a report. An
# entry is a debt with an address, not an exemption: the difference from
# `ALLOWED` above is that these sentences are false, and fixing one is the only
# thing that removes it.
DEFERRED: dict[tuple[str, str], str] = {
    (
        "crates/sunrise-cli/tests/cli.rs",
        "docs/07-clients/tui.md",
    ): "a live doc comment pointing at the spec ADR-0019 deleted; repoint it at "
       "docs/07-clients/parity-matrix.md, which carries the Focus mode MUST",
}


@dataclass(frozen=True)
class Span:
    """One inline code span, and the line a reader would look on."""

    line: int
    body: str


@dataclass(frozen=True)
class Finding:
    file: str
    line: int
    span: str
    message: str


def blank(line: str) -> str:
    """A same-length run of spaces, so masking moves no column."""
    return " " * len(line)


def mask_markdown(text: str) -> list[str]:
    """Blank front matter and fenced code, one entry per source line.

    Everything else survives, headings and block quotes included: a citation in
    a heading or inside a quoted paragraph is still a citation.
    """
    lines = text.split("\n")
    out: list[str] = []
    fence: str | None = None
    # Front matter only when the opening delimiter is actually closed, so a file
    # that opens with a `---` thematic break does not lose its whole body.
    front = bool(lines) and lines[0].rstrip() == "---" and any(
        line.rstrip() in {"---", "..."} for line in lines[1:]
    )

    for index, raw in enumerate(lines):
        if front:
            out.append(blank(raw))
            if index > 0 and raw.rstrip() in {"---", "..."}:
                front = False
            continue

        match = FENCE.match(raw)
        if fence is None:
            if match:
                fence = match.group("char")
                out.append(blank(raw))
            else:
                out.append(raw)
            continue

        out.append(blank(raw))
        if (
            match
            and match.group("char")[0] == fence[0]
            and len(match.group("char")) >= len(fence)
            and not match.group("info").strip()
        ):
            fence = None

    return out


def mask_rust(text: str) -> list[str]:
    """Keep doc comment prose only, blanking the markers and their fences.

    Ordinary code, ordinary `//` comments, `////` non-doc comments and fenced
    examples inside a doc comment all come back as spaces. Fence state belongs
    to one run of one marker kind, so an unclosed fence in one block cannot
    swallow the next.
    """
    out: list[str] = []
    fence: str | None = None
    previous: str | None = None

    for raw in text.split("\n"):
        match = MARKER.match(raw)
        if not match:
            out.append(blank(raw))
            fence = None
            previous = None
            continue

        marker = match.group("marker")
        if marker != previous:
            fence = None
        previous = marker

        body = match.group("body")
        opener = FENCE.match(body.strip())

        if fence is not None:
            out.append(blank(raw))
            if (
                opener
                and opener.group("char")[0] == fence[0]
                and len(opener.group("char")) >= len(fence)
                and not opener.group("info").strip()
            ):
                fence = None
            continue

        if opener:
            fence = opener.group("char")
            out.append(blank(raw))
            continue

        out.append(blank(match.group("indent") + marker) + body)

    return out


def code_spans(masked: list[str]) -> list[Span]:
    """Every inline code span left standing, with its 1-based line."""
    found: list[Span] = []
    for index, line in enumerate(masked):
        for match in CODE_SPAN.finditer(line):
            body = match.group("body")
            # CommonMark strips one leading and one trailing space when both are
            # present, which is how a literal backtick is written.
            if len(body) >= 2 and body.startswith(" ") and body.endswith(" ") and body.strip():
                body = body[1:-1]
            found.append(Span(line=index + 1, body=body))
    return found


def scan_file(name: str, text: str) -> list[Span]:
    """The code spans of one file, read according to its extension."""
    masked = mask_markdown(text) if name.endswith(".md") else mask_rust(text)
    return code_spans(masked)


def line_count(path: str) -> int:
    """Lines in a file, counted the way an editor numbers them.

    Read as bytes: the answer is a newline tally, and a citation must not be
    able to stop the gate by naming a file that is not UTF-8.
    """
    with open(path, "rb") as handle:
        data = handle.read()
    if not data:
        return 0
    return data.count(b"\n") + (0 if data.endswith(b"\n") else 1)


def git_tracked(root: str) -> list[str]:
    """Every path git tracks, which is what "this file exists" has to mean.

    A citation in committed prose to a file that is not committed is dead for
    every reader on github.com. Comparing against the index rather than the
    filesystem is also exact-case, so `ReadMe.md` for `README.md` fails on a
    case-insensitive macOS checkout exactly as it fails on the Linux runner —
    and a build artefact under `target/` cannot make a citation pass locally
    that would fail in CI.
    """
    try:
        listed = subprocess.run(
            ["git", "-C", root, "ls-files", "-z"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"::error::citations: `git ls-files` failed: {error}")
        raise SystemExit(2) from error
    return [name for name in listed.split("\0") if name]


class Tree:
    """The tracked tree, indexed the three ways `classify` asks about it."""

    def __init__(self, tracked: list[str]) -> None:
        self.files = set(tracked)
        self.dirs: set[str] = set()
        self.children: dict[str, set[str]] = {}
        for name in tracked:
            parent = posixpath.dirname(name)
            self.children.setdefault(parent, set()).add(posixpath.basename(name))
            while parent:
                grandparent = posixpath.dirname(parent)
                self.dirs.add(parent)
                self.children.setdefault(grandparent, set()).add(posixpath.basename(parent))
                parent = grandparent

    def anchors(self, citing: str) -> list[str]:
        """Where a citation in `citing` may be resolved from, root first."""
        found = [""]
        crate = CRATE_PREFIX.match(citing)
        if crate:
            found.append(crate.group(1))
        return found

    def claims(self, anchor: str, first_segment: str) -> bool:
        """Whether `anchor` holds an entry the citation's first segment names."""
        return first_segment in self.children.get(anchor, ())

    @staticmethod
    def resolve(anchor: str, path: str) -> str:
        return posixpath.join(anchor, path) if anchor else path


def classify(span: Span, citing: str, root: str, tree: Tree) -> tuple[str, Finding | None]:
    """Decide what one span is: `"skip"`, `"unanchored"` or `"checked"`.

    A `"checked"` verdict carries a `Finding` when the citation is broken and
    `None` when it resolves.
    """
    match = CITATION.match(span.body)
    if not match:
        return "skip", None

    path = match.group("path")
    if path.startswith("./"):
        path = path[2:]
    first = path.split("/", 1)[0]

    anchors = tree.anchors(citing)
    if not any(tree.claims(anchor, first) for anchor in anchors):
        return "unanchored", None

    def broken(message: str) -> tuple[str, Finding]:
        return "checked", Finding(file=citing, line=span.line, span=span.body, message=message)

    start = match.group("start")
    end = match.group("end")
    first_line = int(start) if start is not None else None
    last_line = int(end) if end is not None else first_line

    if first_line is not None:
        if first_line == 0:
            return broken("cites line 0; line numbers start at 1.")
        if last_line is not None and last_line < first_line:
            return broken(f"cites an empty range ({first_line}-{last_line}).")

    resolved = [Tree.resolve(anchor, path) for anchor in anchors]
    target = next((candidate for candidate in resolved if candidate in tree.files), None)

    if target is None:
        if any(candidate in tree.dirs for candidate in resolved):
            if first_line is None:
                return "checked", None
            return broken(f"cites a line, but `{path}` is a directory.")
        if (citing, path) in ALLOWED or (citing, path) in DEFERRED:
            return "checked", None
        return broken("names no file git tracks.")

    if first_line is None:
        return "checked", None

    total = line_count(posixpath.join(root, target))
    if last_line is not None and last_line > total:
        where = f"line {last_line}" if end is None else f"lines {first_line}-{last_line}"
        return broken(f"cites {where}, but `{target}` has {total} line(s).")

    return "checked", None


def check(root: str, list_unanchored: bool) -> int:
    tracked = git_tracked(root)
    files = sorted(
        name
        for name in tracked
        if (name.endswith(".md") or name.endswith(".rs")) and not name.startswith("legacy/")
    )
    if not files:
        print("::error::citations: git reports no markdown or Rust to scan; the gate could not run.")
        return 2

    tree = Tree(tracked)

    for name, listed in (("allowlisted", ALLOWED), ("deferred", DEFERRED)):
        for (citing, path), reason in sorted(listed.items()):
            if path in tree.files:
                print(
                    f"::error::citations: `{path}` is {name} for {citing} ({reason}) but git "
                    "tracks it now; delete the entry."
                )
                return 2

    findings: list[Finding] = []
    unanchored: list[Finding] = []
    used: set[tuple[str, str]] = set()
    checked = 0

    for name in files:
        try:
            with open(posixpath.join(root, name), encoding="utf-8") as handle:
                text = handle.read()
        except (OSError, UnicodeDecodeError) as error:
            print(f"::error::citations: could not read {name}: {error}")
            return 2

        for span in scan_file(name, text):
            verdict, finding = classify(span, name, root, tree)
            if verdict == "skip":
                continue
            if verdict == "unanchored":
                unanchored.append(Finding(file=name, line=span.line, span=span.body, message=""))
                continue
            checked += 1
            cited = CITATION.match(span.body)
            if cited and (name, cited.group("path")) in (ALLOWED | DEFERRED):
                used.add((name, cited.group("path")))
            if finding is not None:
                findings.append(finding)

    for finding in findings:
        print(
            f"::error file={finding.file},line={finding.line}::citations: "
            f"`{finding.span}` {finding.message}"
        )

    stale = sorted((set(ALLOWED) | set(DEFERRED)) - used)
    for citing, path in stale:
        print(
            f"::error::citations: the allowlist carries `{path}` in {citing}, but no such "
            "citation is there any more; delete the entry."
        )

    for (citing, path), reason in sorted(DEFERRED.items()):
        print(f"citations: deferred — {citing} cites `{path}`, which resolves to nothing: {reason}.")

    print(f"citations: {checked} anchored citation(s) across {len(files)} file(s).")
    if unanchored:
        print(
            f"citations: {len(unanchored)} path-like span(s) are claimed by no anchor and were "
            "NOT checked; re-run with --list-unanchored to see them."
        )
        if list_unanchored:
            for note in unanchored:
                print(f"  {note.file}:{note.line}: `{note.span}`")

    if stale:
        return 2
    if findings:
        print(f"::error::citations: {len(findings)} citation(s) resolve to nothing.")
        return 1
    print("OK: citations clean.")
    return 0


# --------------------------------------------------------------------------
# Self-test. Runs as a precondition of every check, so a change to the rules
# fails loudly rather than quietly starting to report good citations as broken
# -- or, worse, quietly starting to read nothing as a citation at all.
# --------------------------------------------------------------------------

MARKDOWN_FIXTURE = '''---
title: front matter, which is not prose
cited: `crates/never/scanned.rs`
---

# A heading citing `docs/in-a-heading.md`

Prose citing `crates/sunrise-core/src/lib.rs:12` and `docs/plain.md`, plus a
range `crates/a.rs:31-63`.

> A block quote citing `docs/quoted.md`.

Not citations: `Vec<u8>`, `cargo test --workspace`, `see docs/prose.md`,
`sunrise_core::engine`, `--all-features`, `0.1.0`, `#[derive(Debug)]`.

Unanchored, so not checked: `recovery.md`, `../06-server/api.md`,
`Views/TaskEditorView.swift:97`, `api/observe.rs`.

```text
Inside a fence, so invisible: `crates/inside/a/fence.rs`
```

A ``span with `backticks` inside`` and a literal `` ` `` are left alone.
'''

RUST_FIXTURE = '''
//! Module prose citing `crates/sunrise-core/src/lib.rs:12`.
//!
//! ```rust
//! // inside a fence: `crates/inside/a/fence.rs`
//! let x = 1;
//! ```

// An ordinary comment citing `crates/ordinary/comment.rs`.
//// Not a doc comment either: `crates/four/slashes.rs`

/// Item prose citing `docs/plain.md` and not `Vec<u8>`.
pub fn f() {
    let _ = "crates/a/string/literal.rs";
}
'''

# A tree with one crate, so the second anchor has something to resolve against,
# and a top-level `tests/`, which is what makes the two anchors disagree.
FIXTURE_TRACKED = [
    "Cargo.toml",
    "docs/03-crypto/recovery.md",
    "crates/sunrise-cli/src/main.rs",
    "crates/sunrise-cli/tests/cli.rs",
    "tests/chaos/README.md",
    "apps/apple/Sunrise.xcodeproj/project.pbxproj",
]


def self_test() -> int:
    failures = 0
    tree = Tree(FIXTURE_TRACKED)

    markdown = [span.body for span in scan_file("docs/fixture.md", MARKDOWN_FIXTURE)]
    for absent in ("crates/never/scanned.rs", "crates/inside/a/fence.rs"):
        if absent in markdown:
            print(f"::error::citations self-test: `{absent}` was not masked out of the markdown fixture")
            failures += 1
    for present in (
        "docs/in-a-heading.md",
        "crates/sunrise-core/src/lib.rs:12",
        "docs/plain.md",
        "crates/a.rs:31-63",
        "docs/quoted.md",
    ):
        if present not in markdown:
            print(f"::error::citations self-test: `{present}` was not extracted from the markdown fixture")
            failures += 1

    rust = sorted(span.body for span in scan_file("crates/c/src/fixture.rs", RUST_FIXTURE))
    wanted = sorted(["crates/sunrise-core/src/lib.rs:12", "docs/plain.md", "Vec<u8>"])
    if rust != wanted:
        print(f"::error::citations self-test: the Rust fixture yielded {rust}, expected {wanted}")
        failures += 1

    def verdict(body: str, citing: str = "docs/x.md") -> str:
        got, _ = classify(Span(line=1, body=body), citing, ".", tree)
        return got

    def finding_for(body: str, citing: str = "docs/x.md") -> Finding | None:
        _, found = classify(Span(line=1, body=body), citing, ".", tree)
        return found

    # Near misses. Each is a real span somewhere in this tree, and none of them
    # is a citation: a gate that fires on all of these is as useless as one that
    # fires on none.
    for body in (
        "Vec<u8>",
        "cargo test --workspace",
        "see docs/prose.md",
        "sunrise_core::engine",
        "--all-features",
        "0.1.0",
        "#[derive(Debug)]",
        "crates/sunrise-cli/src",  # a directory, and no extension
        "docs/a.md and docs/b.md",
    ):
        if verdict(body) != "skip":
            print(f"::error::citations self-test: `{body}` was read as a citation")
            failures += 1

    # Path-shaped, but claimed by no anchor: reported as unchecked, not guessed.
    for body in ("recovery.md", "../06-server/api.md", "Views/TaskEditorView.swift:97", "api/observe.rs"):
        if verdict(body) != "unanchored":
            print(f"::error::citations self-test: `{body}` classified {verdict(body)!r}, expected 'unanchored'")
            failures += 1

    # The crate anchor. The same span is dangling from a document and resolved
    # from inside the crate whose layout Cargo fixes -- and `tests/cli.rs` is the
    # shape that makes the difference, because a top-level `tests/` exists.
    if verdict("tests/cli.rs") != "checked":
        print("::error::citations self-test: a top-level `tests/` path was not claimed by the root anchor")
        failures += 1
    if finding_for("tests/cli.rs") is None:
        print("::error::citations self-test: `tests/cli.rs` resolved from a document, where it cannot")
        failures += 1
    if finding_for("tests/cli.rs", "crates/sunrise-cli/src/main.rs") is not None:
        print("::error::citations self-test: the crate anchor did not resolve `tests/cli.rs`")
        failures += 1
    if verdict("src/main.rs", "crates/sunrise-cli/tests/cli.rs") != "checked":
        print("::error::citations self-test: the crate anchor did not claim `src/main.rs`")
        failures += 1
    # And it must not reach outside the crate that owns it.
    if verdict("src/main.rs") != "unanchored":
        print("::error::citations self-test: `src/main.rs` was anchored from a document")
        failures += 1

    for body in ("Cargo.toml", "docs/03-crypto/recovery.md", "apps/apple/Sunrise.xcodeproj"):
        if finding_for(body) is not None:
            print(f"::error::citations self-test: `{body}` resolves, but was reported broken")
            failures += 1

    # The failures the gate exists for, each reported rather than passed.
    for body, fragment in (
        ("docs/gone.md", "names no file"),
        ("apps/apple/Sunrise.xcodeproj:12", "is a directory"),
        ("Cargo.toml:0", "line 0"),
        ("Cargo.toml:50-40", "empty range"),
    ):
        found = finding_for(body)
        if found is None or fragment not in found.message:
            print(f"::error::citations self-test: `{body}` reported {found}, expected {fragment!r}")
            failures += 1

    if failures:
        return 1
    print("OK: citations self-test clean (36 cases).")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Check backticked path:line citations.")
    parser.add_argument("--root", default=None, help="repository root (default: the working tree this script is in)")
    parser.add_argument(
        "--list-unanchored",
        action="store_true",
        help="print every path-like span the anchor rule declined to check",
    )
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
            print(f"::error::citations: not inside a git work tree: {error}")
            return 2

    failed = self_test()
    if failed:
        return failed
    return check(root, args.list_unanchored)


if __name__ == "__main__":
    sys.exit(main())
