#!/usr/bin/env python3
"""Reject a pull request body whose decision record spells a closing keyword.

Pull request bodies here carry a decision record, one entry per fork:

    N. <the fork>
       Taken:    <option> - <consequence>
       Rejected: <option> - <the evidence that rejected it>
       Reverses: <the exact edit that takes the other option>

GitHub's closing-keyword parser reads the whole body and cannot tell a clause
that describes the change from one that describes *undoing* it. So a
`Reverses:` line saying what reverting would do - "close #282 as wontfix" -
closes #282 when the pull request merges. That happened twice (#300): #240
closed #282 three seconds after it merged, and #293 closed #297 two seconds
after it merged. Both issues were wanted open.

This gate reads the body and fails when a decision-record field (`Taken`,
`Rejected`, `Reverses`, with a `:` or a `.` after the label, in any emphasis)
holds a closing keyword followed by an issue reference. An intended close
belongs on its own `Closes #N` line, which is not a decision-record field and
which this gate never reads.

What it treats as safe, and what it does not
--------------------------------------------

Safe, because GitHub does not read it as a closing reference:

- Anything inside a fenced code block (``` or ~~~). #264 fenced its whole
  decision record and none of its `Reverses:` lines closed anything.
- An issue written as a markdown link, `[#297](https://...)`: the keyword is
  followed by `[`, not by a reference.
- The keyword and the number apart, as in "close it (#297)".

Not safe, and read:

- An inline code span. #293's line was backticked end to end and still
  closed #297.
- Indented lines. A decision record's fields sit three spaces into a list
  item, which is list continuation rather than an indented code block.
- A field that wraps: the lines after a label, up to a blank line or the next
  label, list item, heading or fence, are read as one field, so a keyword at
  the end of one line and the number at the start of the next is caught.
- A label with no content of its own, whose content is the list under it
  (`Reverses:` then `- close #282`). The list is read as the field, through
  blank lines between its items, until a heading, rule or fence, the next
  label, an unindented paragraph after a blank line, or a list item indented
  less than the label (the next entry of an enclosing list).
- A label with no content of its own whose content is a paragraph: on the
  next line, or after a blank line when the paragraph is indented, at least
  as far as the label (`1. Fork` / `   Reverses:` / blank / `   close #5`).
  An unindented paragraph after a blank line is not the label's field.

Where GitHub's own parser is not documented, the gate errs towards failing:
emphasis or backticks between the keyword and the number, a `:` after the
keyword, `owner/repo#N`, a full issue URL, and a hyphenated word such as
`re-close #N` all count. A false alarm costs a rewrite; a miss costs an issue.

Where the body comes from
-------------------------

With a path argument, that file (`-` for stdin), so a body can be checked
before it is published:

    gh pr view N --json body --jq .body | mise run reverses-gate -

With no argument, the `pull_request.body` of the event payload at
`$GITHUB_EVENT_PATH`, which is how the `pr-body` workflow runs it. The body is
never interpolated into a shell command.

Exit 0 when no field holds a closing reference, 1 when one does, 2 when the
gate cannot run. All three are asserted in `test_reverses_gate.py` beside this
file, run by the `reverses-gate-contract` job in ci.yml.
"""

from __future__ import annotations

import dataclasses
import json
import os
import pathlib
import re
import sys

# The decision-record labels. The label may be list-marked, quoted, wrapped in
# emphasis or backticks, and followed by `:` or `.` (#240 wrote
# "**Reverses.**"), with the closing punctuation inside or outside the emphasis.
FIELD = re.compile(
    r"""^\s*
        (?:>\s*)*                     # blockquote markers
        (?:(?:[-*+]|\d+[.)])\s+)?     # a list marker
        [*_`]*\s*
        (?P<label>taken|rejected|reverses)
        \s*[*_`]*\s*[:.]
    """,
    re.IGNORECASE | re.VERBOSE,
)

# What ends a wrapped field: a list item, a heading, a horizontal rule. A label
# with no content of its own is the exception for list items: its content is
# the list under it, read as the field (see `violations`).
LIST_ITEM = re.compile(r"^(?P<indent>\s*)(?:>\s*)*(?:[-*+]|\d+[.)])\s")
BOUNDARY = re.compile(r"^\s*(?:>\s*)*(?:[-*+]\s|\d+[.)]\s|#{1,6}\s|[-*_]{3,}\s*$)")

# What may follow a label that has no content of its own: the label's own
# closing emphasis or backticks, and whitespace.
BARE_LABEL_REST = re.compile(r"^[\s*_`]*$")

# GitHub's nine closing keywords, then the reference. Emphasis, backticks and a
# colon between the two are tolerated because GitHub's handling of them is not
# documented; see the module docstring.
CLOSING = re.compile(
    r"""\b(?P<keyword>close[sd]?|fix(?:e[sd])?|resolve[sd]?)\b
        [\s*_`:]*
        (?P<ref>
            (?:[\w.-]+/[\w.-]+)?\#\d+
          | https?://github\.com/[\w.-]+/[\w.-]+/(?:issues|pull)/\d+
        )
    """,
    re.IGNORECASE | re.VERBOSE,
)

FENCE = re.compile(r"^\s*(?:>\s*)*(?P<fence>`{3,}|~{3,})(?P<info>.*)$")


def unfenced_lines(body: str) -> list[tuple[int, str | None]]:
    """Each line with its 1-based number, and `None` in place of fenced text.

    A fence closes on a run of the same character at least as long as the one
    that opened it, with nothing after it. An unclosed fence runs to the end
    of the body, as CommonMark (and GitHub) render it.
    """
    out: list[tuple[int, str | None]] = []
    open_fence: str | None = None
    for number, line in enumerate(body.splitlines(), start=1):
        match = FENCE.match(line)
        if open_fence is None:
            if match and not (match["fence"][0] == "`" and "`" in match["info"]):
                open_fence = match["fence"]
                out.append((number, None))
            else:
                out.append((number, line))
            continue
        if (
            match
            and match["fence"][0] == open_fence[0]
            and len(match["fence"]) >= len(open_fence)
            and not match["info"].strip()
        ):
            open_fence = None
        out.append((number, None))
    return out


@dataclasses.dataclass
class Field:
    """A decision-record field being read: its label line and what follows."""

    start: int
    label: str
    parts: list[str]
    # The label line's leading whitespace, against which a list item is judged
    # to sit under the label or in an enclosing list.
    indent: int
    # The label has no content of its own yet, so a list may follow it.
    bare: bool
    # The field is the list under a bare label.
    in_list: bool = False
    # A blank line was seen inside the field.
    gap: bool = False


def violations(body: str) -> list[tuple[int, str, str, str]]:
    """Every closing reference inside a decision-record field.

    Returns (line of the label, label, keyword, reference) per finding.
    """
    found: list[tuple[int, str, str, str]] = []
    field: Field | None = None

    def flush() -> None:
        if field is None:
            return
        for match in CLOSING.finditer(" ".join(field.parts)):
            found.append((field.start, field.label, match["keyword"], match["ref"]))

    for number, line in unfenced_lines(body):
        if line is None:
            flush()
            field = None
            continue
        if not line.strip():
            # A blank line ends a field, unless the field is a bare label or
            # the list under one: a list may start after a blank line, and a
            # loose list has blank lines between its items.
            if field is not None and (field.bare or field.in_list):
                field.gap = True
                continue
            flush()
            field = None
            continue
        label = FIELD.match(line)
        if label:
            flush()
            field = Field(
                start=number,
                label=label["label"].capitalize(),
                parts=[line],
                indent=len(line) - len(line.lstrip()),
                bare=bool(BARE_LABEL_REST.match(line[label.end() :])),
            )
            continue
        if field is None:
            continue
        if field.bare or field.in_list:
            item = LIST_ITEM.match(line)
            indent = len(line) - len(line.lstrip())
            if item and len(item["indent"]) >= field.indent:
                # An item of the list under the label. An item indented less
                # than the label belongs to an enclosing list, such as the
                # next entry of the decision record, and ends the field.
                field.bare, field.in_list, field.gap = False, True, False
                field.parts.append(line)
                continue
            if not item and not BOUNDARY.match(line):
                if field.in_list and (not field.gap or indent > field.indent):
                    # An item's continuation line, or a paragraph indented
                    # into the item after a blank line.
                    field.gap = False
                    field.parts.append(line)
                    continue
                if field.bare and (not field.gap or indent >= max(field.indent, 1)):
                    # The label's content wraps onto the next line, or is a
                    # paragraph indented under it after a blank line (inside
                    # the label's list item, where GitHub still reads it).
                    field.bare = False
                    field.parts.append(line)
                    continue
            flush()
            field = None
            continue
        if BOUNDARY.match(line):
            flush()
            field = None
        else:
            field.parts.append(line)
    flush()
    return found


def read_body(argv: list[str]) -> str:
    """The body to check, from a path argument or the event payload."""
    if len(argv) > 1:
        print("::error::reverses-gate: takes at most one argument, a body file or `-`.")
        raise SystemExit(2)
    if argv:
        try:
            if argv[0] == "-":
                return sys.stdin.read()
            return pathlib.Path(argv[0]).read_text(encoding="utf-8")
        except OSError as error:
            print(f"::error::reverses-gate: cannot read {argv[0]}: {error}")
            raise SystemExit(2) from error

    event_path = os.environ.get("GITHUB_EVENT_PATH")
    if not event_path:
        print(
            "::error::reverses-gate: no body file given and GITHUB_EVENT_PATH is "
            "unset, so there is no body to read."
        )
        raise SystemExit(2)
    try:
        event = json.loads(pathlib.Path(event_path).read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        print(f"::error::reverses-gate: cannot read the event payload: {error}")
        raise SystemExit(2) from error
    pull_request = event.get("pull_request") if isinstance(event, dict) else None
    if not isinstance(pull_request, dict):
        print(
            "::error::reverses-gate: the event payload has no `pull_request`; "
            "this gate runs on pull_request events only."
        )
        raise SystemExit(2)
    # An empty body arrives as null.
    body = pull_request.get("body")
    return body if isinstance(body, str) else ""


def main(argv: list[str]) -> int:
    body = read_body(argv)
    found = violations(body)
    for line, label, keyword, ref in found:
        print(
            f"::error::reverses-gate: body line {line}: the `{label}` field says "
            f"`{keyword} {ref}`, and GitHub closes {ref} when this pull request "
            "merges, whatever the sentence around it means. Write the issue as a "
            "link - [#N](https://github.com/OWNER/REPO/issues/N) - or move the "
            "number away from the keyword, or fence the decision record in a "
            "``` block. An intended close goes on its own `Closes #N` line."
        )
    if found:
        print(f"reverses-gate: {len(found)} closing reference(s) in the decision record.")
        return 1
    print("OK: reverses-gate clean - no decision-record field spells a closing reference.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
