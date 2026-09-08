#!/usr/bin/env python3
"""Fail when observability.md's extracted event and metric blocks disagree with the tree.

Why this gate exists
--------------------

`docs/06-server/observability.md` carries two blocks lifted out of the source —
the catalogued `ev = "srv.*"` names and the `sunrise_*` metric names — each
under a marker naming the commit it was last reconciled against. Nothing re-ran
the extraction. `grep -rn "Last extracted" .github/ mise.toml` returned nothing.

The two blocks are the operator-facing inventory of what this server emits, and
a missing entry is invisible in a way a stale document usually is not: the
document still parses, still reads coherently, and its own marker tells the
reader it was checked. It had already drifted twice in one cycle —
`sunrise_relay_batch_duplicate_total` reached the code and `log-events.md` but
not the metric block, and the `ev` count read 24 in the document against 25 in
the tree.

How it decides
--------------

The document specifies its own extraction command, inside the marker comment
above each block:

    grep -rhoE 'ev = "srv\\.[a-z0-9_.]+"' crates/sunrise-server/src | sort -u

so this script *reads that line and runs it* rather than carrying a second copy
of the pattern. The document and the gate cannot disagree about what to extract,
which is the same reason the log-field gate reads an event name out of the same
parse as its fields. Change the command in the document and the gate follows it;
point it at a path that does not exist and the gate says so.

The names rendered in the block come from the quoted portion of that same
pattern — `srv\\.[a-z0-9_.]+` — applied to the fenced block, so the block's
two-column layout, its alignment and its parenthesised asides are all irrelevant
to the comparison. The two sets are then diffed **in both directions**: a name
in the tree and not the block is an undocumented event, and a name in the block
and not the tree is a catalogue still advertising something that was removed.

The count sentence beside each block is checked too, because that is what
actually rotted: the reader sees "The 25 `ev` names" and takes the block as
complete on that word alone.

The `Last extracted` marker
---------------------------

It stays, and its job changes. It was the reader's only assurance; it is now
provenance — which commit a human last reconciled the block against, so a
reviewer can diff that ref against HEAD over the grepped path. The gate asserts
the ref names a commit that exists, which is what keeps it from decaying into a
decorative string. That check is skipped in a shallow clone, where the object
genuinely is not present and a red gate would be about the clone rather than
about the document; the CI job checks out with full history so it runs there.

Scope
-----

**In:** the two extracted blocks in `docs/06-server/observability.md`, their
stated counts, and their provenance markers.

**Out, and deliberately:**

* **Every other list in that document, and every other document.** A gate that
  tried to keep all prose in step with the tree would be a gate nobody could
  keep green. These two blocks are marked as extracted, which is what makes
  them checkable.
* **`docs/10-cross-cutting/log-events.md`.** The event catalogue there is
  already enforced from the other side, by
  `crates/sunrise-log/tests/event_catalog.rs`, which fails when an emitted event
  is not catalogued. Duplicating it here would mean two mechanisms that can
  disagree.
* **Whether a name is *emitted* rather than merely written.** The grep sees a
  string literal. `sunrise_push_apns_total` is defined and never reached, and
  the block says so in a parenthesised aside that this gate ignores.

Usage: observability-catalog-gate.py [--root PATH] [--self-test]
Exit 0 clean, 1 on a violation, 2 if the gate could not run at all.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from dataclasses import dataclass

DOC = "docs/06-server/observability.md"

# The marker comment, and the two facts read out of it.
MARKER = re.compile(
    r"<!--\s*Extracted from the tree.*?-->",
    re.DOTALL,
)
GREP = re.compile(r"grep\s+-rhoE\s+'(?P<pattern>.+?)'\s+(?P<path>\S+)")
EXTRACTED_AT = re.compile(r"Last extracted:\s*(?P<ref>[0-9a-f]{7,40})\b")
FENCE = re.compile(r"^```")
# The count sentence beside each block. Anchored on a distinctive phrase rather
# than on position, so reordering the document does not silently disable it.
COUNTS = (
    ("event", re.compile(r"The (\d+) `ev` names the server emits")),
    ("metric", re.compile(r"^(\d+) metric names, and", re.MULTILINE)),
)


@dataclass(frozen=True)
class Block:
    """One extracted block: what produced it, what it renders, where it is."""

    kind: str  # "event" or "metric", from the order they appear
    pattern: str  # the ERE the document names
    path: str  # the tree path the document names
    ref: str | None  # the `Last extracted` commit, when the marker carries one
    names: tuple[str, ...]  # the names rendered in the fenced block
    line: int  # 1-based line of the marker, for the diagnostic


def inner_pattern(pattern: str) -> str:
    """The quoted name shape inside a grep pattern.

    `ev = "srv\\.[a-z0-9_.]+"` yields `srv\\.[a-z0-9_.]+`, and
    `"sunrise_[a-z0-9_]+"` yields `sunrise_[a-z0-9_]+`. Both of this
    document's patterns wrap the name in the `"` the source writes around it,
    which is what makes one derivation serve the tree side and the block side:
    the tree writes `ev = "srv.start"` and the block renders `srv.start`.
    """
    first = pattern.find('"')
    last = pattern.rfind('"')
    if first < 0 or last <= first:
        raise ValueError(f"no quoted name shape in {pattern!r}")
    return pattern[first + 1 : last]


def parse_blocks(text: str) -> tuple[list[Block], list[str]]:
    """Every marked block in the document, and everything unreadable about them."""
    lines = text.splitlines()
    problems: list[str] = []
    blocks: list[Block] = []
    kinds = ("event", "metric")

    for index, raw in enumerate(lines):
        if "Extracted from the tree" not in raw:
            continue
        marker_line = index + 1
        comment = "\n".join(lines[index:])
        match = MARKER.search(comment)
        if not match:
            problems.append(f"{DOC}:{marker_line}: the extraction marker is never closed with `-->`.")
            continue
        marker = match.group(0)
        end = index + marker.count("\n")

        grep = GREP.search(marker)
        if not grep:
            problems.append(
                f"{DOC}:{marker_line}: the marker names no `grep -rhoE '<pattern>' <path>` "
                "command, so there is nothing to re-run. Restore the command line."
            )
            continue

        ref = EXTRACTED_AT.search(marker)
        cursor = end + 1
        while cursor < len(lines) and not FENCE.match(lines[cursor]):
            if lines[cursor].strip():
                break
            cursor += 1
        if cursor >= len(lines) or not FENCE.match(lines[cursor]):
            problems.append(
                f"{DOC}:{marker_line}: no fenced block follows the extraction marker."
            )
            continue
        body: list[str] = []
        cursor += 1
        while cursor < len(lines) and not FENCE.match(lines[cursor]):
            body.append(lines[cursor])
            cursor += 1
        if cursor >= len(lines):
            problems.append(f"{DOC}:{marker_line}: the block opened after the marker is never closed.")
            continue

        try:
            shape = re.compile(inner_pattern(grep.group("pattern")))
        except (ValueError, re.error) as error:
            problems.append(f"{DOC}:{marker_line}: the marker's pattern is unusable: {error}")
            continue

        kind = kinds[len(blocks)] if len(blocks) < len(kinds) else f"block-{len(blocks) + 1}"
        blocks.append(
            Block(
                kind=kind,
                pattern=grep.group("pattern"),
                path=grep.group("path"),
                ref=ref.group("ref") if ref else None,
                names=tuple(sorted(set(shape.findall("\n".join(body))))),
                line=marker_line,
            )
        )
    return blocks, problems


def names_in_tree(root: str, block: Block) -> tuple[set[str], list[str]]:
    """Run the document's own extraction over the tree it names."""
    problems: list[str] = []
    try:
        whole = re.compile(block.pattern)
        shape = re.compile(inner_pattern(block.pattern))
    except (ValueError, re.error) as error:
        return set(), [f"{DOC}:{block.line}: pattern {block.pattern!r} is unusable: {error}"]

    base = os.path.join(root, block.path)
    if not os.path.isdir(base):
        return set(), [
            f"{DOC}:{block.line}: the marker greps `{block.path}`, which is not a directory. "
            "Point it at the source the block is extracted from."
        ]

    found: set[str] = set()
    read = 0
    for directory, _, files in os.walk(base):
        for name in sorted(files):
            if not name.endswith(".rs"):
                continue
            try:
                with open(os.path.join(directory, name), encoding="utf-8") as handle:
                    text = handle.read()
            except (OSError, UnicodeDecodeError) as error:
                problems.append(f"{block.path}/{name}: unreadable ({error})")
                continue
            read += 1
            for hit in whole.findall(text):
                inner = shape.search(hit if isinstance(hit, str) else "".join(hit))
                if inner:
                    found.add(inner.group(0))
    if read == 0:
        problems.append(
            f"{DOC}:{block.line}: no `.rs` file under `{block.path}`, so the extraction read nothing."
        )
    return found, problems


def compare(block: Block, tree: set[str]) -> list[str]:
    """The set difference, in both directions, as reader-facing lines."""
    listed = set(block.names)
    problems: list[str] = []
    for name in sorted(tree - listed):
        problems.append(
            f"{DOC}:{block.line}: `{name}` is in the tree and not in the {block.kind} block. "
            "Add it, and move `Last extracted` to the commit you reconciled against."
        )
    for name in sorted(listed - tree):
        problems.append(
            f"{DOC}:{block.line}: `{name}` is in the {block.kind} block and not in the tree. "
            "It was removed from the source and the catalogue still advertises it."
        )
    return problems


def check_counts(text: str, blocks: list[Block]) -> list[str]:
    """The stated count beside each block, which is what actually rotted."""
    problems: list[str] = []
    by_kind = {block.kind: block for block in blocks}
    for kind, pattern in COUNTS:
        block = by_kind.get(kind)
        if block is None:
            continue
        match = pattern.search(text)
        if not match:
            problems.append(
                f"{DOC}: no sentence states how many {kind} names the block holds. "
                f"This gate looks for {pattern.pattern!r}; a reader takes the block as "
                "complete on that sentence alone, so it has to be there to be checked."
            )
            continue
        stated = int(match.group(1))
        if stated != len(block.names):
            problems.append(
                f"{DOC}: the prose says {stated} {kind} names and the block lists "
                f"{len(block.names)}."
            )
    return problems


def check_provenance(root: str, blocks: list[Block]) -> list[str]:
    """`Last extracted` has to name a commit, or it is a decorative string."""
    problems: list[str] = []
    missing = [b for b in blocks if b.ref is None]
    for block in missing:
        problems.append(
            f"{DOC}:{block.line}: the marker carries no `Last extracted:` commit. It is the "
            "provenance a reviewer diffs against HEAD over the grepped path."
        )
    shallow = subprocess.run(
        ["git", "-C", root, "rev-parse", "--is-shallow-repository"],
        capture_output=True,
        text=True,
        check=False,
    )
    if shallow.stdout.strip() == "true":
        print(
            "observability-catalog: shallow clone, so `Last extracted` refs were not resolved. "
            "The CI job checks out full history."
        )
        return problems
    for block in blocks:
        if block.ref is None:
            continue
        exists = subprocess.run(
            ["git", "-C", root, "cat-file", "-e", f"{block.ref}^{{commit}}"],
            capture_output=True,
            check=False,
        )
        if exists.returncode != 0:
            problems.append(
                f"{DOC}:{block.line}: `Last extracted: {block.ref}` names no commit in this "
                "repository."
            )
    return problems


def check(root: str) -> int:
    path = os.path.join(root, DOC)
    try:
        with open(path, encoding="utf-8") as handle:
            text = handle.read()
    except (OSError, UnicodeDecodeError) as error:
        print(f"::error::observability-catalog: cannot read {DOC}: {error}")
        return 2

    blocks, problems = parse_blocks(text)
    if len(blocks) != 2:
        print(
            f"::error::observability-catalog: expected the two extracted blocks in {DOC}, "
            f"found {len(blocks)}. Either one lost its marker or a third arrived and needs "
            "naming here."
        )
        for problem in problems:
            print(f"::error::observability-catalog: {problem}")
        return 1

    for block in blocks:
        found, trouble = names_in_tree(root, block)
        problems += trouble
        problems += compare(block, found)
        print(f"observability-catalog: {block.kind} block — {len(block.names)} listed, {len(found)} in `{block.path}`.")

    problems += check_counts(text, blocks)
    problems += check_provenance(root, blocks)

    if problems:
        for problem in problems:
            print(f"::error::observability-catalog: {problem}")
        print(f"::error::observability-catalog: {len(problems)} problem(s) in {DOC}.")
        return 1
    print("OK: observability-catalog clean.")
    return 0


# --------------------------------------------------------------------------
# Self-test. Runs as a precondition of every check. Four of the six fixtures
# are shapes the gate must *reject*: a gate nobody has watched fail is a gate
# nobody knows works.
# --------------------------------------------------------------------------

FIXTURE = """# Observability

The 2 `ev` names the server emits, complete:

<!-- Extracted from the tree; do not edit by hand. Re-run and reconcile:
     grep -rhoE 'ev = "srv\\.[a-z0-9_.]+"' src | sort -u
     Last extracted: 0123abc -->

```
srv.start                        srv.stop
```

2 metric names, and one that an earlier revision listed:

<!-- Extracted from the tree; do not edit by hand. Re-run and reconcile:
     grep -rhoE '"sunrise_[a-z0-9_]+"' src | sort -u
     Last extracted: 0123abc -->

```
sunrise_a_total
sunrise_b_total          (LoggingProvider; never reached)
```
"""


def self_test() -> int:
    failures = 0

    def fail(message: str) -> None:
        nonlocal failures
        print(f"::error::observability-catalog self-test: {message}")
        failures += 1

    blocks, problems = parse_blocks(FIXTURE)
    if problems:
        fail(f"the clean fixture reported {problems}")
    if len(blocks) != 2:
        fail(f"the clean fixture yielded {len(blocks)} block(s), expected 2")
        return 1

    events, metrics = blocks
    if events.names != ("srv.start", "srv.stop"):
        fail(f"the two-column block read as {events.names}")
    if metrics.names != ("sunrise_a_total", "sunrise_b_total"):
        fail(f"the annotated block read as {metrics.names} — the aside leaked in")
    if events.path != "src" or metrics.ref != "0123abc":
        fail(f"the marker read as path={events.path!r} ref={metrics.ref!r}")

    # The comparison, in both directions.
    if compare(events, {"srv.start", "srv.stop"}) != []:
        fail("a matching set was reported as a difference")
    missing = compare(events, {"srv.start", "srv.stop", "srv.new"})
    if len(missing) != 1 or "in the tree and not in" not in missing[0]:
        fail(f"a name only in the tree read as {missing}")
    removed = compare(events, {"srv.start"})
    if len(removed) != 1 or "and not in the tree" not in removed[0]:
        fail(f"a name only in the block read as {removed}")

    # The counts, stated and misstated.
    if check_counts(FIXTURE, blocks) != []:
        fail(f"correct counts were rejected: {check_counts(FIXTURE, blocks)}")
    wrong = check_counts(FIXTURE.replace("The 2 `ev`", "The 3 `ev`"), blocks)
    if len(wrong) != 1 or "the prose says 3 event names" not in wrong[0]:
        fail(f"a wrong count read as {wrong}")
    absent = check_counts(FIXTURE.replace("The 2 `ev` names the server emits", "Every name"), blocks)
    if len(absent) != 1 or "no sentence states" not in absent[0]:
        fail(f"a missing count sentence read as {absent}")

    # A marker whose command has been deleted has to be reported, not skipped.
    broken, trouble = parse_blocks(FIXTURE.replace("grep -rhoE 'ev = \"srv\\.[a-z0-9_.]+\"' src | sort -u", "(the command)"))
    if len(broken) != 1 or not trouble or "names no `grep" not in trouble[0]:
        fail(f"a marker with no command read as {len(broken)} block(s), {trouble}")

    # A block that is never closed is a parse failure, not an empty set.
    unclosed, trouble = parse_blocks(FIXTURE[: FIXTURE.index("srv.start")])
    if not trouble or "never closed" not in trouble[-1]:
        fail(f"an unclosed block read as {unclosed}, {trouble}")

    if failures:
        return 1
    print("OK: observability-catalog self-test clean (12 cases).")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Check observability.md's extracted blocks.")
    parser.add_argument("--root", default=None, help="repository root (default: the working tree this script is in)")
    parser.add_argument("--self-test", action="store_true", help="assert the parsing and comparison rules and exit")
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
            print(f"::error::observability-catalog: not inside a git work tree: {error}")
            return 2

    failed = self_test()
    if failed:
        return failed
    return check(root)


if __name__ == "__main__":
    sys.exit(main())
