#!/usr/bin/env python3
"""Fail when the two `cargo mutants` invocations stop agreeing on their flags.

Why this gate exists
--------------------

`--all-features` is not a preference. cargo-mutants mutates the *source
file*; cargo decides whether that file is compiled. A module behind a
non-default feature is therefore mutated and then not built, so the mutant
changes nothing, the suite passes, and the mutant is recorded MISSED —
which in `outcomes.json` is the same value a mutant gets when a test
genuinely failed to kill it. The two are indistinguishable downstream, so
the gate scores the feature flag as though it were a test gap.

That is not hypothetical here. `sunrise-sync` puts its SSE + POST client
transport behind a non-default `sse` feature. With the flag off, all 94 of
`src/sse.rs`'s mutants compiled out and scored missed, and the crate's
floor read 27.17% — a number describing a build configuration and nothing
else. Turning the flag on, same tree, before a single test was added, read
36.89%. Nine points of a recorded floor were an artefact of one missing
argument. `docs/10-cross-cutting/testing.md` §Features is where that is
written up.

The fix put `--all-features` in both places that run cargo-mutants, and
left nothing keeping them in step. There are now three statements of one
rule:

* `mise.toml` — the `mutants` task, which is what a person runs locally.
* `.github/workflows/ci.yml` — the `mutants` matrix, which is what the
  nightly runs.
* `docs/10-cross-cutting/testing.md` — the prose that says the rule.

Two of them are executable and this gate checks those two. Delete the flag
from either and the other keeps passing, silently measuring a different
population from the floor it is compared against — and the only thing that
would notice is a human reading two files side by side, which is the
arrangement that produced the 27.17% in the first place.

Why a separate script rather than `grep-gate.sh`
------------------------------------------------

`grep-gate.sh` greps `--include='*.rs'`. Both invocations live in non-Rust
files, so it cannot see either one.

Why this and not "have CI call `mise run mutants`"
--------------------------------------------------

Collapsing the two invocations into one would remove the divergence rather
than check for it, which is better in principle. It is not available: mise
is installed on no Linux job in `ci.yml` — the full set of actions the
workflow uses is checkout, rust-toolchain, rust-cache, install-action,
upload-artifact and download-artifact, and mise reaches the three macOS
jobs by `brew install` — and the whole `mutants` matrix is
`if: schedule || workflow_dispatch`, so nothing about that change would
report on a pull request. This script and its contract test run on every
pull request, which is the difference between a guard and a wish.

What this gate cannot do
------------------------

It compares *arguments*, not measurements. It cannot tell whether
`--all-features` is the right set of features for either invocation, and
it cannot tell whether the two runs would produce the same population for
any other reason — a different `-p`, a different `.cargo/mutants.toml`, a
different cargo-mutants version. It checks the one argument whose absence
has already corrupted a floor here, and it checks that both places carry
it. The prose copy in `docs/10-cross-cutting/testing.md` is not checked
either: asserting a sentence is a check on the wording, not on the rule.
It is named in the failure text so whoever is changing the flags knows the
third copy exists.

Two exit codes, because they are two different pieces of news
-------------------------------------------------------------

* **1 — an invocation is missing the flag.** The two places disagree, or
  both dropped it. The remedy is to put it back, in every place.
* **2 — the gate could not run.** A file it reads is missing or
  unreadable, or one of the two files holds no `cargo mutants` invocation
  at all. That last one is the important case: if the CI matrix stops
  invoking cargo-mutants and this script keeps reporting green, it is
  reporting on nothing. A gate that cannot find what it checks must not
  read as clean — the same argument `grep-gate.sh` opens with, and the
  same two-code split `file-size-gate.py` and `mutants-gate.py` use.

Run it with `mise run mutants-flags-gate`, or directly. Its contract is
asserted by `.github/scripts/test_mutants_flags_gate.py`.
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys

REQUIRED_FLAG = "--all-features"

# Where the third, unchecked copy of the rule lives. Named in failures so
# a person fixing the flags is told about it rather than finding it later.
PROSE_COPY = "docs/10-cross-cutting/testing.md (§Features)"

# The start of an invocation. `cargo mutants`, allowing the run of spaces
# a wrapped command can pick up.
INVOCATION = re.compile(r"\bcargo\s+mutants\b")


class CannotRun(Exception):
    """The gate could not check anything.

    Distinct from a verdict, and exits 2 for the reason the module
    docstring gives: "no invocation found" and "every invocation is fine"
    are opposite findings and only one of them is good news.
    """


def logical_lines(text: str) -> list[tuple[int, str]]:
    """Join shell continuations, and drop whole-line comments.

    A `cargo mutants` invocation is spread over four lines in `ci.yml` and
    sits on one in `mise.toml`, so the flag and the command it belongs to
    are only on the same line in one of the two. Joining on a trailing
    backslash puts them together for both.

    Whole-line comments go first, and they have to: both files discuss
    `--all-features` in prose, and `ci.yml` quotes a whole
    `cargo mutants --list -p <crate> --all-features` command in a comment.
    Counting those would let the gate pass on the strength of a sentence
    about the flag while the command underneath it had lost the flag —
    precisely inverting what it is for. `#` starts a comment in TOML, in
    YAML and in the shell bodies of mise tasks alike.

    Returns (1-based line number of where the logical line starts, text).
    """
    out: list[tuple[int, str]] = []
    pending: list[str] = []
    start = 0
    for number, raw in enumerate(text.splitlines(), start=1):
        if raw.lstrip().startswith("#"):
            # A comment cannot continue a command, so anything pending
            # ends here rather than silently absorbing the comment.
            if pending:
                out.append((start, " ".join(pending)))
                pending = []
            continue
        stripped = raw.strip()
        if not pending:
            start = number
        if stripped.endswith("\\"):
            pending.append(stripped[:-1].strip())
            continue
        pending.append(stripped)
        out.append((start, " ".join(pending)))
        pending = []
    if pending:
        out.append((start, " ".join(pending)))
    return out


def invocations(path: pathlib.Path) -> list[tuple[int, str]]:
    """Every `cargo mutants` command in one file, as (line, text)."""
    try:
        text = path.read_text()
    except OSError as error:
        raise CannotRun(f"cannot read {path}: {error}") from error
    return [(number, line) for number, line in logical_lines(text)
            if INVOCATION.search(line)]


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Check that every cargo-mutants invocation carries "
                    f"{REQUIRED_FLAG}.")
    parser.add_argument(
        "--mise", type=pathlib.Path, default=pathlib.Path("mise.toml"),
        help="the mise config holding the `mutants` task")
    parser.add_argument(
        "--workflow", type=pathlib.Path,
        default=pathlib.Path(".github/workflows/ci.yml"),
        help="the workflow holding the `mutants` matrix")
    args = parser.parse_args()

    checked = 0
    offenders: list[tuple[pathlib.Path, int, str]] = []
    try:
        for path in (args.mise, args.workflow):
            found = invocations(path)
            if not found:
                raise CannotRun(
                    f"no `cargo mutants` invocation in {path}. This gate "
                    "exists to keep two invocations in step; with one of "
                    "them gone it is checking nothing, and reporting that "
                    "as a pass would be worse than reporting nothing. If "
                    "the invocation moved, point this gate at where it "
                    "went; if it is genuinely gone, delete this gate in "
                    "the same change."
                )
            for number, line in found:
                checked += 1
                if REQUIRED_FLAG not in line:
                    offenders.append((path, number, line))
    except CannotRun as error:
        print(error, file=sys.stderr)
        return 2

    if offenders:
        print(f"{REQUIRED_FLAG} is missing from "
              f"{len(offenders)} of {checked} cargo-mutants invocation(s):",
              file=sys.stderr)
        for path, number, line in offenders:
            print(f"  {path}:{number}: {line}", file=sys.stderr)
        print(
            "\nAn invocation without it mutates modules behind non-default "
            "features and then does not compile them, so those mutants are "
            "recorded MISSED and are indistinguishable from a real test "
            "gap. That has already cost this repository nine points of a "
            "recorded floor; see docs/10-cross-cutting/testing.md "
            "§Features.\n"
            f"\nAdd {REQUIRED_FLAG} back to every invocation above. There "
            f"is a third, prose copy of this rule in {PROSE_COPY} which "
            "this gate does not check and which should say the same thing.",
            file=sys.stderr)
        return 1

    print(f"OK: {checked} cargo-mutants invocation(s) carry {REQUIRED_FLAG} "
          f"({args.mise}, {args.workflow})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
