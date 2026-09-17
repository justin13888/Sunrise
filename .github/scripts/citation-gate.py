#!/usr/bin/env python3
"""Fail when a backticked `path:line` citation in prose points at nothing.

Why this gate exists
--------------------

The dominant way this repository cites its own source is an inline code
span — `crates/sunrise-core/src/engine/sync.rs:199`, `docs/03-crypto/recovery.md`,
`crates/sunrise-sync/src/backoff.rs:31-63`. There are close to two thousand of
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
path, optionally followed by `:LINE` or `:LINE-LINE`, optionally followed by
`#SYMBOL`. Three conditions, each decidable without an opinion about prose:

1. The span content, after CommonMark's one-space strip, matches
   `<path>(:<start>(-<end>)?)?(#<symbol>)?` and holds nothing else — no spaces,
   no trailing word. `` `see crates/foo.rs` `` is not a citation.
2. The final segment ends in one of the extensions in `EXTENSIONS` below, which
   is the closed set this repository's tracked files actually use. A *shape*
   rule instead of a list reads `task.update`, `Task.blocks` and `focus.end` as
   paths — this tree writes hundreds of op kinds and field names that way — and
   filing those under "not checked" makes the gate look blinder than it is
   while burying the residue that genuinely needs a person.
3. The path is **claimed** by an anchor (below). An unclaimed path is not
   checked, and is counted and reported rather than dropped.

The `#symbol` suffix
--------------------

A line number is the fastest-rotting thing a document can hold, and the check
above cannot see it rot: a citation that slides onto the wrong line still names
a line that exists, so the gate passes it and the sentence lies. ADR-0034 is the
measured case — nine `path:line` citations, **eight** of them pointing at
unrelated code inside roughly one release cycle, every one of them green here.

So a citation may name the item it means:
`` `crates/sunrise-core/src/engine/sync.rs:702#is_revoked` ``. When it does, and
the target is Rust, `symbol_span` finds every declaration of that name in the
file and the cited line must fall inside one of them. A renamed or deleted
symbol fails, and so does a line that has drifted out of the symbol it was
written for. Line-less `path#symbol` is admitted too, and checks only that the
declaration is still there — which is what a citation into a file somebody else
is rewriting this week should say.

The suffix is **optional**, and that is load-bearing. Every anchored citation
already in this repository parses and means exactly what it meant before this
suffix existed — the grammar was widened, not changed, and no span in the tree
carried a `#` for the widening to reach. A gate that turns a passing document
red to add a check has not added a check, it has broken a build.

What it does **not** buy: containment is not aboutness. A citation naming the
wrong symbol, or the right symbol for the wrong reason, passes — ADR-0034's step
b citation would have passed had it named `publish_own_cert`. The step from "the
line exists" to "the line is inside the item named" is the whole of the
available improvement, and it is a partial one: run against ADR-0034 as it stood
before the repair, with each citation carrying the symbol the repair gives it,
this catches **five** of the eight. The three it misses are the three whose line
had drifted *within* the item it names — `:280` is still inside
`emit_key_envelopes`, `:936-979` is still inside a 700-line `apply_control_op`,
and `:381` is inside `backfill_key_envelopes`'s own doc run. Containment is a
weaker test the larger the item, and nothing line-based fixes that.

Anchors
-------

Three, tried in this order, and a path claimed by any of them must resolve:

1. **The repository root.** Claimed when the first segment names a top-level
   entry git tracks: `docs/…`, `crates/…`, `.github/…`, `Cargo.toml`.
2. **The citing file's own directory.** `../10-cross-cutting/protocol-versioning.md`
   and `key-rotation.md` are how most of this tree cites, and they mean what a
   markdown renderer and github.com make them mean: relative to the file they
   are written in. The resolution is not reimplemented here — it is
   `docs-link-gate.py`'s `resolve_relative`, imported, because that gate
   resolves `[a](../x.md)` by the same convention and two implementations of
   "where does that point" can disagree.
3. **The crate root**, for a citing file under `crates/<name>/`. Cargo fixes
   that layout, so `tests/live_sync.rs` written inside `crates/sunrise-cli`
   means `crates/sunrise-cli/tests/live_sync.rs` and can mean nothing else.

Anchor 2 claims on two different terms, and the difference is the whole of this
gate's honesty:

* A path that climbs — `../10-cross-cutting/protocol-versioning.md` — has
  exactly one reading, because nothing but a repository path is written that
  way. It is **claimed unconditionally** and a dangling one fails. That is 290
  citations here, and the reason this anchor exists.
* Everything else relative — a bare `recovery.md`, `Views/TaskEditorView.swift:97`,
  or a `./`-prefixed path — is claimed **only if it resolves** against the
  citing file's directory. It has to be: `recovery.md` in `docs/03-crypto/` is a
  sibling, and `recovery.md` in `docs/06-server/auth.md` is shorthand for that
  same other file. `./` is in this half rather than the one above because it has
  a second reading that this repository uses — `./sunrise.toml` is what the
  server looks for in its working directory, listed beside
  `/etc/sunrise/sunrise.toml`, and is not a path in this tree at all. Claiming
  either unconditionally would fail a correct document, which is the one thing a
  gate may not do, so they are checked where the reading is unambiguous and
  **declined, counted and listable** where it is not.

Adding an anchor can only remove failures from the classes already claimed,
never invent one there; the explicitly-relative class is new coverage rather
than a re-reading of something that used to pass.

A citation fails when no anchor resolves it, when an explicitly-relative path
climbs out of the repository, when a cited line is past the end of the resolved
file, when a range is empty (`:50-40`), when line 0 is cited, or when a line is
cited on a directory. Every failure is reported with the citing file and its
line; the gate exits 1 if any failed and 0 with a count when clean.

What is deliberately not checked
--------------------------------

The residue is bare filenames whose directory the surrounding paragraph
established and this gate cannot: `main.rs`, `gcal.rs`, `tokens.rs`, `ci.yml`,
`project.yml`, `0013_baseline.sql`. Every run prints how many there are, and
`--list-unanchored` prints each with its file and line, so the hole is visible
in the gate's own output rather than implied by its silence.

Also out, each for a reason:

* **`legacy/`**, its markdown and its Rust. It is the pre-rewrite tree kept
  verbatim, and its READMEs cite `apps/api/src/server.ts` — a path that *does*
  resolve against this repository's `apps/` and means something else entirely
  there. Scanning it would produce confident nonsense.
* **Directory citations with no extension** (`crates/sunrise-core/src/engine`).
  Rule 2 excludes them, so a renamed directory is not caught.
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
  decides that. Without a `#symbol` suffix this gate answers only "does that
  line exist"; with one it also answers "is that line inside the item named",
  which is strictly more and still strictly less than aboutness.
* **A symbol suffix on a target that is not Rust.** There is no resolver for
  one, and `docs/x.md#heading` is a link fragment rather than a declaration, so
  the whole span is declined instead of guessed at. Recorded here because the
  grammar admits it and the check does not.

Usage: citation-gate.py [--root PATH] [--list-unanchored] [--self-test]
Exit 0 clean, 1 on a dangling citation, 2 if the gate could not run at all.
"""

from __future__ import annotations

import argparse
import importlib.util
import pathlib
import posixpath
import re
import subprocess
import sys
import tempfile
from dataclasses import dataclass


def _sibling(name: str, filename: str):
    """Import a sibling gate script, whose name is not an identifier.

    Registered in `sys.modules` before it executes because the module it loads
    defines dataclasses, and `@dataclass` looks its own module up by name.
    """
    path = pathlib.Path(__file__).resolve().parent / filename
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        print(f"::error::citations: cannot load {path}; the gate could not run.")
        raise SystemExit(2)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


# Anchor 2's resolution, borrowed rather than rewritten. See the docstring.
resolve_relative = _sibling("docs_link_gate", "docs-link-gate.py").resolve_relative

# A code span, CommonMark's rule: a run of N backticks closed by a run of
# exactly N. Single-line on purpose -- see the docstring.
CODE_SPAN = re.compile(r"(?P<ticks>`+)(?!`)(?P<body>[^\n]+?)(?<!`)(?P=ticks)(?!`)")

# The file types this repository tracks, which is what makes a dotted name a
# path rather than an op kind (`task.update`, `Task.blocks`, `focus.end`).
#
# Derived from the tree rather than invented:
#
#   git ls-files | grep -oE '\.[A-Za-z][A-Za-z0-9]*$' | sort -u
#
# minus the types nothing cites (`.png`, `.ico`, `.icns`, `.db`, `.html`,
# `.graphql`, `.example`) and the dotfile suffixes. Measured, so the reasons
# differ per entry and are worth stating:
#
# * `.cbor` and `.jsonc` earn their place outright — three resolving citations
#   run through the first (`tests/fixtures/hello/v1.cbor` and its neighbours)
#   and two through the second (`biome.jsonc`), and a shorter list drops them.
# * `.css` is tracked and cited only by bare name (`tokens.css`), so nothing
#   resolves through it today. It is here so those spans are counted as paths
#   this gate declined rather than dismissed as prose.
# * `.yaml` and `.xml` are speculative: one tracked `.yaml` file, no `.xml` at
#   all, and no citation to either. They cost nothing and spare the next person
#   a puzzling miss.
#
# **A new file type belongs here.** Until it is added, citations to it are
# counted as unchecked rather than verified, which the declined tally in every
# run will show.
EXTENSIONS = frozenset(
    "rs md toml py swift sh yml yaml sql json jsonc ts tsx css txt ics xml lock cbor".split()
)

# The whole span, or it is not a citation.
CITATION = re.compile(
    r"""
    ^
    (?P<path>
        [A-Za-z0-9_.][A-Za-z0-9_.+-]*
        (?: / [A-Za-z0-9_.+-]+ )*
        \. (?P<ext> [A-Za-z][A-Za-z0-9]{0,11} )
    )
    (?: : (?P<start>[0-9]{1,9}) (?: - (?P<end>[0-9]{1,9}) )? )?
    (?: \# (?P<symbol> [A-Za-z_][A-Za-z0-9_]{0,127} ) )?
    $
    """,
    re.VERBOSE,
)

# The declaration of one named Rust item, at whatever indent it sits.
#
# Line-based on purpose: a Rust parser is a dependency this repository does not
# have in CI, and the two questions asked here — "where does the item start"
# and "where does it end" — are both answerable from rustfmt's own output,
# which every file in the tree has been through. `{name}` is `re.escape`d by
# `symbol_span`.
#
# `impl<'a> Foo` is not matched, and neither is a path-qualified `#Engine::f`
# (the grammar above admits no `::`). Both are misses rather than false
# failures: an unmatched suffix is reported as a symbol the file does not
# declare, which is a red check somebody reads, not a silent pass.
SYMBOL_DECL = (
    r"^(?P<indent>[ \t]*)"
    r"(?:pub(?:\([^)]*\))?[ \t]+)?"
    r"(?:default[ \t]+)?"
    r"(?:async[ \t]+)?"
    r"(?:unsafe[ \t]+)?"
    r"(?:const[ \t]+)?"
    r"(?:fn|struct|enum|trait|impl|mod|type|static|union)[ \t]+"
    r"{name}\b"
)

FENCE = re.compile(r"^[ \t]{0,3}(?P<char>`{3,}|~{3,})(?P<info>.*)$")
# `////` is an ordinary comment, not a doc comment, so the lookahead matters.
# The same MARKER `doc-comment-gate.py` uses, which is this repository's
# enforced definition of a doc comment.
MARKER = re.compile(r"^(?P<indent>[ \t]*)(?P<marker>///(?!/)|//!)(?P<body>.*)$")

CRATE_PREFIX = re.compile(r"^(crates/[^/]+)/")

# Citations this gate accepts against a path git does not track.
#
# **Keyed on `(citing file, cited path)`, and that pair is load-bearing — do
# not simplify it to a path key.** An entry excuses one sentence, not a
# spelling everywhere it appears. ADR-0019 names `docs/07-clients/tui.md`
# because it is the document that decision deleted; a path key would have
# silenced the live doc comment in `crates/sunrise-cli/tests/cli.rs` that was
# still sending readers to it, which is a real defect and was fixed rather
# than hidden.
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


def symbol_span(path: str, name: str) -> list[tuple[int, int]]:
    """Every span in a Rust file that declares `name`, doc comment included.

    Returns a list rather than the one span the first draft of this reached
    for, because a name is declared more than once in a single file all over
    this tree: a trait method and each `impl` of it, a `struct` and the `mod`
    that shares its name. Taking the first would fail a citation into the
    second, and a gate that fails a correct document is worse than one that
    passes a wrong one. Containment is therefore tested against the union.

    A span **starts** at the first line of the contiguous run of `///` lines
    and `#[…]` attributes above the declaration, so
    `tests.rs:6388#a_revoked_devices_ops_still_apply_at_the_replica` — a
    citation of the test's *doc*, which is where this repository puts its
    reasoning — is inside its own symbol. A multi-line attribute breaks the
    run and truncates the span, which can only report a citation that is
    inside; the author then sees a red check rather than a silent pass.

    A span **ends** at the first line that is exactly this declaration's indent
    followed by `}` — rustfmt guarantees an item's closing brace sits alone at
    the item's own indent, and matching on that is far steadier than counting
    braces through string literals and format specifiers. An item with no body
    (`fn peek(&self) -> Hlc;`, `pub struct Unit;`, `type Alias = …;`) ends at
    the first line ending in `;` before any `{`, and a one-line item whose
    braces balance on the declaration ends there. A span whose close is never
    found runs to the end of the file: over-broad, so it can only pass a
    citation, never fail one.
    """
    try:
        with open(path, "rb") as handle:
            lines = handle.read().decode("utf-8", errors="replace").splitlines()
    except OSError:
        return []

    declaration = re.compile(SYMBOL_DECL.format(name=re.escape(name)))
    spans: list[tuple[int, int]] = []

    for index, line in enumerate(lines):
        found = declaration.match(line)
        if not found:
            continue

        start = index
        while start > 0:
            above = lines[start - 1].strip()
            if above.startswith("///") or above.startswith("#["):
                start -= 1
                continue
            break

        closing = found.group("indent") + "}"
        end = len(lines)
        opened = False
        for cursor in range(index, len(lines)):
            body = lines[cursor].rstrip()
            if cursor > index and body == closing:
                end = cursor + 1
                break
            if "{" in body:
                # A whole item on its declaration line — `enum E { A, B }` —
                # never reaches the closing-brace rule, because its brace
                # never gets a line of its own.
                if cursor == index and body.count("{") == body.count("}"):
                    end = cursor + 1
                    break
                opened = True
            if not opened and body.endswith(";"):
                end = cursor + 1
                break

        spans.append((start + 1, end))

    return spans


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
    """The tracked tree, indexed the ways `readings` asks about it."""

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

    def holds(self, path: str) -> bool:
        """Whether the tree has a file or a directory at `path`."""
        return path in self.files or path in self.dirs

    def claims(self, anchor: str, first_segment: str) -> bool:
        """Whether `anchor` holds an entry the citation's first segment names."""
        return first_segment in self.children.get(anchor, ())

    @staticmethod
    def crate_of(citing: str) -> str | None:
        """`crates/<name>` when the citing file lives in one."""
        match = CRATE_PREFIX.match(citing)
        return match.group(1) if match else None


def readings(citing: str, path: str, tree: Tree) -> tuple[list[str], bool, bool]:
    """How `path`, written in `citing`, could be read.

    Returns the candidate repository paths in anchor order, whether any anchor
    **claims** the citation — an unclaimed one is declined rather than failed —
    and whether it climbs out of the repository.
    """
    if path.startswith("../"):
        # A `../` path climbs out of the citing file's directory, which is a
        # thing only a repository path does: there is exactly one reading and
        # no ambiguity to be careful about. Claimed either way, so a dangling
        # one fails -- which is the class this anchor exists for, and 290 of
        # this repository's citations are in it.
        #
        # `./` is *not* here, deliberately. It has a second reading — the
        # process's working directory — and this repository uses it that way:
        # `./sunrise.toml` appears in three config-precedence lists beside
        # `/etc/sunrise/sunrise.toml`, naming a file an operator creates on the
        # deployment host rather than anything in the tree. So a `./` path
        # falls through to the implicit rule below and is claimed only when it
        # resolves, which costs nothing — all four that name a repository file
        # resolve, and the three that name a runtime path are declined instead
        # of being reported as defects they are not.
        relative = resolve_relative(citing, path)
        if relative is None:
            return [], True, True
        return [relative], True, False

    found: list[str] = []
    claimed = False
    first = path.split("/", 1)[0]

    if tree.claims("", first):
        found.append(path)
        claimed = True

    # Implicitly relative: `recovery.md` beside its sibling means the sibling,
    # and the same span in another directory is shorthand for somewhere else.
    # So this anchor claims only what it actually resolves.
    relative = resolve_relative(citing, path)
    if relative is not None and tree.holds(relative) and relative not in found:
        found.append(relative)
        claimed = True

    crate = tree.crate_of(citing)
    if crate is not None and tree.claims(crate, first):
        candidate = posixpath.join(crate, path)
        if candidate not in found:
            found.append(candidate)
        claimed = True

    return found, claimed, False


def classify(span: Span, citing: str, root: str, tree: Tree) -> tuple[str, Finding | None]:
    """Decide what one span is: `"skip"`, `"unanchored"` or `"checked"`.

    A `"checked"` verdict carries a `Finding` when the citation is broken and
    `None` when it resolves.
    """
    match = CITATION.match(span.body)
    if not match or match.group("ext").lower() not in EXTENSIONS:
        return "skip", None

    symbol = match.group("symbol")
    if symbol is not None and match.group("ext").lower() != "rs":
        # `docs/x.md#heading` is a link fragment, not a declaration, and there
        # is no resolver for a symbol outside Rust. Declining the whole span
        # keeps such a citation exactly as unchecked as it is today rather
        # than inventing a verdict for it — and, decisively, it cannot turn a
        # document red for a suffix the gate never promised to read.
        return "skip", None

    path = match.group("path")
    candidates, claimed, escapes = readings(citing, path, tree)
    if not claimed:
        return "unanchored", None

    def broken(message: str) -> tuple[str, Finding]:
        return "checked", Finding(file=citing, line=span.line, span=span.body, message=message)

    if escapes:
        return broken("climbs out of the repository.")

    start = match.group("start")
    end = match.group("end")
    first_line = int(start) if start is not None else None
    last_line = int(end) if end is not None else first_line

    if first_line is not None:
        if first_line == 0:
            return broken("cites line 0; line numbers start at 1.")
        if last_line is not None and last_line < first_line:
            return broken(f"cites an empty range ({first_line}-{last_line}).")

    target = next((candidate for candidate in candidates if candidate in tree.files), None)

    if target is None:
        if any(candidate in tree.dirs for candidate in candidates):
            if first_line is None:
                return "checked", None
            return broken(f"cites a line, but `{path}` is a directory.")
        if (citing, path) in ALLOWED:
            return "checked", None
        return broken("names no file git tracks.")

    if first_line is not None:
        total = line_count(posixpath.join(root, target))
        if last_line is not None and last_line > total:
            where = f"line {last_line}" if end is None else f"lines {first_line}-{last_line}"
            return broken(f"cites {where}, but `{target}` has {total} line(s).")

    if symbol is not None:
        spans = symbol_span(posixpath.join(root, target), symbol)
        if not spans:
            return broken(f"names `{symbol}`, which `{target}` does not declare.")
        if first_line is not None and not any(
            low <= first_line and (last_line or first_line) <= high for low, high in spans
        ):
            where = f"line {first_line}" if end is None else f"lines {first_line}-{last_line}"
            spelled = ", ".join(f"{low}-{high}" for low, high in spans)
            return broken(
                f"cites {where}, but `{symbol}` in `{target}` spans {spelled}."
            )

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

    for (citing, path), reason in sorted(ALLOWED.items()):
        if path in tree.files:
            print(
                f"::error::citations: `{path}` is allowlisted for {citing} ({reason}) but git "
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
            if cited and (name, cited.group("path")) in ALLOWED:
                used.add((name, cited.group("path")))
            if finding is not None:
                findings.append(finding)

    for finding in findings:
        print(
            f"::error file={finding.file},line={finding.line}::citations: "
            f"`{finding.span}` {finding.message}"
        )

    stale = sorted(set(ALLOWED) - used)
    for citing, path in stale:
        print(
            f"::error::citations: the allowlist carries `{path}` in {citing}, but no such "
            "citation is there any more; delete the entry."
        )

    print(f"citations: {checked} anchored citation(s) across {len(files)} file(s).")
    if unanchored:
        # Split, because the two halves are declined for the same reason but a
        # reader sizing the hole should see both: `main.rs` and `ci.yml` are
        # bare names, `api/observe.rs` and `store/devices.rs:90` are fragments
        # of a path the surrounding paragraph already established. Neither can
        # be placed without guessing which directory was meant.
        # A leading `./` does not make a name a path: `./sunrise.toml` is a
        # bare filename with a prefix that says "here".
        partial = sum(1 for note in unanchored if "/" in note.span.removeprefix("./"))
        print(
            f"citations: {len(unanchored)} path-like span(s) were NOT checked — "
            f"{len(unanchored) - partial} bare filename(s) and {partial} partial path(s), each "
            "shorthand for a directory the prose around it establishes and this gate cannot; "
            "re-run with --list-unanchored to see them."
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
`sunrise_core::engine`, `--all-features`, `0.1.0`, `#[derive(Debug)]`,
`task.update`, `Task.blocks`, `focus.end`.

Relative, and checked: `../06-server/api.md`, `recovery.md`.

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

# The one fixture written to disk rather than held as a string, because
# `symbol_span` reads a file and the rest of the self-test never does. Line
# numbers are load-bearing and are named in the assertions below:
#
#   1-2   doc of `wanted`      3   its attribute     4  its `fn` line
#   1-9   `wanted`'s span, doc and attribute included
#   11-13 `other`               15-17 `trait T`        16 `twice`, `;`-ended
#   19-23 `impl`                20-22 `twice` again, this one with a body
#
# `twice` appearing twice is the whole reason `symbol_span` returns a list.
SYMBOL_FIXTURE = """\
/// Doc line one.
/// Doc line two.
#[allow(dead_code)]
pub(super) fn wanted(
    x: u8,
) -> u8 {
    let _sql = "SELECT 1 FROM t";
    x
}

fn other() -> u8 {
    0
}

trait T {
    fn twice(&self) -> u8;
}

impl T for u8 {
    fn twice(&self) -> u8 {
        2
    }
}
"""

# A tree with one crate, so anchor 3 has something to resolve against, a
# top-level `tests/`, which is what makes anchors 1 and 3 disagree, and two
# documents in different directories, which is what anchor 2 is about.
FIXTURE_TRACKED = [
    "Cargo.toml",
    "docs/03-crypto/recovery.md",
    "docs/03-crypto/key-rotation.md",
    "docs/06-server/api.md",
    "crates/sunrise-cli/src/main.rs",
    "crates/sunrise-cli/tests/cli.rs",
    "tests/chaos/README.md",
    "schemas/generated.json/kept.json",
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

    def verdict(body: str, citing: str = "docs/03-crypto/recovery.md") -> str:
        got, _ = classify(Span(line=1, body=body), citing, ".", tree)
        return got

    def finding_for(body: str, citing: str = "docs/03-crypto/recovery.md") -> Finding | None:
        _, found = classify(Span(line=1, body=body), citing, ".", tree)
        return found

    # Near misses. Each is a real span somewhere in this tree, and none of them
    # is a citation: a gate that fires on all of these is as useless as one that
    # fires on none. The last three are what the closed extension set is for.
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
        "task.update",
        "Task.blocks",
        "focus.end",
    ):
        if verdict(body) != "skip":
            print(f"::error::citations self-test: `{body}` was read as a citation")
            failures += 1

    # Anchor 2, the explicit half: one reading, so a dangling one must fail.
    if finding_for("../06-server/api.md") is not None:
        print("::error::citations self-test: a resolving `../` citation was reported broken")
        failures += 1
    dangling = finding_for("../06-server/does-not-exist.md")
    if dangling is None or "names no file" not in dangling.message:
        print(f"::error::citations self-test: a dangling `../` citation reported {dangling}")
        failures += 1
    escaping = finding_for("../../../etc/passwd.toml")
    if escaping is None or "climbs out" not in escaping.message:
        print(f"::error::citations self-test: an escaping citation reported {escaping}")
        failures += 1

    # `./` is the implicit half, not the explicit one, because this repository
    # writes `./sunrise.toml` for a runtime working directory.
    if verdict("./key-rotation.md") != "checked":
        print("::error::citations self-test: a resolving `./` citation was not checked")
        failures += 1
    if verdict("./sunrise.toml") != "unanchored":
        print("::error::citations self-test: a `./` runtime path was read as a repository citation")
        failures += 1

    # Anchor 2, the implicit half: claimed where it resolves, declined where
    # it does not, because the same span means different files in different
    # directories and guessing would fail a correct document.
    if finding_for("key-rotation.md") is not None:
        print("::error::citations self-test: a resolving sibling citation was reported broken")
        failures += 1
    if verdict("key-rotation.md", "docs/06-server/api.md") != "unanchored":
        print("::error::citations self-test: a non-sibling bare filename was not declined")
        failures += 1
    for body in ("main.rs", "ci.yml", "0013_baseline.sql", "Views/TaskEditorView.swift:97"):
        if verdict(body) != "unanchored":
            print(f"::error::citations self-test: `{body}` classified {verdict(body)!r}, expected 'unanchored'")
            failures += 1

    # Anchor 3. The same span is dangling from a document and resolved from
    # inside the crate whose layout Cargo fixes -- and `tests/cli.rs` is the
    # shape that makes the difference, because a top-level `tests/` exists.
    if finding_for("tests/cli.rs") is None:
        print("::error::citations self-test: `tests/cli.rs` resolved from a document, where it cannot")
        failures += 1
    if finding_for("tests/cli.rs", "crates/sunrise-cli/src/main.rs") is not None:
        print("::error::citations self-test: the crate anchor did not resolve `tests/cli.rs`")
        failures += 1
    if verdict("src/main.rs", "crates/sunrise-cli/tests/cli.rs") != "checked":
        print("::error::citations self-test: the crate anchor did not claim `src/main.rs`")
        failures += 1

    for body in ("Cargo.toml", "docs/03-crypto/recovery.md", "schemas/generated.json"):
        if finding_for(body) is not None:
            print(f"::error::citations self-test: `{body}` resolves, but was reported broken")
            failures += 1

    # The failures the gate exists for, each reported rather than passed.
    for body, fragment in (
        ("docs/gone.md", "names no file"),
        ("schemas/generated.json:12", "is a directory"),
        ("Cargo.toml:0", "line 0"),
        ("Cargo.toml:50-40", "empty range"),
    ):
        found = finding_for(body)
        if found is None or fragment not in found.message:
            print(f"::error::citations self-test: `{body}` reported {found}, expected {fragment!r}")
            failures += 1

    # The `#symbol` suffix. These are the only self-test cases that read a
    # file, so they get a tree of their own rather than this repository's --
    # the same reason the contract test beside this one synthesises its
    # fixtures: a case that read the real tree would go red for whatever
    # somebody edited this week instead of for a change to the rule.
    with tempfile.TemporaryDirectory() as scratch:
        fixture = pathlib.Path(scratch) / "crates" / "sunrise-cli" / "src"
        fixture.mkdir(parents=True)
        (fixture / "main.rs").write_text(SYMBOL_FIXTURE, encoding="utf-8")

        def symbol_verdict(body: str) -> tuple[str, Finding | None]:
            return classify(Span(line=1, body=body), "docs/03-crypto/recovery.md", scratch, tree)

        main = "crates/sunrise-cli/src/main.rs"

        # Inside the symbol: the declaration, its doc, and its closing brace.
        # The doc line matters most -- this repository puts its reasoning in
        # doc comments, so a citation of one must be inside its own item.
        for line in (1, 3, 4, 9):
            verdict_at, found = symbol_verdict(f"{main}:{line}#wanted")
            if verdict_at != "checked" or found is not None:
                print(f"::error::citations self-test: `{main}:{line}#wanted` reported {found}, expected clean")
                failures += 1

        # Outside it, which is the whole point of the suffix.
        for line in (11, 12, 21):
            _, found = symbol_verdict(f"{main}:{line}#wanted")
            if found is None or "spans 1-9" not in found.message:
                print(f"::error::citations self-test: `{main}:{line}#wanted` reported {found}, expected a span miss")
                failures += 1

        # A name declared twice is one citation target, not two: the trait
        # method and its impl are both `twice`, and a line in either is in.
        for line in (16, 20, 21):
            _, found = symbol_verdict(f"{main}:{line}#twice")
            if found is not None:
                print(f"::error::citations self-test: `{main}:{line}#twice` reported {found}, expected clean")
                failures += 1
        _, found = symbol_verdict(f"{main}:12#twice")
        if found is None or "16-16, 20-22" not in found.message:
            print(f"::error::citations self-test: `{main}:12#twice` reported {found}, expected both spans named")
            failures += 1

        # Line-less: checks the declaration is still there and nothing else.
        # This is the form a citation into a file somebody else is rewriting
        # should take, so it has to work without a line to contain.
        _, found = symbol_verdict(f"{main}#wanted")
        if found is not None:
            print(f"::error::citations self-test: `{main}#wanted` reported {found}, expected clean")
            failures += 1
        _, found = symbol_verdict(f"{main}#absent")
        if found is None or "does not declare" not in found.message:
            print(f"::error::citations self-test: `{main}#absent` reported {found}, expected a missing symbol")
            failures += 1
        _, found = symbol_verdict(f"{main}:4#absent")
        if found is None or "does not declare" not in found.message:
            print(f"::error::citations self-test: `{main}:4#absent` reported {found}, expected a missing symbol")
            failures += 1

        # No suffix: byte-for-byte the behaviour of every citation in the tree
        # before this suffix existed. If this moves, the widening was not one.
        for line, want in ((4, True), (23, True), (24, False)):
            _, found = symbol_verdict(f"{main}:{line}")
            if (found is None) != want:
                print(f"::error::citations self-test: `{main}:{line}` reported {found}, expected {'clean' if want else 'out of range'}")
                failures += 1

        # A symbol on a target that is not Rust is declined, not failed. The
        # gate has no resolver for one and `docs/x.md#heading` is a fragment.
        for body in ("docs/03-crypto/recovery.md#heading", "docs/03-crypto/recovery.md:2#heading",
                     "Cargo.toml#package", "docs/03-crypto/gone.md#heading"):
            if symbol_verdict(body)[0] != "skip":
                print(f"::error::citations self-test: `{body}` was not declined")
                failures += 1

    if failures:
        return 1
    print("OK: citations self-test clean (67 cases).")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Check backticked path:line citations.")
    parser.add_argument("--root", default=None, help="repository root (default: the working tree this script is in)")
    parser.add_argument(
        "--list-unanchored",
        action="store_true",
        help="print every path-like span the anchor rules declined to check",
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
