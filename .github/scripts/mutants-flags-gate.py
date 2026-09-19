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

Two of them are executable today and this gate checks every place one can
be. Delete the flag from either and the other keeps passing, silently
measuring a different population from the floor it is compared against —
and the only thing that would notice is a human reading two files side by
side, which is the arrangement that produced the 27.17% in the first
place.

What the gate reads is `mise.toml` plus every file under
`.github/workflows/`, not two hard-coded names. Naming two files made a
third executable copy — a matrix moved into a workflow of its own, a
release job that measures something — invisible, while the gate went on
reporting OK about the two it knew about.

What it counts is an *invocation*, not a line. Shell puts several
commands on one line, and a containment test over the joined line is
satisfied by any of them: `cargo mutants --list … --all-features &&
cargo mutants -p X --jobs 1` has a flag in it and no flag on the
invocation that measures the floor. So each logical line is split into
commands on `&&`, `||`, `;` and `|`, any `#`-to-end-of-line remainder is
dropped, and the flag is looked for in the *tokens* of each command whose
first two are `cargo mutants`. A comment about the flag, a neighbouring
`echo` about the flag, and a `--list` call carrying the flag all stop
vouching for the command beside them.

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
  unreadable, or *no* file it read holds a `cargo mutants` invocation at
  all. That last one is judged over the union and not per file, because
  moving the matrix from one workflow to another leaves a tree entirely
  in step and would otherwise be reported as a broken gate. It is the
  important case: if cargo-mutants stops being invoked anywhere and this
  script keeps reporting green, it is reporting on nothing. A gate that
  cannot find what it checks must not read as clean — the same argument
  `grep-gate.sh` opens with, and the same two-code split
  `file-size-gate.py` and `mutants-gate.py` use.

Run it with `mise run mutants-flags-gate`, or directly. Its contract is
asserted by `.github/scripts/test_mutants_flags_gate.py`.
"""

from __future__ import annotations

import argparse
import pathlib
import re
import shlex
import sys

REQUIRED_FLAG = "--all-features"

# Where the third, unchecked copy of the rule lives. Named in failures so
# a person fixing the flags is told about it rather than finding it later.
PROSE_COPY = "docs/10-cross-cutting/testing.md (§Features)"

# The start of an invocation. `cargo mutants`, allowing the run of spaces
# a wrapped command can pick up. Used to find *candidate* text only; what
# decides that a command is an invocation is its first two tokens.
INVOCATION = re.compile(r"\bcargo\s+mutants\b")

# The file that holds the local `mutants` task. Named literally rather
# than globbed, so deleting it is a read error and an exit 2 rather than
# a file that quietly stops being checked.
DEFAULT_MISE = pathlib.Path("mise.toml")

# Every workflow, not the one that happens to hold the matrix today. A
# second executable copy in a workflow this gate did not name would be
# invisible to it while the gate went on reporting OK, which is the
# hazard the gate exists for.
DEFAULT_WORKFLOW_DIR = pathlib.Path(".github/workflows")
WORKFLOW_GLOBS = ("*.yml", "*.yaml")

# Shell separators between one command and the next. `&&` and `||` are
# matched before `|` so that `||` is one boundary rather than two empty
# commands.
SEPARATORS = ("&&", "||", ";", "|")


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


def commands(line: str) -> list[str]:
    """Split one logical line into the commands it actually runs.

    The unit this gate checks has to be an *invocation*, not a line. A
    logical line is a piece of shell, and shell puts several commands on
    one: `cargo mutants --list ... --all-features > population.txt &&
    cargo mutants -p X --jobs 1` is one line, two invocations, and only
    one of them measures anything. Counting that as a single unit and
    asking whether the flag appears anywhere in it reports the whole line
    green on the strength of the `--list` call, while the invocation that
    produces the floor has no flag at all — the exact 27.17%-vs-36.89%
    corruption this gate exists to prevent, reported as a pass.

    The same containment test is satisfied by any neighbouring text: a
    preceding `echo "we run with --all-features" && cargo mutants -p X`
    passes, and so does a trailing `# dropped --all-features temporarily`,
    because `logical_lines` only drops comments that occupy a whole line.
    Both were constructed against this repository's own two files and both
    reported exit 0. Splitting first is what makes those three shapes red.

    Quote-aware, because a separator inside a quoted argument is an
    argument and not a separator. A `#` that starts a word ends the line:
    everything after it is a comment, and it is dropped here — before any
    flag test sees it — rather than being allowed to vouch for the command
    in front of it.
    """
    out: list[str] = []
    current: list[str] = []
    quote: str | None = None
    index = 0
    while index < len(line):
        char = line[index]
        if quote is not None:
            current.append(char)
            if char == quote:
                quote = None
            index += 1
            continue
        if char in "'\"":
            quote = char
            current.append(char)
            index += 1
            continue
        # A comment starts at the beginning of a word, as it does in the
        # shell, in TOML and in YAML. `foo#bar` is not a comment and
        # neither is `$#`.
        if char == "#" and (index == 0 or line[index - 1].isspace()):
            break
        matched = next(
            (sep for sep in SEPARATORS if line.startswith(sep, index)), None)
        if matched is not None:
            out.append("".join(current))
            current = []
            index += len(matched)
            continue
        current.append(char)
        index += 1
    out.append("".join(current))
    return [command.strip() for command in out if command.strip()]


def tokens_of(command: str) -> list[str]:
    """`shlex` tokens for one command, or a whitespace split if it will not lex.

    An unbalanced quote makes `shlex` raise rather than answer. That is a
    malformed command and not this gate's business to diagnose, but
    falling over on it would take the gate out — so the fallback keeps the
    flag test running on something, and a flag on its own word survives a
    whitespace split intact.
    """
    try:
        return shlex.split(command, comments=True)
    except ValueError:
        return command.split()


def invocation_tokens(command: str) -> list[str] | None:
    """The invocation inside one command, from `cargo` onwards, or None.

    `cargo` and `mutants` have to be two adjacent *tokens*, which is what
    stops prose from qualifying: `echo "we run cargo mutants
    --all-features"` is two tokens, the second of them a quoted sentence,
    and it is not an invocation of anything.

    Not anchored at the first token, deliberately. A workflow step
    written inline — `- run: cargo mutants -p x --jobs 1`, which is
    ordinary YAML and is a fixture in this gate's own contract test —
    begins `-`, `run:`. Requiring position 0 would make every command of
    that shape invisible to the gate while it went on reporting OK, which
    is the same class of hole as counting a line instead of a command.
    Everything before `cargo` is dropped rather than searched, so a flag
    that belongs to a wrapper (`FOO=--all-features cargo mutants …`) does
    not vouch for the invocation either.
    """
    tokens = tokens_of(command)
    for index in range(len(tokens) - 1):
        if tokens[index] == "cargo" and tokens[index + 1] == "mutants":
            return tokens[index:]
    return None


def invocations(path: pathlib.Path) -> list[tuple[int, str, list[str]]]:
    """Every `cargo mutants` invocation in one file.

    Returns (1-based line where the logical line starts, the command as
    written, the invocation's tokens). The text of a command an
    invocation shares a line with cannot make it carry a flag.
    """
    try:
        text = path.read_text()
    except OSError as error:
        raise CannotRun(f"cannot read {path}: {error}") from error
    found: list[tuple[int, str, list[str]]] = []
    for number, line in logical_lines(text):
        if not INVOCATION.search(line):
            continue
        for command in commands(line):
            tokens = invocation_tokens(command)
            if tokens is not None:
                found.append((number, command, tokens))
    return found


def default_paths() -> list[pathlib.Path]:
    """Everything that can hold an executable invocation, not two names.

    Hard-coding `mise.toml` and `.github/workflows/ci.yml` made a third
    executable copy — a second workflow, a release job, a matrix moved to
    a file of its own — invisible while the gate went on reporting OK on
    the two it knew about. Globbing the workflow directory costs nothing
    and removes the class.

    `mise.toml` stays a literal name so that deleting it is a read error
    rather than a file that silently stops being checked; the workflows
    are a glob because which of them holds an invocation is not fixed.
    """
    return [DEFAULT_MISE] + sorted(
        path
        for pattern in WORKFLOW_GLOBS
        for path in DEFAULT_WORKFLOW_DIR.glob(pattern)
    )


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Check that every cargo-mutants invocation carries "
                    f"{REQUIRED_FLAG}.")
    parser.add_argument(
        "paths", nargs="*", type=pathlib.Path,
        help="files to check; defaults to mise.toml plus "
             ".github/workflows/*.yml")
    args = parser.parse_args()

    paths = args.paths or default_paths()

    checked = 0
    offenders: list[tuple[pathlib.Path, int, str]] = []
    try:
        for path in paths:
            for number, command, tokens in invocations(path):
                checked += 1
                if REQUIRED_FLAG not in tokens:
                    offenders.append((path, number, command))
        if not checked:
            # Judged over the union rather than per file. Per file, moving
            # the matrix from one workflow to another was an exit 2 on a
            # tree that is entirely in step; over the union, the only thing
            # that trips this is cargo-mutants no longer being invoked
            # anywhere — which is the case that must never read as clean.
            raise CannotRun(
                "no `cargo mutants` invocation in any of "
                f"{', '.join(str(path) for path in paths)}. This gate "
                "exists to keep every invocation in step; with none of "
                "them left it is checking nothing, and reporting that as "
                "a pass would be worse than reporting nothing. If the "
                "invocations moved, point this gate at where they went; "
                "if they are genuinely gone, delete this gate in the "
                "same change."
            )
    except CannotRun as error:
        print(error, file=sys.stderr)
        return 2

    if offenders:
        print(f"{REQUIRED_FLAG} is missing from "
              f"{len(offenders)} of {checked} cargo-mutants invocation(s):",
              file=sys.stderr)
        for path, number, command in offenders:
            print(f"  {path}:{number}: {command}", file=sys.stderr)
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
          f"({', '.join(str(path) for path in paths)})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
