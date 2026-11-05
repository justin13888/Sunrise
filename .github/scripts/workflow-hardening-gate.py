#!/usr/bin/env python3
"""Reject a workflow that trusts a ref it does not control, or a token it never scoped.

Three assertions, all about the same thing: what a CI run is allowed to be,
and what it is allowed to block.

## 1. Every action is pinned to a commit

A `uses:` ref that names a tag or a branch is a promise by somebody else that
the code behind that name will not change. `ci.yml` said so itself for as long
as it had a `changes` job — that job declined a third-party action and named
CVE-2025-30066 while doing it, in which every tag of `tj-actions/changed-files`
was retargeted in place to dump runner memory into build logs. Floating major
tags are exactly the configuration that let that reach the repositories it
reached, and this repository had ninety-six of them.

A forty-character commit sha is not a promise, it is an identity. It is also
not an upgrade policy: `.github/dependabot.yml` is what moves the pins, weekly
and grouped, so a pin that is stale is a pull request rather than a silence.

A pin also carries a trailing `# <version>` comment. Forty hex characters
say nothing to a reviewer about what they are approving, and dependabot
rewrites the comment along with the sha on every bump. The gate requires the
comment to be there; it cannot check that it is true, which would need the
network, and does not pretend to.

Local refs (`./.github/actions/...`) are exempt because they are this
repository's own tree, already covered by whatever gates the tree. `docker://`
refs are exempt because a sha is not what pins them.

## 2. Every workflow declares a top-level `permissions:`

Without one, the scope of `GITHUB_TOKEN` comes from a repository settings page
— changeable with no commit, no diff and no review, and invisible to anyone
reading the workflow. Declaring it in the file is not a stronger grant, it is a
*legible* one, and the diff becomes the record.

Composite actions under `.github/actions/` are exempt: `permissions:` is not a
key they may carry. They are still read for assertion 1, because a composite
action pulls actions of its own and is the easiest place for an unpinned ref to
hide from a reviewer looking only at `workflows/`.

## 3. Every required context is a job that reports on every pull request

`.github/rulesets/*.json` records the repository rulesets, in the shape
`gh api repos/OWNER/REPO/rulesets/ID` returns (see `.github/rulesets/README.md`).
A context in a `required_status_checks` rule is matched against job names by
exact string, so a renamed job leaves the ruleset waiting for a check nothing
will ever report, and every pull request blocks behind it. The same holds for a
job whose workflow has no `pull_request` trigger, or one under `paths:`,
`paths-ignore:`, `branches:` or `branches-ignore:`, or under a `types:` list
that omits any of the default `opened`, `synchronize` and `reopened`: a pull
request it skips, or a head commit it never runs on, gets no check run, not a
skipped one. A branch filter fails whatever it names, because a pull request
stacked on another one's branch targets that branch, not the default one.
A job-level `if:` is fine, because a skipped job reports `skipped`, which GitHub
counts as passing.

Each context must equal some workflow job's `name:` (its id where it has none),
in a workflow that runs on every pull request. A name built from
`${{ matrix.* }}` is not expanded, so a matrix leg cannot be required until the
gate learns to expand one. The gate reads the record, not the live ruleset:
whether the two agree is the README's procedure, not something an offline
check can see.

## Parsing

Line-oriented, standard library only. No workflow YAML parser ships with
CPython, the runners install nothing extra, and every other gate in this
directory is stdlib-only for the same reason. Block scalars (`run: |`) are
tracked and skipped so a shell line inside one cannot be read as a `uses:`,
and comments are stripped before a line is read.

Exit 0 when every ref is pinned, every workflow is scoped and every required
context reports on every pull request, 1 when one is not, 2 when the gate
cannot run.

All three are asserted in `test_workflow_hardening_gate.py` beside this file,
run by the `workflow-hardening-gate-contract` job in ci.yml. The gate takes an
optional root so those cases can point it at a synthesised tree rather than at
this repository.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys

CLEAN = 0
VIOLATION = 1
CANNOT_RUN = 2

# `uses` as a mapping key, bare or quoted. GitHub accepts all three spellings.
KEY = r"""(?:uses|"uses"|'uses')"""
USES = re.compile(rf"^(?P<indent>\s*)(?P<dash>-\s+)?{KEY}\s*:(?P<ref>.*)$")
# The same key inside a flow mapping: `- {uses: x}`, `- {name: y, uses: x}`.
FLOW_USES = re.compile(rf"{KEY}\s*:\s*(?P<ref>[^,}}\]\s]+)")
# A block scalar header: `run: |`, `script: >-`, and so on.
BLOCK_SCALAR = re.compile(
    r"^(?P<indent>\s*)(?P<dash>-\s+)?(?:[\w.-]+|\"[^\"]+\"|'[^']+')\s*:\s*[|>][-+0-9]*\s*$"
)
# A key at column zero is a top-level key in a workflow document.
TOP_LEVEL_PERMISSIONS = re.compile(r"^permissions:\s*(?P<inline>\S.*)?$")
PINNED = re.compile(r"^[^/\s]+/[^@\s]+@[0-9a-f]{40}$")
EXEMPT_PREFIXES = ("./", "../", "docker://")


def strip_comment(line: str) -> str:
    """Drop a trailing `# ...`, leaving a `#` that sits inside quotes alone."""
    out: list[str] = []
    quote: str | None = None
    for index, char in enumerate(line):
        if quote:
            out.append(char)
            if char == quote:
                quote = None
            continue
        if char in "'\"":
            quote = char
            out.append(char)
            continue
        if char == "#" and (index == 0 or line[index - 1] in " \t"):
            break
        out.append(char)
    return "".join(out)


def uses_refs(text: str) -> list[tuple[int, str | None, str]]:
    """Every `uses:` in a document, as (line, ref, raw line).

    A `ref` of `None` means the gate found the key and could not read its
    value. That is reported as a violation rather than skipped: the shapes it
    covers — a ref written as a block scalar, or a key whose value is missing
    — are all legal YAML that Actions accepts, so treating "unreadable" as
    "absent" would be a hole exactly where someone hiding a ref would put one.
    """
    found: list[tuple[int, str | None, str]] = []
    lines = text.splitlines()
    scalar_indent: int | None = None
    index = 0
    while index < len(lines):
        raw = lines[index]
        stripped = raw.strip()
        if scalar_indent is not None:
            # A blank line does not end a block scalar; a dedent does.
            if not stripped or len(raw) - len(raw.lstrip()) > scalar_indent:
                index += 1
                continue
            scalar_indent = None
        if not stripped or stripped.startswith("#"):
            index += 1
            continue

        line = strip_comment(raw)
        if not line.strip():
            index += 1
            continue

        # Flow mapping: `- {uses: x}` and `- {name: y, uses: x}`.
        if "{" in line and "uses" in line:
            for match in FLOW_USES.finditer(line[line.index("{") :]):
                found.append((index + 1, match.group("ref").strip("'\""), raw))
            index += 1
            continue

        match = USES.match(line)
        if match:
            ref = match.group("ref").strip()
            dash = match.group("dash") or ""
            key_column = len(match.group("indent")) + len(dash)
            if ref.startswith(("|", ">")):
                # The mechanism that makes `run: |` safe to skip is the one
                # that would hide a ref written the same way. Fail closed.
                found.append((index + 1, None, raw))
                scalar_indent = key_column
                index += 1
                continue
            if not ref:
                # `uses:` with the value on a following, more-indented line.
                ahead = index + 1
                while ahead < len(lines) and (
                    not lines[ahead].strip() or lines[ahead].lstrip().startswith("#")
                ):
                    ahead += 1
                if ahead < len(lines) and len(lines[ahead]) - len(
                    lines[ahead].lstrip()
                ) > key_column:
                    value = strip_comment(lines[ahead]).strip().strip("'\"")
                    found.append((ahead + 1, value or None, lines[ahead]))
                    index = ahead + 1
                    continue
                found.append((index + 1, None, raw))
                index += 1
                continue
            found.append((index + 1, ref.strip("'\""), raw))
            index += 1
            continue

        block = BLOCK_SCALAR.match(line)
        if block:
            # The key's column, not the dash's: for `- run: |` the body and the
            # step's sibling keys both sit to the right of the dash, and only
            # the body sits to the right of `run`.
            scalar_indent = len(block.group("indent")) + len(block.group("dash") or "")
        index += 1
    return found


def has_top_level_permissions(text: str) -> bool:
    for raw in text.splitlines():
        if raw.startswith("#"):
            continue
        if TOP_LEVEL_PERMISSIONS.match(strip_comment(raw)):
            return True
    return False


def _indent(raw: str) -> int:
    return len(raw) - len(raw.lstrip())


def _code_lines(text: str) -> list[tuple[int, str]]:
    """(indent, content) for every line that is not blank or a comment."""
    out: list[tuple[int, str]] = []
    for raw in text.splitlines():
        line = strip_comment(raw).rstrip()
        if line.strip():
            out.append((_indent(line), line.strip()))
    return out


def _unquote(value: str) -> str:
    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in "'\"":
        return value[1:-1]
    return value


def job_names(text: str) -> list[str]:
    """The check-run name of every job in a workflow: its `name:`, else its id.

    A job's keys are found by indentation alone: the first line under `jobs:`
    fixes the column job ids sit at, and the first line under a job fixes the
    column its own keys sit at. A `name:` deeper than that belongs to a step.
    """
    lines = _code_lines(text)
    names: list[str] = []
    start = next((i for i, (ind, c) in enumerate(lines) if ind == 0 and c == "jobs:"), None)
    if start is None or start + 1 >= len(lines) or lines[start + 1][0] == 0:
        return names
    job_indent = lines[start + 1][0]
    index = start + 1
    while index < len(lines) and lines[index][0] >= job_indent:
        indent, content = lines[index]
        if indent == job_indent and content.endswith(":"):
            job_id = _unquote(content[:-1])
            name = job_id
            index += 1
            key_indent = lines[index][0] if index < len(lines) else 0
            while index < len(lines) and lines[index][0] > job_indent:
                indent, content = lines[index]
                if indent == key_indent and content.startswith("name:"):
                    name = _unquote(content[len("name:") :])
                index += 1
            names.append(name)
            continue
        index += 1
    return names


def pull_request_trigger(text: str) -> str:
    """How a workflow's `on:` reaches pull requests.

    `"none"` when it does not run on `pull_request` at all, `"filtered"` when it
    does under a filter some pull request falls outside, and `"every"`
    otherwise. A filter is any of `paths:`, `paths-ignore:`, `branches:` and
    `branches-ignore:` (whatever it names: the base branch a stacked pull
    request targets is not the default one), and a `types:` list that omits any
    of the default `opened`, `synchronize` and `reopened`, each of which is a
    head commit some pull request would otherwise get no check run on.
    """
    lines = _code_lines(text)
    for index, (indent, content) in enumerate(lines):
        if indent != 0 or not re.match(r"""^(?:on|"on"|'on')\s*:""", content):
            continue
        inline = content.split(":", 1)[1].strip()
        if inline:
            events = [_unquote(e) for e in inline.strip("[]").split(",")]
            return "every" if "pull_request" in events else "none"
        if index + 1 >= len(lines) or lines[index + 1][0] == 0:
            return "none"
        event_indent = lines[index + 1][0]
        cursor = index + 1
        while cursor < len(lines) and lines[cursor][0] >= event_indent:
            indent, content = lines[cursor]
            event, _, rest = content.lstrip("- ").partition(":")
            cursor += 1
            if indent != event_indent or _unquote(event) != "pull_request":
                continue
            rest = rest.strip()
            if rest:
                # A flow mapping, `pull_request: {types: [opened]}`.
                if re.search(r"\b(?:paths|branches)(?:-ignore)?\b", rest):
                    return "filtered"
                types = re.search(r"\btypes\s*:\s*(\[[^\]]*\]|[^,}]+)", rest)
                if types and not _reaches_every_head(_flow_items(types.group(1))):
                    return "filtered"
                return "every"
            filters: dict[str, list[str]] = {}
            key_indent = lines[cursor][0] if cursor < len(lines) else 0
            key = ""
            while cursor < len(lines) and lines[cursor][0] > event_indent:
                indent, content = lines[cursor]
                cursor += 1
                if indent == key_indent and not content.startswith("-"):
                    key, _, value = content.partition(":")
                    key = _unquote(key)
                    filters[key] = _flow_items(value) if value.strip() else []
                elif key:
                    filters[key].append(_unquote(content.lstrip("- ")))
            if filters.keys() & {"paths", "paths-ignore", "branches", "branches-ignore"}:
                return "filtered"
            if "types" in filters and not _reaches_every_head(filters["types"]):
                return "filtered"
            return "every"
        return "none"
    return "none"


# The activity types `pull_request` runs on when it lists none. Each one moves
# or restores the head a required check is read at.
DEFAULT_PULL_REQUEST_TYPES = frozenset({"opened", "synchronize", "reopened"})


def _flow_items(value: str) -> list[str]:
    """The items of `[a, b]` or of a bare scalar `a`."""
    return [_unquote(item) for item in value.strip().strip("[]").split(",") if item.strip()]


def _reaches_every_head(types: list[str]) -> bool:
    return DEFAULT_PULL_REQUEST_TYPES <= set(types)


def required_contexts(path: pathlib.Path) -> list[str] | None:
    """Every `required_status_checks` context a ruleset record names.

    `None` when the file is not a ruleset this gate can read.
    """
    try:
        ruleset = json.loads(path.read_text(encoding="utf-8"))
        contexts: list[str] = []
        for rule in ruleset["rules"]:
            if rule["type"] != "required_status_checks":
                continue
            for check in rule["parameters"]["required_status_checks"]:
                if not isinstance(check["context"], str):
                    return None
                contexts.append(check["context"])
        return contexts
    except (ValueError, KeyError, TypeError):
        return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "root",
        nargs="?",
        default=".",
        type=pathlib.Path,
        help="Repository root to inspect (default: the working directory).",
    )
    args = parser.parse_args()

    workflow_dir = args.root / ".github" / "workflows"
    if not workflow_dir.is_dir():
        print(
            f"::error::workflow-hardening: no `{workflow_dir}` to read. The gate is "
            "pointed at something that is not a repository root, and a gate that "
            "cannot look must not report that it found nothing."
        )
        return CANNOT_RUN

    workflows = sorted(
        path
        for path in workflow_dir.iterdir()
        if path.suffix in {".yml", ".yaml"} and path.is_file()
    )
    if not workflows:
        print(
            f"::error::workflow-hardening: `{workflow_dir}` holds no workflow. "
            "Either every workflow was deleted or the gate is looking in the "
            "wrong place; both are red."
        )
        return CANNOT_RUN

    # `rglob`, and both spellings: a composite action may sit at any depth
    # under `.github/actions/` (`uses: ./.github/actions/a/b` is legal) and
    # GitHub accepts `action.yaml` as readily as `action.yml`. A one-level
    # `*/action.yml` glob would leave both layouts unread — and the reason to
    # read composites at all is that they are where an unpinned ref hides from
    # a reviewer who opened `workflows/` and stopped.
    actions_dir = args.root / ".github" / "actions"
    composites = (
        sorted(set(actions_dir.rglob("action.yml")) | set(actions_dir.rglob("action.yaml")))
        if actions_dir.is_dir()
        else []
    )

    failures = 0
    checked = 0
    for path in workflows + composites:
        text = path.read_text(encoding="utf-8")
        shown = path.relative_to(args.root) if path.is_relative_to(args.root) else path
        for number, ref, raw in uses_refs(text):
            if ref is None:
                failures += 1
                print(
                    f"::error file={shown},line={number}::workflow-hardening: a "
                    "`uses:` whose value this gate cannot read. A ref written as a "
                    "block scalar, or a key with no value, is legal YAML that "
                    "Actions accepts and that a reviewer skims past. Write it on "
                    "one line as `owner/repo@<40-char sha>  # <version>`."
                )
                continue
            if ref.startswith(EXEMPT_PREFIXES):
                continue
            checked += 1
            if not PINNED.match(ref):
                failures += 1
                print(
                    f"::error file={shown},line={number}::workflow-hardening: "
                    f"`{ref}` is not pinned to a commit. A tag or a branch is a ref "
                    "its owner can move after review — which is what CVE-2025-30066 "
                    "was. Pin it as `owner/repo@<40-char sha>  # <version>`; "
                    "dependabot updates it from there."
                )
                continue
            if "#" not in raw:
                failures += 1
                print(
                    f"::error file={shown},line={number}::workflow-hardening: "
                    f"`{ref.split('@')[0]}` is pinned but unlabelled. Forty hex "
                    "characters tell a reviewer nothing about what they are "
                    "approving. Add the version it resolves to as a trailing "
                    "comment — `# v4.4.0` — which is what dependabot rewrites."
                )

    for path in workflows:
        shown = path.relative_to(args.root) if path.is_relative_to(args.root) else path
        if not has_top_level_permissions(path.read_text(encoding="utf-8")):
            failures += 1
            print(
                f"::error file={shown},line=1::workflow-hardening: no top-level "
                "`permissions:` block. Without one the token's scope comes from a "
                "settings page that changes with no commit and no diff. Declare the "
                "scope here — `contents: read` if the workflow only checks out."
            )

    # Assertion 3: every required context in a ruleset record is a job that
    # reports on every pull request.
    reporters: dict[str, list[str]] = {}
    for path in workflows:
        text = path.read_text(encoding="utf-8")
        trigger = pull_request_trigger(text)
        for name in job_names(text):
            reporters.setdefault(name, []).append(trigger)

    rulesets_dir = args.root / ".github" / "rulesets"
    records = sorted(rulesets_dir.glob("*.json")) if rulesets_dir.is_dir() else []
    required = 0
    for path in records:
        shown = path.relative_to(args.root) if path.is_relative_to(args.root) else path
        contexts = required_contexts(path)
        if contexts is None:
            failures += 1
            print(
                f"::error file={shown},line=1::workflow-hardening: not a ruleset "
                "this gate can read. It needs a `rules` list whose "
                "`required_status_checks` rule lists `{\"context\": \"<job name>\"}` "
                "objects, which is the shape `gh api repos/OWNER/REPO/rulesets/ID` "
                "returns."
            )
            continue
        for context in contexts:
            required += 1
            triggers = reporters.get(context)
            if not triggers:
                failures += 1
                print(
                    f"::error file={shown},line=1::workflow-hardening: required "
                    f"context `{context}` is no job's name in `.github/workflows/`. "
                    "A required context nothing reports waits forever and blocks "
                    "every pull request. Correct it to the job's `name:` byte for "
                    "byte; a name built from `${{ matrix.* }}` cannot be required "
                    "until this gate expands matrices."
                )
            elif "every" not in triggers:
                failures += 1
                print(
                    f"::error file={shown},line=1::workflow-hardening: required "
                    f"context `{context}` is a job whose workflow does not run on "
                    "every pull request (no `pull_request` trigger, or one under "
                    "`paths:`, `branches:`, or a `types:` list missing `opened`, "
                    "`synchronize` or `reopened`). A pull request it skips never "
                    "gets the check, and never merges."
                )

    if failures:
        print(f"workflow-hardening: {failures} violation(s) across {len(workflows) + len(composites) + len(records)} file(s).")
        return VIOLATION

    print(
        f"OK: workflow-hardening clean — {checked} action ref(s) pinned to a commit "
        f"across {len(workflows) + len(composites)} file(s), each of the "
        f"{len(workflows)} workflow(s) declares its own token scope, and each of the "
        f"{required} required context(s) in {len(records)} ruleset record(s) is a "
        "job that reports on every pull request."
    )
    return CLEAN


if __name__ == "__main__":
    sys.exit(main())
