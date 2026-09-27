#!/usr/bin/env python3
"""Fail when a Rust doc comment has been mangled into a shape rustdoc misreads.

Why this gate exists
--------------------

`cargo fmt` does not reflow comments — that is a deliberate rustfmt scope
decision, not an oversight — and clippy has no rule for comment *shape*. So a
doc comment that a hand-edit has folded, split or detached passes every gate
this repository has. Two instances were created during PR #77, the second while
fixing the first, which is what makes the class worth a check rather than a
one-off correction:

    /// A member cannot republish a *sibling's* cert.    /// A member cannot ...

Two `///` markers on one physical line. It renders as one run-on line, reads as
duplicated prose in the source, and `cargo fmt --check` is clean because
formatting comments is not rustfmt's job.

What this checks, and why each is decidable
-------------------------------------------

Six shapes, each of which a tool can settle without an opinion about prose:

* **`collapsed`** — a second `///` or `//!` on the same physical line. The #77
  shape above. Code spans are masked first, so a comment that *documents* the
  marker (``use `///` for an outer doc comment``) is not a violation.
* **`unclosed-fence`** — an odd number of code-fence openers in one doc
  comment. Everything after the unclosed fence renders as code, including the
  prose that follows it, and rustdoc will additionally try to *compile* that
  prose as a doc test.
* **`indented-code`** — a paragraph indented four columns past its container,
  which CommonMark reads as an indented code block and rustdoc then compiles as
  Rust. This is the #58/#60 defect exactly: kynos 0.1.0's route macros generate
  a doc comment whose continuation lines are indented, rustdoc reads the English
  as a code block, and `cargo test --doc` tries to compile it — which is why
  `rust-doctest` carries a `--skip relative_uri` today. Not one doc comment in
  this workspace opens a code block by indentation; every one of them fences,
  so the rule is the tree's own convention rather than an invention.
* **`detached`** — a doc comment on the line directly after an *outer*
  attribute. Rust accepts it and attaches the comment to the item below, so
  nothing complains, but it is almost always the trace of an edit that inserted
  an item between an existing doc comment and the thing it documented. PR #63
  hit exactly that when a new test landed between an existing test's doc
  comment and its `#[test]`. An *inner* attribute (`#![…]`) is excluded: it
  applies to the item containing it and is written as the first line of a
  function body, so the comment below one documents whatever comes next —
  which is what `sunrise-cli`'s two `#![allow(clippy::print_stdout)]`
  functions do, correctly.
* **`split`** — two doc comment blocks of the same kind separated by a blank
  line. Rust joins them into one doc string and the blank line does not
  survive, so two paragraphs run together in the rendered output. It normally
  means an edit split one comment in two.
* **`stranded`** — a `///` block that opens with another item's summary, and
  whose own summary starts further down with no blank line before it. This is
  what `split` cannot see: an edit that moved or inserted an item and left the
  old item's doc lines sitting directly on top of the new item's, so the two
  runs are one well-formed block. Both instances this repository has had were
  this shape (#268 in `engine/oplog.rs`, #290 in `engine/tests.rs`), and both
  survived review until someone read them. The rule is two facts about names,
  not prose: the block's first sentence names one or more backticked
  identifiers and *none* of them occurs in the documented item's signature,
  and a later sentence that opens at the start of a line — directly after a
  line that ended one — names a backticked identifier that *does*. That second
  sentence is the item's own summary, and the lines above it belong to
  somebody else.

  It is deliberately narrower than "the summary names something the signature
  does not". That weaker rule fires on 1,096 of the tree's 1,658 summaries
  that name an identifier, because a summary legitimately names the table a
  function counts, the op kind it folds, or the type it returns; a rule that
  needs an allowlist of a thousand entries is not a gate. Requiring the second
  sentence — the item's real summary, naming its own parameter — is what makes
  the rule decidable, and it reports nothing on the tree today.

  The later sentence opens with an uppercase letter, or with a backticked
  name when it sits in the block's first paragraph. Past the first paragraph
  a line opening with a backticked name is how this tree documents its
  parameters one per line, and admitting it there reported three correct
  blocks (`sunrise-core-bindings`' `import_ical`, `sunrise-domain`'s
  `parse`, `sunrise-sync`'s `mark_carried`).

  What it therefore does not see: a stranded run whose first sentence names no
  backticked identifier, or names one the new item's signature happens to
  share; a displaced summary whose true one names nothing in the signature
  either; and a true summary that opens with a backticked name below a
  stranded run of more than one paragraph, which is the #290 shape with its
  second sentence reworded. Those need a reader, the way `citation-gate.py`
  leaves aboutness to one.

Scope
-----

**In:** `///` and `//!` line doc comments in every `.rs` file git tracks.

**Out, and deliberately:**

* **Prose style of any kind.** No line-length rule, no sentence rule, no
  vocabulary. The tree's longest doc lines are markdown tables and a CDDL
  block, all of them correct and none of them wrappable, so a width rule would
  need an exemption list — and an exemption list is how a gate becomes
  something people argue with rather than fix. The shapes above are the subset
  a tool can decide alone.
* **An attribute spread over several lines.** `detached` compares against one
  physical line, so a doc comment below the closing `)]` of a multi-line
  `#[derive(…)]` is not reported. Matching a bare `)]` would fire on array and
  tuple literals, and the workspace holds no instance of either shape, so the
  narrow rule is the one that ships.
* **`/** … */` block doc comments and `#[doc = "…"]` attributes.** No `.rs`
  file in this repository uses either. Adding one would be unscanned, which is
  worth knowing; it is recorded here rather than guessed at.
* **Whether the link targets resolve.** `mise run rust-doc` denies every
  rustdoc warning and is the compiler's answer to that question (#120). This
  script does not read a link.
* **Generated code.** Only files git tracks are scanned, so
  `sunrise-relay-client`'s generated `api.rs` — which lives under `target/` and
  is written by a build script — is out of the set by construction, as it
  should be: nobody hand-edits it and holding a generator to a house comment
  style is not a thing this repository does.

Usage: doc-comment-gate.py [--root PATH] [--self-test]
Exit 0 clean, 1 on a violation, 2 if the gate could not run at all.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from dataclasses import dataclass

# `////` is an ordinary comment, not a doc comment, so the lookahead matters.
MARKER = re.compile(r"^([ \t]*)(///(?!/)|//!)(.*)$")
# A code span, so a doc comment that quotes a marker is not read as carrying one.
CODE_SPAN = re.compile(r"(?P<ticks>`+)(?!`).*?(?<!`)(?P=ticks)(?!`)", re.DOTALL)
FENCE = re.compile(r"^(?P<indent>[ \t]*)(?P<char>`{3,}|~{3,})(?P<info>.*)$")
LIST_ITEM = re.compile(r"^(?P<indent>[ \t]*)(?P<marker>[-*+]|\d{1,9}[.)])(?P<gap>[ \t]+)(?=\S)")
# Outer attributes only. An inner attribute (`#![…]`) applies to the item that
# *contains* it and is written as the first line of a function body, so a doc
# comment below one starts fresh and documents the next item — which is what
# `sunrise-cli`'s two `#![allow(clippy::print_stdout)]` functions do, correctly.
ATTRIBUTE = re.compile(r"^[ \t]*#\[")
TAB_WIDTH = 4
# A backticked span that is exactly one Rust identifier: `start`, `ops`.
# `(stream_id, epoch)` or `Foo::bar` is not one, and is not compared.
BACKTICKED_IDENT = re.compile(r"`([A-Za-z_][A-Za-z0-9_]*)`")
# Values rather than items: a summary saying it returns `None` names what the
# item yields, not another item, and no item this could be stranded from is
# called that.
VALUE_WORDS = frozenset({"None", "Some", "Ok", "Err", "true", "false", "self", "Self"})
IDENTIFIER = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
# The end of a sentence: a terminator followed by whitespace or the end.
SENTENCE_END = re.compile(r"[.!?](?=\s|$)")
# A line that finishes a sentence, allowing closing markup after the stop.
ENDS_SENTENCE = re.compile(r"[.!?][)\"'`*_]*$")
# The line that finishes an item's signature, once no parenthesis is open:
# it opens a body, or ends a declaration or a braced item.
SIGNATURE_END = re.compile(r"\{|[;}]\s*$")
# A field or a variant, which ends with a `,` on the item's own first line or
# on the line that closes its parenthesised fields. Only there: on any other
# line a trailing `,` ends one bound of a `where` clause or one parameter of a
# generic list, and the signature reads on.
ONE_LINE_ITEM_END = re.compile(r",\s*$")


@dataclass(frozen=True)
class Finding:
    line: int
    code: str
    message: str


def indent_width(prefix: str) -> int:
    """Columns a run of spaces and tabs occupies, tabs stopping every four."""
    width = 0
    for ch in prefix:
        width = width + (TAB_WIDTH - width % TAB_WIDTH) if ch == "\t" else width + 1
    return width


def mask_code_spans(text: str) -> str:
    """Blank inline code, keeping every offset, so a quoted marker is not one."""
    return CODE_SPAN.sub(lambda m: " " * len(m.group(0)), text)


@dataclass(frozen=True)
class Block:
    """One run of consecutive doc comment lines sharing a marker."""

    marker: str
    start: int  # 1-based line number of the first line
    end: int  # 1-based line number of the last line
    body: tuple[str, ...]  # the text after the marker, one entry per line


def doc_blocks(lines: list[str]) -> list[Block]:
    """Every maximal run of `///` (or `//!`) lines, in source order.

    A run ends at the first line that is not a doc comment of the same kind, so
    an outer comment directly below an inner one is two blocks rather than one —
    which is what Rust does with them.
    """
    blocks: list[Block] = []
    index = 0
    while index < len(lines):
        match = MARKER.match(lines[index])
        if not match:
            index += 1
            continue
        marker = match.group(2)
        start = index
        body: list[str] = []
        while index < len(lines):
            inner = MARKER.match(lines[index])
            if not inner or inner.group(2) != marker:
                break
            body.append(inner.group(3))
            index += 1
        blocks.append(Block(marker, start + 1, index, tuple(body)))
    return blocks


def check_collapsed(lines: list[str]) -> list[Finding]:
    """A second doc marker on a physical line: the #77 shape."""
    found: list[Finding] = []
    for number, raw in enumerate(lines, 1):
        match = MARKER.match(raw)
        if not match:
            continue
        rest = mask_code_spans(match.group(3))
        for stray in ("///", "//!"):
            column = rest.find(stray)
            if column >= 0:
                found.append(
                    Finding(
                        number,
                        "collapsed",
                        f"a second `{stray}` at column {len(match.group(1)) + 3 + column + 1} — "
                        "two doc comment lines have been folded into one physical line. "
                        "Split them back apart; rustfmt will not do it for you.",
                    )
                )
                break
    return found


def check_block_shape(block: Block) -> list[Finding]:
    """Fence balance and indented code blocks, over one doc comment.

    The block model is CommonMark's, kept to the part that decides these two
    questions: fences, list containers, whether a paragraph is open, and whether
    an indented code block is open. A four-column indent is code only when it is
    four columns past the innermost open list item and no paragraph is running —
    otherwise it is a list continuation or a lazy wrap, both of which are prose.
    """
    found: list[Finding] = []
    fence: tuple[str, int] | None = None
    fence_line = 0
    containers: list[int] = []
    in_paragraph = False
    in_indented_code = False

    for offset, body in enumerate(block.body):
        number = block.start + offset
        stripped = body.strip()

        if fence is not None:
            closer = FENCE.match(body)
            if (
                closer
                and closer.group("char")[0] == fence[0][0]
                and len(closer.group("char")) >= len(fence[0])
                and not closer.group("info").strip()
                and indent_width(closer.group("indent")) - fence[1] < 4
            ):
                fence = None
            continue

        if not stripped:
            in_paragraph = False
            in_indented_code = False
            continue

        indent = indent_width(body[: len(body) - len(body.lstrip(" \t"))])
        while containers and indent < containers[-1] and not in_paragraph:
            containers.pop()
        base = containers[-1] if containers else 0
        relative = indent - base

        if in_indented_code and relative >= 4:
            continue
        in_indented_code = False

        if relative >= 4 and not in_paragraph:
            in_indented_code = True
            found.append(
                Finding(
                    number,
                    "indented-code",
                    f"indented {relative} columns past its container, so CommonMark reads it "
                    "as a code block and rustdoc compiles it as Rust — the #58/#60 defect. "
                    "Dedent the prose, or fence it with ``` and a language.",
                )
            )
            continue

        opener = FENCE.match(body)
        if opener and indent_width(opener.group("indent")) - base < 4:
            fence = (opener.group("char"), indent)
            fence_line = number
            in_paragraph = False
            continue

        item = LIST_ITEM.match(body)
        if item:
            containers.append(indent_width(item.group("indent") + item.group("marker") + item.group("gap")))
        in_paragraph = True

    if fence is not None:
        found.append(
            Finding(
                fence_line,
                "unclosed-fence",
                f"the code fence opened here is never closed before the doc comment ends at "
                f"line {block.end}, so every line after it renders as code and rustdoc will "
                "try to compile the prose.",
            )
        )
    return found


def check_attachment(lines: list[str], blocks: list[Block]) -> list[Finding]:
    """A doc comment after an attribute, and one split from its other half."""
    found: list[Finding] = []
    by_start = {block.start: block for block in blocks}

    for block in blocks:
        previous = lines[block.start - 2].strip() if block.start >= 2 else ""
        if ATTRIBUTE.match(previous):
            found.append(
                Finding(
                    block.start,
                    "detached",
                    f"a doc comment directly below the attribute `{previous[:48]}`. Rust attaches "
                    "it to the item further down, which is almost never what the edit that "
                    "produced this meant — move the comment above the attribute.",
                )
            )

        after = block.end  # 0-based index of the line following the block
        gap = 0
        while after < len(lines) and not lines[after].strip():
            after += 1
            gap += 1
        if gap and after < len(lines) and (after + 1) in by_start:
            if by_start[after + 1].marker == block.marker:
                found.append(
                    Finding(
                        block.end,
                        "split",
                        f"a blank line separates this `{block.marker}` block from the one at line "
                        f"{after + 1}. Rust joins them into one doc string and the blank line does "
                        "not survive, so the two paragraphs run together — either close the gap "
                        f"with a bare `{block.marker}` line or move the item between them.",
                    )
                )
    return found


def first_sentence(text: str) -> str:
    """`text` up to and including its first sentence terminator."""
    end = SENTENCE_END.search(text)
    return text[: end.end()] if end else text


def signature_names(lines: list[str], after: int) -> set[str]:
    """Every identifier in the signature of the item a doc block ends above.

    `after` is the 0-based index of the line following the block. Blank lines,
    ordinary comments and outer attributes (a multi-line one to its closing
    `]`) are skipped; the signature then runs to the line that opens a body or
    ends a declaration, reading through any line that leaves a parenthesis
    open so a parameter list spread over several lines is read whole. A
    trailing `,` ends it only on its first line or on the line that closes a
    parenthesis, which is a field or a variant, and never once `where` has
    been read: on any other line it ends one bound or one generic parameter,
    and the ones after it are still the signature. There is no line cap. A signature cut short
    drops the names below the cut, and a name missing from it can make the
    summary look like another item's, so it is read to its end or the file's.
    """
    index = after
    while index < len(lines):
        stripped = lines[index].strip()
        if not stripped or stripped.startswith("//"):
            index += 1
        elif ATTRIBUTE.match(stripped):
            depth = 0
            while index < len(lines):
                depth += lines[index].count("[") - lines[index].count("]")
                if depth <= 0:
                    break
                index += 1
            if index < len(lines) and not lines[index].rstrip().endswith("]"):
                break  # `#[cfg(test)] mod tests {`: the item shares the line
            index += 1
        else:
            break
    names: set[str] = set()
    depth = 0
    for offset, line in enumerate(lines[index:]):
        names.update(IDENTIFIER.findall(line))
        opened = depth > 0
        depth += line.count("(") - line.count(")")
        if depth > 0:
            continue
        if SIGNATURE_END.search(line):
            break
        if "where" in names:
            continue  # every `,` from here on ends one bound
        if (offset == 0 or opened) and ONE_LINE_ITEM_END.search(line):
            break
    return names


def check_stranded(lines: list[str], block: Block) -> list[Finding]:
    """Another item's summary left on top of this item's, with no blank line.

    Reported at the line where the item's own summary starts, which is where
    the lines above it should be cut from. Lines inside a code fence are never
    read as a sentence.
    """
    if block.marker != "///":
        return []
    summary: list[str] = []
    for body in block.body:
        if not body.strip():
            break
        summary.append(body.strip())
    opening = [
        name for name in BACKTICKED_IDENT.findall(first_sentence(" ".join(summary))) if name not in VALUE_WORDS
    ]
    if not opening:
        return []
    names = signature_names(lines, block.end)
    if any(name in names for name in opening):
        return []

    fence: str | None = None
    first_paragraph = True
    for offset in range(1, len(block.body)):
        previous, current = block.body[offset - 1].strip(), block.body[offset].strip()
        opener = FENCE.match(block.body[offset - 1])
        if opener:
            char = opener.group("char")[0]
            fence = None if fence == char else (fence or char)
        if not previous:
            first_paragraph = False
        if fence is not None or not previous or not current:
            continue
        if not ENDS_SENTENCE.search(previous):
            continue
        # A sentence opening with a backticked name is a seam only in the
        # first paragraph. Further down, a line opening with a parameter's
        # name is how this tree documents its parameters one by one.
        if not (current[0].isupper() or (current[0] == "`" and first_paragraph)):
            continue
        rest = " ".join(line.strip() for line in block.body[offset:])
        own = [name for name in BACKTICKED_IDENT.findall(first_sentence(rest)) if name in names]
        if own:
            return [
                Finding(
                    block.start + offset,
                    "stranded",
                    f"this sentence names `{own[0]}` from the item's signature, but the summary above "
                    f"it at line {block.start} names only {', '.join(f'`{n}`' for n in dict.fromkeys(opening))}, "
                    "none of which the item has. The lines above this one are another item's doc "
                    "comment left behind by an edit — move them to the item they describe.",
                )
            ]
    return []


def scan_text(text: str) -> list[Finding]:
    """Every finding in one file's source, ordered by line then code."""
    lines = text.splitlines()
    blocks = doc_blocks(lines)
    found = check_collapsed(lines) + check_attachment(lines, blocks)
    for block in blocks:
        found += check_block_shape(block)
        found += check_stranded(lines, block)
    return sorted(found, key=lambda f: (f.line, f.code))


def git_tracked(root: str) -> list[str]:
    out = subprocess.run(
        ["git", "-C", root, "ls-files", "-z", "*.rs"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    return sorted(name for name in out.split("\0") if name)


def check(root: str) -> int:
    try:
        files = git_tracked(root)
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"::error::doc-comments: could not list tracked files: {error}")
        return 2

    # A floor against a listing that has stopped finding things, not against
    # deleting a crate. The workspace tracks well over 200 `.rs` files.
    if len(files) < 50:
        print(f"::error::doc-comments: only {len(files)} tracked `.rs` file(s) found; the scan is broken.")
        return 2

    violations = 0
    scanned = 0
    for name in files:
        try:
            with open(f"{root}/{name}", encoding="utf-8") as handle:
                text = handle.read()
        except (OSError, UnicodeDecodeError) as error:
            print(f"::error::doc-comments: {name}: unreadable ({error})")
            return 2
        scanned += 1
        for finding in scan_text(text):
            print(f"::error file={name},line={finding.line}::doc-comments[{finding.code}]: {name}:{finding.line}: {finding.message}")
            violations += 1

    if violations:
        print(f"::error::doc-comments: {violations} mangled doc comment(s) across {scanned} file(s).")
        return 1
    print(f"OK: doc-comments clean ({scanned} files).")
    return 0


# --------------------------------------------------------------------------
# Self-test. Runs as a precondition of every check, so a change to the rules
# fails loudly rather than quietly starting to pass everything. Half of these
# cases are shapes the gate must *reject*: a gate nobody has watched fail is a
# gate nobody knows works.
# --------------------------------------------------------------------------

# The live #77 shape, reproduced verbatim in structure.
COLLAPSED = '''
/// A member cannot republish a *sibling's* cert.    /// A member cannot republish.
pub fn f() {}
'''

# The #58/#60 shape: prose rustdoc reads as a code block and then compiles.
INDENTED = '''
//! Returns a relative URI.
//!
//!     The path is relative to the server root, and the caller
//!     joins it themselves.
'''

UNCLOSED = '''
/// Example:
///
/// ```rust
/// let x = 1;
///
/// And the prose resumes, still inside the fence.
pub fn f() {}
'''

# The #63 shape: an edit put an item between a doc comment and its attribute.
DETACHED = '''
#[test]
/// What this test proves.
fn t() {}
'''

SPLIT = '''
/// First half of a sentence

/// second half of the same sentence.
pub fn f() {}
'''

# The #290 shape, from `engine/tests.rs` before it was fixed: `ledger_rows`'s
# summary left on top of `events_emitted_by`'s, with no blank line between.
STRANDED = '''
    /// How many `device_revoke` ops this replica has kept, believed or not.
    ///
    /// The register is a fold over these, so "stored and skipped" and "never
    /// arrived" look identical in `device_revocations` and are told apart only
    /// here.
    /// The `ev` names of every event emitted while `f` runs, in order.
    ///
    /// Hand-rolled rather than borrowed from `sunrise-log`'s capture target.
    pub(super) fn events_emitted_by(f: impl FnOnce()) -> Vec<String> {
        Vec::new()
    }
'''

# Every shape below must be accepted. Each one exists because a plausible
# implementation of a rule above rejects it.
CLEAN = '''
//! Use `///` for an outer doc comment and `//!` for an inner one, and note
//! that `////` is an ordinary comment rather than a doc comment.
//!
//! - A list item whose continuation is indented to line up with its text,
//!   which is prose and not a code block.
//!
//!   A second paragraph inside that same list item, indented three columns.
//!
//! ```rust
//! let indented_inside_a_fence = 1;
//!     // four more columns, still inside the fence
//! ```
//!
//! A paragraph whose second line
//!     is over-indented as a lazy continuation, which CommonMark keeps as
//! prose because the paragraph was already open.

//// Not a doc comment at all, so nothing here is scanned: /// ///

/// Documented, and its attributes come after it, which is the right order.
#[must_use]
#[allow(dead_code)]
pub fn f() -> u8 {
    0
}

/// An inner attribute applies to the function, so the comment below it belongs
/// to the constant and is not detached from anything.
pub fn h() -> u32 {
    #![allow(clippy::print_stdout)]
    /// Enough to choose from without becoming a list.
    const PICKS: u32 = 5;
    PICKS
}

/// An outer comment directly below an inner one is two blocks, not one split
//! block — different markers, so no `split` finding.
pub fn g() {}

/// How many `device_revoke` ops are stored — a table the signature never names.
/// Nothing here names the parameter either, so this is prose, not a seam.
#[must_use]
pub(super) fn ledger_rows(db: &Db) -> i64 {
    0
}

/// The `ev` name of `record`, which is what the signature holds.
/// The `sunrise-log` catalogue is the gate on it.
pub fn ev(record: &Record) -> &str {
    ""
}

/// Rows of `device_revoke_ops`, and only once. A later sentence naming `db`
/// mid-line is not a seam, and neither is one inside a fence:
///
/// ```text
/// done.
/// Then `db` again.
/// ```
pub fn rows(
    db: &Db,
) -> i64 {
    0
}
'''


def self_test() -> int:
    failures = 0

    def expect(name: str, text: str, codes: list[str]) -> None:
        nonlocal failures
        actual = sorted(f.code for f in scan_text(text))
        if actual != sorted(codes):
            print(f"::error::doc-comments self-test: {name} yielded {actual}, expected {sorted(codes)}")
            failures += 1

    expect("COLLAPSED", COLLAPSED, ["collapsed"])
    expect("INDENTED", INDENTED, ["indented-code"])
    expect("UNCLOSED", UNCLOSED, ["unclosed-fence"])
    expect("DETACHED", DETACHED, ["detached"])
    expect("SPLIT", SPLIT, ["split"])
    expect("STRANDED", STRANDED, ["stranded"])
    expect("CLEAN", CLEAN, [])

    # The findings have to name the right line, or the message sends a reader
    # to the wrong place and the gate is worse than nothing.
    lines = {f.code: f.line for f in scan_text(COLLAPSED)}
    if lines.get("collapsed") != 2:
        print(f"::error::doc-comments self-test: collapsed reported at line {lines.get('collapsed')}, expected 2")
        failures += 1
    unclosed = scan_text(UNCLOSED)
    if not unclosed or unclosed[0].line != 4:
        print(f"::error::doc-comments self-test: unclosed fence reported at {unclosed}, expected line 4")
        failures += 1
    # `stranded` names the line the item's own summary starts on, which is
    # where the lines above it have to be cut from.
    stranded = scan_text(STRANDED)
    if not stranded or stranded[0].line != 7:
        print(f"::error::doc-comments self-test: stranded reported at {stranded}, expected line 7")
        failures += 1

    # `doc_blocks` splits on the marker, which is what makes the `split` rule
    # safe next to an inner comment following an outer one.
    blocks = doc_blocks(CLEAN.splitlines())
    markers = [b.marker for b in blocks]
    if markers != ["//!", "///", "///", "///", "///", "//!", "///", "///", "///"]:
        print(f"::error::doc-comments self-test: blocks read as {markers}")
        failures += 1

    if failures:
        return 1
    print("OK: doc-comments self-test clean (11 cases).")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Check Rust doc comment shape.")
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
            print(f"::error::doc-comments: not inside a git work tree: {error}")
            return 2

    failed = self_test()
    if failed:
        return failed
    return check(root)


if __name__ == "__main__":
    sys.exit(main())
