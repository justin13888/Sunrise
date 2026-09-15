#!/usr/bin/env python3
"""Print the assertions behind a failed `xcodebuild test`, from its result bundle.

Why this exists
---------------

`mise run ios-app` and `mise run macos-app` pass `-quiet`, and `-quiet` on a
failing test run prints this and nothing else::

    Failing tests:
            LibraryReachUITests.testASavedViewRoundTripsThroughTheMenu()
    ** TEST FAILED **

No assertion message, no file, no line. That is the whole log tail of the CI job
that caught the flake this script was written for: the name of the test was the
only evidence, and the name of the test is the one thing already in the diff.

Dropping `-quiet` is not the fix. The iOS job compiles two apps, a Rust
xcframework and five test bundles, and its unquiet log is tens of thousands of
lines of compiler invocations with the twelve lines that matter somewhere in the
middle. What is wanted is the twelve lines, which is what a result bundle holds
and what this prints.

What it prints
--------------

Every failed test case, and under each one the `Failure Message` and
`Source Code Reference` nodes XCTest recorded against it — which is
`file:line: message`, the text an engineer would have got from Xcode's test
navigator. A run with fifty passing tests and one failure prints one stanza.

Exit codes, because they are three different pieces of news
-----------------------------------------------------------

* **0 — failures found and printed.** The caller has what it needs.
* **1 — the bundle is readable and holds no failed test case.** The run went red
  for something that is not a test assertion: a build error, a runner that could
  not launch, a timeout. The log itself is the place to look, and saying so is
  more useful than printing nothing.
* **2 — the bundle could not be read.** `xcresulttool` is missing, the path is
  wrong, or the schema moved. Treated as its own outcome rather than folded into
  "no failures", which is how a diagnostic rots into a silent no-op.

The caller still decides the build's fate. This script never makes a red run
green: the tasks that call it exit with `xcodebuild`'s own status.

Schema
------

`xcrun xcresulttool get test-results tests` (Xcode 16 and later; verified on
Xcode 26.6, xcresulttool 24757). The node types are its own vocabulary --
`xcrun xcresulttool get test-results tests --schema` prints them.
"""

from __future__ import annotations

import json
import subprocess
import sys

# `get test-results tests` returns a tree of `TestNode`s. These are the three
# node types this reads; the rest describe devices, configurations and timings.
CASE = "Test Case"
MESSAGE = "Failure Message"
REFERENCE = "Source Code Reference"


def load(bundle: str) -> dict:
    """The test tree, or exit 2 saying why not."""
    try:
        completed = subprocess.run(
            [
                "xcrun",
                "xcresulttool",
                "get",
                "test-results",
                "tests",
                "--path",
                bundle,
                "--compact",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError as error:  # xcrun itself absent
        sys.stderr.write(f"xcresult-failures: cannot run xcresulttool: {error}\n")
        raise SystemExit(2) from error
    if completed.returncode != 0:
        sys.stderr.write(
            f"xcresult-failures: xcresulttool exited {completed.returncode} "
            f"for {bundle}:\n{completed.stderr.strip()}\n"
        )
        raise SystemExit(2)
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        sys.stderr.write(f"xcresult-failures: unreadable JSON from {bundle}: {error}\n")
        raise SystemExit(2) from error


def failed_cases(node: dict) -> list[dict]:
    """Every failed `Test Case` in the tree, depth first.

    A case is the unit an engineer recognises. Its parents (the bundle, the
    suite, the plan) are failed too, and reporting those would say the same
    thing three times in decreasing detail.
    """
    found = []
    if node.get("nodeType") == CASE and node.get("result") == "Failed":
        found.append(node)
    for child in node.get("children", []):
        found.extend(failed_cases(child))
    return found


def detail(node: dict) -> list[str]:
    """The assertion text recorded under one case, in the order XCTest wrote it."""
    lines = []
    for child in node.get("children", []):
        if child.get("nodeType") in (MESSAGE, REFERENCE):
            lines.append(child.get("name", "").strip())
        lines.extend(detail(child))
    return [line for line in lines if line]


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        sys.stderr.write("usage: xcresult-failures.py <path/to/bundle.xcresult>\n")
        return 2
    bundle = argv[1]
    report = load(bundle)

    cases = []
    for root in report.get("testNodes", []):
        cases.extend(failed_cases(root))
    if not cases:
        sys.stderr.write(
            f"xcresult-failures: {bundle} records no failed test case. The run went "
            "red for something that is not an assertion — a build error, a runner "
            "that could not launch, or a timeout. Read the log above.\n"
        )
        return 1

    for case in cases:
        print(f"✗ {case.get('nodeIdentifier') or case.get('name', '<unnamed>')}")
        for line in detail(case) or ["(no assertion text recorded)"]:
            print(f"    {line}")
        print()
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
