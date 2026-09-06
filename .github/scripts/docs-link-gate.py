#!/usr/bin/env python3
"""Fail when a markdown link or heading anchor in this repository resolves to nothing.

Why this gate exists
--------------------

Nothing checked that a relative link or an in-file anchor pointed at anything
real, so a mis-transcribed anchor was invisible: the reader clicks it, GitHub
finds no such fragment, and silently leaves them at the top of the file. The
live instance was an ADR cross-reference —
`0013-focus-session-op-representation.md` linked to
`#amendment-2026-08--or-set--append-only-row` while its own heading slugs to
`amendment-2026-08-or-set--append-only-row`, one hyphen after `08` rather than
two. The neighbouring ADR-0010 does the same thing correctly, which is what made
it read as a typo rather than as a convention.

The class is live rather than theoretical: four separate broken references were
found by hand during one documentation cycle, and two independent passes each
wrote a throwaway validator to answer the same question this script now answers.

Scope
-----

**In:** relative file links, in-file anchors (`#foo`) and cross-file anchors
(`./other.md#foo`), plus image destinations and reference-link *definitions*
(`[label]: ./path.md`), across every `.md` file git tracks — not only `docs/`,
since `README.md`, `AGENTS.md` and the crate-level markdown carry links too.

**Out, and deliberately:**

* Anything with a scheme (`http:`, `https:`, `mailto:`) or a protocol-relative
  `//`. External reachability is flaky by construction — this repository's docs
  cite plenty of third-party URLs — and a gate that goes red on someone else's
  outage is a gate people learn to ignore. With external URLs excluded there is
  nothing left to justify `lychee` or `markdown-link-check`, which is why this
  is standard-library Python like the four gates beside it.
* Rustdoc intra-doc links. Those are the compiler's to check, not this
  script's.
* HTML `<a href>` and `<img src>` elements. No markdown file in this repository
  contains one, to a local path or otherwise; adding one would put its link
  outside this gate. That is a declared boundary, not deferred work.
* Whether a reference-style *use* (`[a][b]`) has a matching definition. A
  dangling definition is already caught above, and an unmatched use renders as
  visible literal text rather than as a silently dead link.

Resolution is against the set of files **git tracks**, not against the
filesystem. That is one decision closing two holes: an untracked file cannot
satisfy a link that is dead for every reader on github.com, and the comparison
is exact-case, so `./ReadMe.md` for `README.md` fails here exactly as it fails
on a Linux runner and on github.com — rather than passing on a case-insensitive
macOS checkout and failing in CI.

GitHub's slug rules, which are the whole difficulty
---------------------------------------------------

An anchor is the heading's *rendered text* put through `github-slugger`:

  1. lower-case it;
  2. delete every character whose Unicode General Category is not one of
     `L*` (letter), `M*` (mark), `Nd` (decimal number) or `Pc` (connector
     punctuation) — except U+0020 SPACE and U+002D HYPHEN-MINUS, which survive;
  3. replace each remaining space with a hyphen;
  4. if that slug is already taken in this file, append `-1`, `-2`, … until it
     is not.

Step 2 is stated as a keep-set rather than as "delete punctuation, symbols,
separators and control characters" because the latter is wrong in a way that
bites: `²`, `½` and `Ⅷ` are `No`/`Nl`, neither punctuation nor symbol, and
`github-slugger` deletes them. `slug_categories_match_github_slugger()` below
derives the deletion set for U+0000–U+00FF from the rule above and asserts it
equals `github-slugger`'s own published ranges, `_` (`Pc`), `ª`/`µ`/`º` (letters)
and NBSP (`Zs`, deleted) included.

Step 2 is also the trap that produced the defect. A character like `→` or an em
dash is **removed, not replaced**, so the spaces that surrounded it survive as
two hyphens:

    ## Amendment (2026-08): OR-Set → append-only row
    ->  amendment-2026-08-or-set--append-only-row

`(`, `)` and `:` vanish without leaving a gap because they are not surrounded by
spaces on both sides, so `(2026-08):` contributes a single hyphen. Getting that
asymmetry wrong by eye is exactly the defect above, and both real cases are
asserted in `self_test()`.

Neither side is Unicode-normalised, because GitHub normalises neither: an NFD
heading and an NFD link agree there and must agree here.

Rendered text means inline markup is resolved before slugging: a code span
contributes its contents, a link contributes its label, an HTML comment
contributes nothing, an entity contributes the character it names, emphasis
markers are dropped, and the result is trimmed.

How the file is read
--------------------

The whole file is parsed as one string rather than line by line, because these
documents are hard-wrapped and a link's label routinely straddles a newline:

    ... the email addresses [logging.md
    §6.1](../10-cross-cutting/logging.md#61-email-addresses) was guarding.

A line-at-a-time scanner cannot see that link at all — it is not checked, and,
worse, not counted, so the total it reports is over an unknown denominator.

Code is masked before links are extracted, position-preserved so reported line
numbers stay true: YAML front matter, fenced code blocks (with the opening
indent measured against the enclosing list item, so a fence indented six spaces
inside a nested list is still a fence), indented code blocks, and inline code
spans. A code sample may legitimately contain a fake path, and an indented
sample is that same case.

Anything shaped like a link that cannot be parsed is reported as a warning and
counted, never as a failure: prose legitimately contains brackets, and a gate
that goes red on prose is one people route around. But it is printed, because a
gate that says "N links, clean" over a denominator it silently trimmed gives the
false assurance it exists to remove.

Usage: docs-link-gate.py [--root PATH] [--self-test]
Exit 0 clean, 1 when a link or anchor does not resolve, 2 if the gate could not
run at all (which is a failure, not a pass).
"""

from __future__ import annotations

import argparse
import html
import posixpath
import re
import subprocess
import sys
import unicodedata
from bisect import bisect_right
from dataclasses import dataclass
from urllib.parse import unquote

# --------------------------------------------------------------------------
# Slug rules
# --------------------------------------------------------------------------

# Every General Category `github-slugger` keeps. Everything else is deleted,
# except the two characters below.
SLUG_KEEP_CATEGORIES = frozenset({"Lu", "Ll", "Lt", "Lm", "Lo", "Mn", "Mc", "Me", "Nd", "Pc"})
SLUG_KEEP_CHARS = frozenset({" ", "-"})

# `github-slugger`'s own generated regex, as deletion ranges over Latin-1. Used
# by the self-test to prove the category rule above reproduces it rather than
# approximates it.
GITHUB_SLUGGER_LATIN1_RANGES = (
    (0x00, 0x1F), (0x21, 0x2C), (0x2E, 0x2F), (0x3A, 0x40), (0x5B, 0x5E),
    (0x60, 0x60), (0x7B, 0xA9), (0xAB, 0xB4), (0xB6, 0xB9), (0xBB, 0xBF),
    (0xD7, 0xD7), (0xF7, 0xF7),
)

# --------------------------------------------------------------------------
# Block structure
# --------------------------------------------------------------------------

SCHEME = re.compile(r"^[A-Za-z][A-Za-z0-9+.\-]*:")
QUOTE_PREFIX = re.compile(r"^(?:[ \t]{0,3}>[ \t]?)+")
FENCE = re.compile(r"^(?P<indent>[ \t]*)(?P<char>`{3,}|~{3,})(?P<info>.*)$")
ATX = re.compile(r"^(?P<indent>[ \t]*)(?P<hashes>#{1,6})(?:[ \t]+(?P<text>.*?))?[ \t]*$")
SETEXT = re.compile(r"^[ \t]*(?P<char>=+|-+)[ \t]*$")
LIST_ITEM = re.compile(r"^(?P<indent>[ \t]*)(?P<marker>[-*+]|\d{1,9}[.)])(?P<gap>[ \t]+|$)")
TABLE_ROW = re.compile(r"\|")
DEFINITION = re.compile(r"^[ \t]{0,3}\[(?P<label>[^\]\n]+)\]:[ \t]*(?P<dest>\S+)")

# --------------------------------------------------------------------------
# Inline structure
# --------------------------------------------------------------------------

# A code span may not contain a blank line, which is what stops a stray backtick
# in prose from swallowing half a document.
CODE_SPAN = re.compile(r"(?P<ticks>`+)(?!`)(?P<body>(?:[^\n`]|\n(?![ \t]*\n)|`(?!`))+?)(?P=ticks)(?!`)")
HTML_COMMENT = re.compile(r"<!--.*?-->", re.DOTALL)
INLINE_LINK = re.compile(r"!?\[(?P<label>[^\]]*)\]\([^)]*\)")
REFERENCE_LINK = re.compile(r"!?\[(?P<label>[^\]]*)\]\[[^\]]*\]")
HTML_TAG = re.compile(r"</?[A-Za-z][^>]*>")
EMPHASIS = re.compile(r"[*~]{1,3}")
# `_` is kept by the slug rules (it is `Pc`), so unlike `*` and `~` it has to be
# recognised as an emphasis delimiter rather than left to be deleted as
# punctuation — and only at a word boundary, because GitHub does not emphasise
# `under_scores`.
UNDERSCORE = re.compile(r"(?<![0-9A-Za-z_])_{1,3}(?=\S)|(?<=\S)_{1,3}(?![0-9A-Za-z_])")

BLANK_LINE = re.compile(r"\n[ \t]*\n")
TAB_WIDTH = 4


@dataclass(frozen=True)
class Link:
    """One markdown link destination, with where it was written."""

    line: int
    text: str
    dest: str


@dataclass(frozen=True)
class Unparsed:
    """Something shaped like a link that the scanner could not read."""

    line: int
    excerpt: str


def slugify(heading_text: str, taken: dict[str, int]) -> str:
    """Reproduce `github-slugger` for one heading, mutating the per-file counter.

    `taken` maps every slug already handed out in this file to the number of
    times its *base* has been claimed, which is how the `-1`/`-2` suffixes are
    numbered: the second `## Notes` becomes `notes-1`, and a literal `## Notes 1`
    written afterwards would take `notes-1-1` rather than colliding.
    """
    lowered = heading_text.lower()
    kept = [
        ch
        for ch in lowered
        if ch in SLUG_KEEP_CHARS or unicodedata.category(ch) in SLUG_KEEP_CATEGORIES
    ]
    base = "".join(kept).replace(" ", "-")

    slug = base
    while slug in taken:
        taken[base] += 1
        slug = f"{base}-{taken[base]}"
    taken[slug] = 0
    return slug


def render_inline(text: str) -> str:
    """Strip inline markup down to the text GitHub slugs.

    Order matters. Code spans come first so a documented `` `<b>` `` keeps its
    angle brackets as text; HTML is removed next; entities are decoded last so a
    written `&lt;script&gt;` becomes the literal text it renders as rather than
    being mistaken for a tag.
    """
    text = CODE_SPAN.sub(lambda m: m.group("body"), text)
    text = HTML_COMMENT.sub("", text)
    text = INLINE_LINK.sub(lambda m: m.group("label"), text)
    text = REFERENCE_LINK.sub(lambda m: m.group("label"), text)
    text = HTML_TAG.sub("", text)
    text = UNDERSCORE.sub("", EMPHASIS.sub("", text))
    return html.unescape(text).strip()


def indent_width(prefix: str) -> int:
    """Columns a run of spaces and tabs occupies, tabs stopping every four."""
    width = 0
    for ch in prefix:
        width = width + (TAB_WIDTH - width % TAB_WIDTH) if ch == "\t" else width + 1
    return width


def blank(line: str) -> str:
    """A same-length line of spaces, so masking does not move any offset."""
    return " " * len(line)


def mask_blocks(lines: list[str]) -> tuple[list[str], list[tuple[int, str]]]:
    """Blank every line that is code, and return the heading lines found.

    Headings are left unmasked: a heading may itself contain a link, and that
    link deserves checking like any other.
    """
    masked: list[str] = []
    headings: list[tuple[int, str]] = []
    taken_previous: str | None = None

    fence: tuple[str, int] | None = None  # (delimiter run, indent it opened at)
    containers: list[int] = []  # content indent of each open list item
    in_paragraph = False
    in_indented_code = False
    # Front matter only if the opening delimiter is actually closed. A file that
    # opens with a `---` thematic break must not mask the rest of the document:
    # that would silently drop every link in it, which is the failure this gate
    # reports rather than commits.
    front_matter = bool(lines) and lines[0].rstrip() == "---" and any(
        line.rstrip() in {"---", "..."} for line in lines[1:]
    )

    for index, raw in enumerate(lines):
        if front_matter:
            masked.append(blank(raw))
            if index > 0 and raw.rstrip() in {"---", "..."}:
                front_matter = False
            continue

        quote = QUOTE_PREFIX.match(raw)
        body = raw[quote.end() :] if quote else raw
        stripped = body.strip()

        if fence is not None:
            masked.append(blank(raw))
            closer = FENCE.match(body)
            if (
                closer
                and closer.group("char")[0] == fence[0][0]
                and len(closer.group("char")) >= len(fence[0])
                and not closer.group("info").strip()
                and indent_width(closer.group("indent")) - fence[1] < 4
            ):
                fence = None
            taken_previous = None
            continue

        if not stripped:
            masked.append(raw)
            in_paragraph = False
            in_indented_code = False
            taken_previous = None
            continue

        indent = indent_width(body[: len(body) - len(body.lstrip(" \t"))])
        while containers and indent < containers[-1] and not in_paragraph:
            containers.pop()
        base = containers[-1] if containers else 0
        relative = indent - base

        # An indented code block runs until a non-blank line comes back out.
        if in_indented_code and relative >= 4:
            masked.append(blank(raw))
            taken_previous = None
            continue
        in_indented_code = False

        if relative >= 4 and not in_paragraph:
            in_indented_code = True
            masked.append(blank(raw))
            taken_previous = None
            continue

        opener = FENCE.match(body)
        if opener and indent_width(opener.group("indent")) - base < 4:
            fence = (opener.group("char"), indent)
            masked.append(blank(raw))
            in_paragraph = False
            taken_previous = None
            continue

        item = LIST_ITEM.match(body)
        if item:
            containers.append(indent_width(item.group("indent") + item.group("marker") + item.group("gap")))

        atx = ATX.match(body)
        if atx and relative < 4:
            # A closing run of hashes is decoration, not content — but only when
            # preceded by whitespace, per CommonMark, so a heading ending in `C#`
            # keeps it.
            text = re.sub(r"(?:^|[ \t])#+[ \t]*$", "", (atx.group("text") or "")).strip()
            headings.append((index + 1, text))
            masked.append(raw)
            in_paragraph = False
            taken_previous = None
            continue

        underline = SETEXT.match(body)
        if underline and taken_previous is not None:
            headings.append((index, taken_previous))
            masked.append(raw)
            in_paragraph = False
            taken_previous = None
            continue

        masked.append(raw)
        in_paragraph = True
        taken_previous = (
            stripped
            if not TABLE_ROW.search(body) and not item and not stripped.startswith(">")
            else None
        )

    return masked, headings


def mask_code_spans(text: str) -> str:
    """Blank out inline code so a documented `[a](b)` is not read as a link."""
    return CODE_SPAN.sub(lambda m: " " * len(m.group(0)), text)


def find_links(text: str) -> tuple[list[tuple[int, str, str]], list[int]]:
    """Pull `(offset, destination, label)` out of every inline link.

    Runs over the whole document rather than one line, because these files are
    hard-wrapped and a label routinely straddles a newline. Written as a scanner
    rather than a regex because both the label and the destination may contain
    balanced brackets, which a regex cannot count.

    Returns the links found and the offsets of every `](` that could not be
    read, which the caller reports as a warning rather than as a failure.
    """
    found: list[tuple[int, str, str]] = []
    parsed: set[int] = set()
    index = 0
    length = len(text)

    while True:
        start = text.find("[", index)
        if start < 0:
            break

        cursor = start
        depth = 0
        close = -1
        while cursor < length:
            ch = text[cursor]
            if ch == "\\":
                cursor += 2
                continue
            if ch == "[":
                depth += 1
            elif ch == "]":
                depth -= 1
                if depth == 0:
                    close = cursor
                    break
            cursor += 1

        if close < 0 or not text.startswith("](", close):
            index = start + 1
            continue

        label = text[start + 1 : close]
        if BLANK_LINE.search(label):
            # A blank line ends the block, so these brackets are not one link.
            index = start + 1
            continue

        cursor = close + 2
        depth = 1
        while cursor < length and depth:
            ch = text[cursor]
            if ch == "\\":
                cursor += 2
                continue
            if ch == "(":
                depth += 1
            elif ch == ")":
                depth -= 1
            cursor += 1

        raw_dest = text[close + 2 : cursor - 1]
        if depth or BLANK_LINE.search(raw_dest):
            index = start + 1
            continue

        dest = raw_dest.strip()
        # An optional title follows the destination: `(path "Title")`.
        if dest:
            dest = dest.split(None, 1)[0]
        if dest.startswith("<") and dest.endswith(">"):
            dest = dest[1:-1]

        parsed.add(close)
        # A wrapped label carries the blockquote markers of the lines it
        # crosses; they are noise in the reported message, not in the check.
        found.append((start, dest, " ".join(QUOTE_PREFIX.sub("", part).strip() for part in label.split("\n")).strip()))
        index = cursor

    unparsed = [m.start() for m in re.finditer(r"\]\(", text) if m.start() not in parsed]
    return found, unparsed


def scan_text(text: str) -> tuple[set[str], list[Link], list[Unparsed]]:
    """Every heading slug, link and unreadable link-shape in one document."""
    normalised = text.replace("\r\n", "\n").replace("\r", "\n")
    lines = normalised.split("\n")

    masked_lines, headings = mask_blocks(lines)
    masked = mask_code_spans("\n".join(masked_lines))

    starts = [0]
    for line in masked_lines[:-1]:
        starts.append(starts[-1] + len(line) + 1)

    def line_of(offset: int) -> int:
        return bisect_right(starts, offset)

    taken: dict[str, int] = {}
    slugs = {slugify(render_inline(text_of), taken) for _, text_of in headings}

    links: list[Link] = []
    raw_links, unparsed_offsets = find_links(masked)
    for offset, dest, label in raw_links:
        links.append(Link(line=line_of(offset), text=label, dest=dest))

    # Reference-link definitions. A dangling one is a defect on its own terms,
    # so it is checked without matching it to a use.
    for index, line in enumerate(masked_lines):
        definition = DEFINITION.match(line)
        if definition:
            links.append(
                Link(line=index + 1, text=definition.group("label"), dest=definition.group("dest"))
            )

    unparsed = [
        Unparsed(line=line_of(offset), excerpt=" ".join(masked[max(0, offset - 30) : offset + 30].split()))
        for offset in unparsed_offsets
    ]
    return slugs, links, unparsed


def git_tracked(root: str) -> list[str]:
    """Every path git tracks, which is what "this file exists" has to mean.

    A link in committed documentation to a file that is not committed is dead
    for every reader on github.com, and comparing against this set rather than
    against the filesystem is also exact-case — so a `./ReadMe.md` for
    `README.md` fails here the same way it fails on a Linux runner, instead of
    passing on a case-insensitive macOS checkout.
    """
    try:
        listed = subprocess.run(
            ["git", "-C", root, "ls-files", "-z"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"::error::docs-links: `git ls-files` failed: {error}")
        raise SystemExit(2) from error
    return [name for name in listed.split("\0") if name]


def check(root: str) -> int:
    tracked = git_tracked(root)
    files = sorted(name for name in tracked if name.endswith(".md"))
    if not files:
        print("::error::docs-links: git reports no tracked markdown; the gate could not run.")
        return 2

    tracked_files = set(tracked)
    tracked_dirs = {""}
    for name in tracked:
        parent = posixpath.dirname(name)
        while parent and parent not in tracked_dirs:
            tracked_dirs.add(parent)
            parent = posixpath.dirname(parent)

    slugs: dict[str, set[str]] = {}
    links: dict[str, list[Link]] = {}
    unparsed: dict[str, list[Unparsed]] = {}
    for name in files:
        with open(posixpath.join(root, name), encoding="utf-8") as handle:
            file_slugs, file_links, file_unparsed = scan_text(handle.read())
        slugs[name] = file_slugs
        links[name] = file_links
        unparsed[name] = file_unparsed

    broken = 0
    total = 0
    unreadable = 0

    for name in files:
        for note in unparsed[name]:
            unreadable += 1
            print(
                f"::warning file={name},line={note.line}::docs-links: "
                f"could not read a link near `{note.excerpt}`; it was NOT checked."
            )

        for link in links[name]:
            total += 1
            dest = link.dest
            if not dest or dest.startswith("//") or SCHEME.match(dest):
                continue

            target, _, fragment = dest.partition("#")
            target = unquote(target)
            fragment = unquote(fragment)
            where = f"::error file={name},line={link.line}::docs-links: [{link.text}]({dest})"

            if target:
                joined = target[1:] if target.startswith("/") else posixpath.join(posixpath.dirname(name), target)
                resolved = posixpath.normpath(joined)
                if resolved == ".." or resolved.startswith("../"):
                    print(f"{where} escapes the repository.")
                    broken += 1
                    continue
                if resolved in tracked_files:
                    anchor_file = resolved
                elif resolved in tracked_dirs:
                    continue  # a directory link; it has no headings to anchor into
                else:
                    print(f"{where} points at no file git tracks.")
                    broken += 1
                    continue
            else:
                anchor_file = name

            if not fragment:
                continue
            # `#L42` on a source file is a GitHub line anchor, not a heading;
            # only markdown has headings to resolve against.
            if not anchor_file.endswith(".md"):
                continue
            if fragment not in slugs[anchor_file]:
                place = "in this file" if anchor_file == name else f"in {anchor_file}"
                print(f"{where} has no heading {place} slugging to `{fragment}`.")
                broken += 1

    print(f"docs-links: {total} links across {len(files)} markdown files.")
    if unreadable:
        print(f"docs-links: {unreadable} link-shaped construct(s) could not be read and were not checked.")
    if broken:
        print(f"::error::docs-links: {broken} link(s) resolve to nothing.")
        return 1
    print("OK: docs-links clean.")
    return 0


# --------------------------------------------------------------------------
# Self-test. Runs as a precondition of every check, so a change to the rules
# fails loudly rather than quietly starting to report correct links as broken.
# --------------------------------------------------------------------------

SLUG_CASES: tuple[tuple[str, str], ...] = (
    # The correct neighbour: `(`, `)` and `:` leave no gap.
    ("Amendment (2026-08): tracing carries the transport", "amendment-2026-08-tracing-carries-the-transport"),
    # The arrow is removed, so the spaces around it become two hyphens.
    ("Amendment (2026-08): OR-Set → append-only row", "amendment-2026-08-or-set--append-only-row"),
    # An em dash behaves exactly like the arrow.
    ("Scope — and its edges", "scope--and-its-edges"),
    ("Why `slugify` exists", "why-slugify-exists"),
    ("See [the ADR](./0013.md)", "see-the-adr"),
    ("**Bold** and _thin_", "bold-and-thin"),
    ("under_scores kept", "under_scores-kept"),
    # An entity contributes the character it names, and `&` is deleted, so the
    # spaces that surrounded it survive.
    ("A &amp; B", "a--b"),
    # An HTML comment contributes nothing, and the result is trimmed.
    ("Heading <!-- a comment -->", "heading"),
    # Marks are KEPT. An ASCII-only predicate passes every case above and fails
    # every case below, which is the regression this list exists to catch.
    ("हिन्दी शीर्षक", "हिन्दी-शीर्षक"),
    ("日本語の見出し", "日本語の見出し"),
    # `İ`.lower() is `i` + U+0307 COMBINING DOT ABOVE, and the mark survives.
    ("İstanbul", "i̇stanbul"),
    # U+FE0F VARIATION SELECTOR-16 is `Mn`, so it survives while `⚠` (`So`)
    # does not — a github-slugger quirk, reproduced rather than tidied.
    ("⚠️ Warning", "️-warning"),
    # `²` and `½` are `No`: neither punctuation nor symbol, and both deleted.
    ("m² and ½", "m-and-"),
    # A heading that slugs to nothing at all still has to slug to something.
    # Three symbols deleted, the two spaces between them left behind.
    ("→ ← ↑", "--"),
    ("!!!", ""),
)

FIXTURE = """---
status: accepted
---

# Title

Text with a [wrapped
link](./other.md#some-heading) in it.

```
[fenced dangling](./ghost1.md)
```

    [indented dangling](./ghost2.md)

- item

  - nested

      ```
      [deep fenced dangling](./ghost3.md)
      ```

Inline `[code dangling](./ghost4.md)` stays out.

An unbalanced ](( shape.

[def]: ./defined.md
"""

# A file whose first line is a thematic break, not front matter. Every link in
# it must still be found.
FIXTURE_NO_FRONT_MATTER = """---

# Heading

A [link](./after-a-rule.md) below an opening thematic break.
"""


def slug_categories_match_github_slugger() -> bool:
    """Prove the category rule *is* github-slugger's regex, over Latin-1.

    Every interesting disagreement lives in this range — `_` (`Pc`, kept),
    `ª`/`µ`/`º` (letters, kept), `-` and space (kept by exception), NBSP (`Zs`,
    deleted), and `²`/`³`/`¹`/`¼`/`½`/`¾` (`No`, deleted, which is why the rule
    is written as a keep-set and not as "delete punctuation and symbols").
    """
    derived: list[tuple[int, int]] = []
    start: int | None = None
    for code in range(0x100):
        ch = chr(code)
        deleted = ch not in SLUG_KEEP_CHARS and unicodedata.category(ch) not in SLUG_KEEP_CATEGORIES
        if deleted and start is None:
            start = code
        elif not deleted and start is not None:
            derived.append((start, code - 1))
            start = None
    if start is not None:
        derived.append((start, 0xFF))

    if tuple(derived) == GITHUB_SLUGGER_LATIN1_RANGES:
        return True
    print(f"::error::docs-links self-test: derived deletion ranges {derived} != github-slugger's")
    return False


def self_test() -> int:
    failures = 0

    if not slug_categories_match_github_slugger():
        failures += 1

    for text, expected in SLUG_CASES:
        actual = slugify(render_inline(text), {})
        if actual != expected:
            print(f"::error::docs-links self-test: {text!r} slugged to {actual!r}, expected {expected!r}")
            failures += 1

    # Duplicate headings take `-1`, `-2` in the order they appear.
    taken: dict[str, int] = {}
    repeated = [slugify("Notes", taken) for _ in range(3)]
    if repeated != ["notes", "notes-1", "notes-2"]:
        print(f"::error::docs-links self-test: duplicate headings numbered {repeated}")
        failures += 1

    # Neither side is normalised, so an NFD heading answers an NFD link and a
    # composed link to a decomposed heading is correctly reported as broken.
    nfd = unicodedata.normalize("NFD", "Café")
    if slugify(nfd, {}) != nfd.lower() or slugify(nfd, {}) == "café":
        print("::error::docs-links self-test: NFD heading was normalised")
        failures += 1

    slugs, links, unparsed = scan_text(FIXTURE)
    dests = sorted(link.dest for link in links)
    expected_dests = ["./defined.md", "./other.md#some-heading"]
    if dests != expected_dests:
        print(f"::error::docs-links self-test: fixture yielded {dests}, expected {expected_dests}")
        failures += 1
    if slugs != {"title"}:
        print(f"::error::docs-links self-test: fixture headings {sorted(slugs)}, expected ['title']")
        failures += 1
    wrapped = [link for link in links if link.dest == "./other.md#some-heading"]
    # Reported at the line the link *starts* on, which is where a reader looks.
    if not wrapped or wrapped[0].line != 7 or wrapped[0].text != "wrapped link":
        print(f"::error::docs-links self-test: wrapped link read as {wrapped}")
        failures += 1
    if len(unparsed) != 1:
        print(f"::error::docs-links self-test: expected 1 unreadable shape, got {unparsed}")
        failures += 1

    bare_slugs, bare_links, _ = scan_text(FIXTURE_NO_FRONT_MATTER)
    if [link.dest for link in bare_links] != ["./after-a-rule.md"] or bare_slugs != {"heading"}:
        print(
            "::error::docs-links self-test: an opening thematic break was mistaken for "
            f"front matter; found {[l.dest for l in bare_links]} and {sorted(bare_slugs)}"
        )
        failures += 1

    if failures:
        return 1
    print(f"OK: docs-links self-test clean ({len(SLUG_CASES) + 7} cases).")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Check markdown links and heading anchors.")
    parser.add_argument("--root", default=None, help="repository root (default: the working tree this script is in)")
    parser.add_argument("--self-test", action="store_true", help="assert the slug and parsing rules and exit")
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
            print(f"::error::docs-links: not inside a git work tree: {error}")
            return 2

    failed = self_test()
    if failed:
        return failed
    return check(root)


if __name__ == "__main__":
    sys.exit(main())
