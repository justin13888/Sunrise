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

Scope, and why it stops where it does
-------------------------------------

**In:** relative file links, in-file anchors (`#foo`) and cross-file anchors
(`./other.md#foo`), across every `.md` file git tracks — not only `docs/`, since
`README.md`, `AGENTS.md` and the crate-level markdown carry links too.

**Out:** anything with a scheme (`http:`, `https:`, `mailto:`). External
reachability is flaky by construction — this repository's docs cite plenty of
third-party URLs — and a gate that goes red on someone else's outage is a gate
people learn to ignore. Rustdoc intra-doc links are the compiler's job, not
this script's.

That boundary is also why this is standard-library Python rather than `lychee`
or `markdown-link-check`: with external URLs out of scope there is nothing left
to justify a new dependency, a network call, or a tool that the repository's
four other gates would be the only ones not to be.

GitHub's slug rules, which are the whole difficulty
---------------------------------------------------

An anchor is the heading's *rendered text* put through `github-slugger`:

  1. lower-case it;
  2. delete every character that is not alphanumeric, a space, a hyphen or an
     underscore;
  3. replace each remaining space with a hyphen;
  4. if that slug is already taken in this file, append `-1`, `-2`, … until it
     is not.

Step 2 is the trap. A character like `→` or an em dash is **removed, not
replaced**, so the spaces that surrounded it survive as two hyphens:

    ## Amendment (2026-08): OR-Set → append-only row
    ->  amendment-2026-08-or-set--append-only-row

`(`, `)` and `:` vanish without leaving a gap because they are not surrounded by
spaces on both sides, so `(2026-08):` contributes a single hyphen. Getting that
asymmetry wrong by eye is exactly the defect above, and both real cases are
asserted in `self_test()` below.

Rendered text also means inline markup is resolved before slugging: a code span
contributes its contents, a link contributes its label, emphasis markers are
dropped. Fenced code blocks are skipped entirely — a code sample may legitimately
contain a fake path — as are inline code spans, so a documented `[a](b)` example
is not mistaken for a link.

Usage: docs-link-gate.py [--root PATH] [--self-test]
Exit 0 clean, 1 when a link or anchor does not resolve, 2 if the gate could not
run at all (which is a failure, not a pass).
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import unicodedata
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import unquote

# A destination with one of these is somebody else's to keep alive.
SCHEME = re.compile(r"^[A-Za-z][A-Za-z0-9+.\-]*:")

# ```lang / ~~~lang, indented up to three spaces, per CommonMark.
FENCE = re.compile(r"^ {0,3}(?P<char>`{3,}|~{3,})(?P<info>.*)$")

# `# Heading`, up to six hashes, optional closing run of hashes.
ATX = re.compile(r"^ {0,3}(?P<hashes>#{1,6})(?:[ \t]+(?P<text>.*?))?[ \t]*$")

# The underline form: `Heading` on one line, `===` or `---` on the next.
SETEXT = re.compile(r"^ {0,3}(?P<char>=+|-+)[ \t]*$")

# Inline constructs resolved to their rendered text before slugging.
CODE_SPAN = re.compile(r"(?P<ticks>`+)(?P<body>.+?)(?P=ticks)", re.DOTALL)
INLINE_LINK = re.compile(r"!?\[(?P<label>[^\]]*)\]\([^)]*\)")
REFERENCE_LINK = re.compile(r"!?\[(?P<label>[^\]]*)\]\[[^\]]*\]")
HTML_TAG = re.compile(r"</?[A-Za-z][^>]*>")
EMPHASIS = re.compile(r"[*~]{1,3}")
# `_` is kept by the slug rules, so unlike `*` and `~` it has to be recognised
# as an emphasis delimiter rather than left to be stripped as punctuation — and
# only at a word boundary, because GitHub does not emphasise `under_scores`.
UNDERSCORE = re.compile(r"(?<![0-9A-Za-z_])_{1,3}(?=\S)|(?<=\S)_{1,3}(?![0-9A-Za-z_])")


@dataclass(frozen=True)
class Link:
    """One markdown link, with where it was written."""

    path: Path
    line: int
    text: str
    dest: str


def slugify(heading_text: str, taken: dict[str, int]) -> str:
    """Reproduce `github-slugger` for one heading, mutating the per-file counter.

    `taken` maps every slug already handed out in this file to the number of
    times its *base* has been claimed, which is how the `-1`/`-2` suffixes are
    numbered: the second `## Notes` becomes `notes-1`, and a literal `## Notes 1`
    written afterwards would take `notes-1-1` rather than colliding.
    """
    text = heading_text.lower()
    # Normalise so a combining sequence slugs the same as its composed form;
    # GitHub's renderer emits NFC and slugs that.
    text = unicodedata.normalize("NFC", text)
    kept = [ch for ch in text if ch.isalnum() or ch in " -_"]
    # Only U+0020 becomes a hyphen. A tab or a non-breaking space is not
    # alphanumeric and was dropped above, which is what github-slugger does too.
    base = "".join(kept).replace(" ", "-")

    slug = base
    while slug in taken:
        taken[base] += 1
        slug = f"{base}-{taken[base]}"
    taken[slug] = 0
    return slug


def render_inline(text: str) -> str:
    """Strip inline markup down to the text GitHub slugs."""
    text = CODE_SPAN.sub(lambda m: m.group("body"), text)
    text = INLINE_LINK.sub(lambda m: m.group("label"), text)
    text = REFERENCE_LINK.sub(lambda m: m.group("label"), text)
    text = HTML_TAG.sub("", text)
    return UNDERSCORE.sub("", EMPHASIS.sub("", text))


def mask_code_spans(line: str) -> str:
    """Blank out inline code so a documented `[a](b)` is not read as a link."""
    return CODE_SPAN.sub(lambda m: " " * len(m.group(0)), line)


def strip_front_matter(lines: list[str]) -> tuple[list[str], int]:
    """Drop a leading YAML block, returning the rest and the line offset."""
    if lines and lines[0].rstrip() == "---":
        for index in range(1, len(lines)):
            if lines[index].rstrip() in {"---", "..."}:
                return lines[index + 1 :], index + 1
    return lines, 0


def scan(path: Path, root: Path) -> tuple[set[str], list[Link]]:
    """Return every heading slug in one file, and every link written in it."""
    body, offset = strip_front_matter(path.read_text(encoding="utf-8").splitlines())

    slugs: set[str] = set()
    taken: dict[str, int] = {}
    links: list[Link] = []

    fence: str | None = None
    previous: str | None = None  # the candidate line for a setext underline

    for index, raw in enumerate(body):
        line_no = offset + index + 1

        if fence is not None:
            closer = FENCE.match(raw)
            if closer and closer.group("char")[0] == fence[0] and len(closer.group("char")) >= len(fence) and not closer.group("info").strip():
                fence = None
            previous = None
            continue

        opener = FENCE.match(raw)
        if opener:
            fence = opener.group("char")
            previous = None
            continue

        atx = ATX.match(raw)
        if atx:
            # A closing run of hashes is decoration, not content — but only
            # when it is preceded by whitespace, per CommonMark, so a heading
            # ending in `C#` keeps it.
            text = re.sub(r"(?:^|[ \t])#+[ \t]*$", "", (atx.group("text") or "")).strip()
            slugs.add(slugify(render_inline(text), taken))
            previous = None
            continue

        underline = SETEXT.match(raw)
        if underline and previous is not None and previous.strip() and "|" not in previous:
            slugs.add(slugify(render_inline(previous.strip()), taken))
            previous = None
            continue

        for dest, text in find_links(mask_code_spans(raw)):
            links.append(Link(path=path.relative_to(root), line=line_no, text=text, dest=dest))

        previous = raw

    return slugs, links


def find_links(line: str) -> list[tuple[str, str]]:
    """Pull `(destination, label)` out of every inline link on one line.

    Written as a scanner rather than a regex because a destination may contain
    balanced parentheses, which a regex cannot count.
    """
    found: list[tuple[str, str]] = []
    index = 0
    while True:
        open_bracket = line.find("[", index)
        if open_bracket < 0:
            return found
        close_bracket = line.find("]", open_bracket)
        if close_bracket < 0:
            return found
        if not line.startswith("](", close_bracket):
            index = open_bracket + 1
            continue

        cursor = close_bracket + 2
        depth = 1
        while cursor < len(line) and depth:
            if line[cursor] == "(" and line[cursor - 1] != "\\":
                depth += 1
            elif line[cursor] == ")" and line[cursor - 1] != "\\":
                depth -= 1
            cursor += 1
        if depth:
            index = open_bracket + 1
            continue

        dest = line[close_bracket + 2 : cursor - 1].strip()
        # An optional title follows the destination: `(path "Title")`.
        if " " in dest and dest[-1] in "\"')":
            dest = dest.split(" ", 1)[0]
        if dest.startswith("<") and dest.endswith(">"):
            dest = dest[1:-1]
        found.append((dest, line[open_bracket + 1 : close_bracket]))
        index = cursor


def tracked_markdown(root: Path) -> list[Path]:
    """Every `.md` file git tracks, so vendored and ignored trees stay out."""
    try:
        listed = subprocess.run(
            ["git", "-C", str(root), "ls-files", "-z", "*.md"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"::error::docs-links: `git ls-files` failed: {error}")
        raise SystemExit(2) from error
    return sorted(root / name for name in listed.split("\0") if name)


def check(root: Path) -> int:
    files = tracked_markdown(root)
    if not files:
        print("::error::docs-links: git reports no tracked markdown; the gate could not run.")
        return 2

    slugs: dict[Path, set[str]] = {}
    links: list[Link] = []
    for path in files:
        file_slugs, file_links = scan(path, root)
        slugs[path.relative_to(root)] = file_slugs
        links.extend(file_links)

    broken = 0
    for link in links:
        dest = link.dest
        if not dest or dest.startswith("//") or SCHEME.match(dest):
            continue

        target, _, fragment = dest.partition("#")
        target = unquote(target)
        fragment = unquote(fragment)

        if target:
            # A leading `/` means the repository root; anything else is
            # relative to the directory the link was written in.
            resolved = (root / target.lstrip("/")) if target.startswith("/") else (root / link.path).parent / target
            try:
                resolved = resolved.resolve()
                resolved.relative_to(root.resolve())
            except (OSError, ValueError):
                print(f"::error file={link.path},line={link.line}::docs-links: [{link.text}]({dest}) escapes the repository.")
                broken += 1
                continue
            if not resolved.exists():
                print(f"::error file={link.path},line={link.line}::docs-links: [{link.text}]({dest}) points at no such file.")
                broken += 1
                continue
            anchor_file = resolved.relative_to(root.resolve())
        else:
            anchor_file = link.path

        if not fragment:
            continue
        # `#L42` on a source file is a GitHub line anchor, not a heading; only
        # markdown has headings to resolve against.
        if anchor_file.suffix.lower() != ".md":
            continue
        if anchor_file not in slugs:
            print(f"::error file={link.path},line={link.line}::docs-links: [{link.text}]({dest}) targets untracked markdown.")
            broken += 1
            continue
        if fragment not in slugs[anchor_file]:
            where = "in this file" if anchor_file == link.path else f"in {anchor_file}"
            print(
                f"::error file={link.path},line={link.line}::docs-links: "
                f"[{link.text}]({dest}) has no heading {where} slugging to `{fragment}`."
            )
            broken += 1

    print(f"docs-links: {len(links)} links across {len(files)} markdown files.")
    if broken:
        print(f"::error::docs-links: {broken} link(s) resolve to nothing.")
        return 1
    print("OK: docs-links clean.")
    return 0


def self_test() -> int:
    """Assert the slug rules against the two real ADR headings that motivated them.

    Both are checked in the tree by the gate proper; asserting them here is what
    makes a slugifier change that breaks the arrow case fail loudly rather than
    quietly start reporting a correct link as broken.
    """
    cases: list[tuple[str, str]] = [
        # The correct neighbour: `(`, `)` and `:` leave no gap.
        ("Amendment (2026-08): tracing carries the transport", "amendment-2026-08-tracing-carries-the-transport"),
        # The arrow is removed, so the spaces around it become two hyphens.
        ("Amendment (2026-08): OR-Set → append-only row", "amendment-2026-08-or-set--append-only-row"),
        # An em dash behaves exactly like the arrow.
        ("Scope — and its edges", "scope--and-its-edges"),
        ("Why `slugify` exists", "why-slugify-exists"),
        ("See [the ADR](./0013.md)", "see-the-adr"),
        ("**Bold** and _thin_", "bold-and-thin"),
        # A trailing space survives as a hyphen; only the ATX parser above
        # removes a closing run of hashes, and it does so before slugging.
        ("Amendment ", "amendment-"),
        ("under_scores kept", "under_scores-kept"),
    ]
    failures = 0
    for text, expected in cases:
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

    if failures:
        return 1
    print(f"OK: docs-links self-test clean ({len(cases) + 1} cases).")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--root", default=None, help="repository root (default: the working tree this script is in)")
    parser.add_argument("--self-test", action="store_true", help="assert the slug rules and exit")
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    if args.root:
        root = Path(args.root)
    else:
        try:
            root = Path(
                subprocess.run(
                    ["git", "rev-parse", "--show-toplevel"],
                    capture_output=True,
                    text=True,
                    check=True,
                ).stdout.strip()
            )
        except (OSError, subprocess.CalledProcessError) as error:
            print(f"::error::docs-links: not inside a git work tree: {error}")
            return 2

    failed = self_test()
    if failed:
        return failed
    return check(root)


if __name__ == "__main__":
    sys.exit(main())
