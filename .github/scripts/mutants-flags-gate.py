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

What the gate reads is `mise.toml` plus every `*.yml`, `*.yaml` and
`*.sh` under `.github/`, at any depth — not two hard-coded names, and not
the workflow directory alone. Naming two files made a third executable
copy invisible; globbing one directory left the composite action (this
repository has `.github/actions/rust-checks/action.yml`, with eight
`run:` steps) and the shell scripts under `.github/scripts/` just as
invisible, while the gate went on reporting OK.

Alongside the files, a *count*. `MINIMUM_INVOCATIONS` says how many
invocations the tree holds, and fewer is exit 2. It has to be asserted
rather than inferred because the "nothing to check" test below is judged
over the union of the files, and a union cannot see a count fall from two
to one: take the matrix out of everything globbed here and `mise.toml`'s
invocation keeps the union non-empty, so the gate stays green with half
of what it compares gone.

What it counts is an *invocation*, not a line. Shell puts several
commands on one line, and a containment test over the joined line is
satisfied by any of them: `cargo mutants --list … --all-features &&
cargo mutants -p X --jobs 1` has a flag in it and no flag on the
invocation that measures the floor. So each logical line is split into
commands on `&&`, `||`, `&`, `;`, `|` and a newline; each command is
lexed into words; and every adjacent `cargo mutants` pair in it is an
invocation, ending where the next one starts. A comment about the flag, a
neighbouring `echo` about the flag, a `--list` call carrying the flag and
a background job carrying it all stop vouching for the command beside
them.

And what satisfies the flag test is the flag as *written*. Three ways it
was satisfiable by something else, each executed against real copies of
this repository's own files and each reporting exit 0 with the measuring
invocation unflagged: a second invocation in one command, because only
the first pair was taken; `--exclude-re '--all-features'`, because
lexing threw the quotes away and a regex that mentions the flag lexed to
the flag; and `-- --all-features`, which is an argument to `cargo test`
and says nothing about what cargo-mutants built. So an invocation carries
the flag only if a word whose source text is exactly `--all-features`
appears in it before any `--`.

There is one comment rule, not two. A `#` that starts a word ends the
line; `foo#bar` is a path. Lexing with a second, stricter rule made
`--output out/run#3 --all-features` red on a correct tree and made
`./ci/wrap.sh --tag v1#2 cargo mutants -p x` vanish from the gate
entirely — wrong in both directions from the same disagreement. And a
command that mentions cargo-mutants and will not lex at all is exit 2:
falling back to a whitespace split turned an unbalanced quote into a way
of passing, which is the one thing a typo must never be.

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
  unreadable; a command that mentions cargo-mutants will not lex; *no*
  file it read holds a `cargo mutants` invocation at all; or the files
  hold fewer than `MINIMUM_INVOCATIONS` between them. The "none at all"
  case is judged over the union and not per file, because moving the
  matrix from one workflow to another leaves a tree entirely in step and
  would otherwise be reported as a broken gate — and the count is what
  covers the gap that leaves, where some but not all of the invocations
  have gone. Together they are the important case: if cargo-mutants stops
  being invoked where this script can see it and the script keeps
  reporting green, it is reporting on nothing. A gate that cannot find
  what it checks must not read as clean — the same argument
  `grep-gate.sh` opens with, and the same two-code split
  `file-size-gate.py` and `mutants-gate.py` use.

Run it with `mise run mutants-flags-gate`, or directly. Its contract is
asserted by `.github/scripts/test_mutants_flags_gate.py`.
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys
from typing import NamedTuple

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

# Everything under `.github/` that can run a command, not the workflow
# directory alone. A composite action runs `run:` steps exactly as a
# workflow does — this repository already has
# `.github/actions/rust-checks/action.yml` with eight of them — and a
# shell script under `.github/scripts/` is what a nightly job calls. An
# executable copy in any of those was invisible to a gate that globbed
# `.github/workflows/` while the gate went on reporting OK.
DEFAULT_GITHUB_DIR = pathlib.Path(".github")
GITHUB_GLOBS = ("**/*.yml", "**/*.yaml", "**/*.sh")

# Shell separators between one command and the next. Longest first, so
# that `&&` is one boundary rather than `&` twice and `||` is one rather
# than two empty commands.
#
# A single `&` is in here because leaving it out was a hole, not a
# simplification: `cargo mutants --list … --all-features > pop.txt &
# cargo mutants -p X --jobs 1` is one background job and one measuring
# invocation, and a gate that reads it as a single command finds the flag
# in it and reports the measuring half green. That is the same
# 27.17%-vs-36.89% corruption the `&&` case describes, one character
# apart from it.
SEPARATORS = ("&&", "||", "&", ";", "|", "\n")

# How many invocations the tree is expected to hold, at least. Asserted
# because "no invocation anywhere" is judged over the union, and a union
# is blind to a count that falls from two to one: move the matrix out of
# the globbed files entirely and `mise.toml`'s surviving invocation keeps
# the union non-empty, so the gate reports OK while the thing it was
# checking has left. A number is the only thing that notices, and it has
# to be a number a person edits deliberately in the same change that adds
# or removes an invocation.
MINIMUM_INVOCATIONS = 2


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


class Word(NamedTuple):
    """One shell word, as the shell sees it and as a person wrote it.

    `value` is the word after quote removal, which is what decides
    whether two adjacent words are `cargo mutants`. `raw` is the source
    text, which is what decides whether a word *is* the flag rather than
    merely unquoting to it — see `carries_flag`.
    """

    value: str
    raw: str


def words(command: str) -> list[Word]:
    """Lex one command into words, keeping each word's source text.

    Deliberately not `shlex`, for two reasons that are the same reason.

    `shlex.split(…, comments=True)` truncates at *any* `#`, while
    `commands` above drops a comment only where one starts a word. Two
    comment rules in one file are wrong in both directions at once:
    `--output out/run#3 --all-features` lost its flag to a `#` that is
    part of a path and went red on a correct tree, and
    `./ci/wrap.sh --tag v1#2 cargo mutants -p x` lost its *invocation* the
    same way and went green with the flag missing. There is now one rule,
    `commands`', and this function has no opinion about `#` at all.

    And `shlex` answers with values, having thrown the quoting away.
    `--exclude-re '--all-features'` lexes to a word whose value is exactly
    the flag, so the gate accepted a regex that merely mentions it as the
    feature selection of the run — executed against this repository's own
    files, exit 0, with the measuring invocation unflagged. Keeping `raw`
    beside `value` is what lets `carries_flag` ask the question that
    matters: was the flag *written*, or was it produced by unquoting
    something else?

    Raises `ValueError` on an unterminated quote or a trailing escape.
    Callers turn that into a `CannotRun`, per the module docstring: a
    command this gate cannot parse is a command it cannot judge.
    """
    out: list[Word] = []
    value: list[str] = []
    raw: list[str] = []
    started = False
    index = 0
    while index < len(command):
        char = command[index]
        if char.isspace():
            if started:
                out.append(Word("".join(value), "".join(raw)))
                value, raw, started = [], [], False
            index += 1
            continue
        started = True
        if char == "'":
            close = command.find("'", index + 1)
            if close < 0:
                raise ValueError("no closing quotation")
            value.append(command[index + 1:close])
            raw.append(command[index:close + 1])
            index = close + 1
            continue
        if char == '"':
            index += 1
            raw.append('"')
            while True:
                if index >= len(command):
                    raise ValueError("no closing quotation")
                inner = command[index]
                if inner == '"':
                    raw.append('"')
                    index += 1
                    break
                if inner == "\\" and index + 1 < len(command) and \
                        command[index + 1] in '"\\$`':
                    value.append(command[index + 1])
                    raw.append(command[index:index + 2])
                    index += 2
                    continue
                value.append(inner)
                raw.append(inner)
                index += 1
            continue
        if char == "\\":
            if index + 1 >= len(command):
                raise ValueError("no escaped character")
            value.append(command[index + 1])
            raw.append(command[index:index + 2])
            index += 2
            continue
        value.append(char)
        raw.append(char)
        index += 1
    if started:
        out.append(Word("".join(value), "".join(raw)))
    return out


def invocations_in(command: str) -> list[list[Word]]:
    """Every invocation inside one command, each from its `cargo` onwards.

    `cargo` and `mutants` have to be two adjacent *words*, which is what
    stops prose from qualifying: `echo "we run cargo mutants
    --all-features"` is two words, the second of them a quoted sentence,
    and it is not an invocation of anything.

    Not anchored at the first word, deliberately. A workflow step written
    inline — `- run: cargo mutants -p x --jobs 1`, which is ordinary YAML
    and is a fixture in this gate's own contract test — begins `-`,
    `run:`. Requiring position 0 would make every command of that shape
    invisible to the gate while it went on reporting OK, which is the
    same class of hole as counting a line instead of a command. It is
    also what lets a wrapper be seen through: `./ci/wrap.sh cargo mutants
    -p x` is the wrapper's argv and cargo-mutants' invocation both.
    Everything before `cargo` is dropped rather than searched, so a flag
    that belongs to a wrapper (`FOO=--all-features cargo mutants …`) does
    not vouch for the invocation either.

    *Every* pair, not the first. Taking only the first made the second
    invocation in a command invisible, which is the same hole the split
    into commands closes one level up; between them a command holds
    exactly as many invocations as it runs.

    Each invocation ends where the next one begins, so one invocation's
    arguments cannot be read as its neighbour's.
    """
    lexed = words(command)
    starts = [
        index
        for index in range(len(lexed) - 1)
        if lexed[index].value == "cargo" and lexed[index + 1].value == "mutants"
    ]
    return [
        lexed[start:(starts[position + 1] if position + 1 < len(starts)
                     else len(lexed))]
        for position, start in enumerate(starts)
    ]


def carries_flag(invocation: list[Word]) -> bool:
    """Whether this invocation selects features with the required flag.

    Two restrictions, each of which was a way of passing without the
    feature selection the floor depends on.

    Written, not unquoted into. `--exclude-re '--all-features'` names the
    flag in a regex; `--all-features` selects features. Only a word whose
    *source text* is the flag counts, so quoting it is not a way to
    satisfy the gate by accident.

    Before any `--`. Everything after cargo-mutants' `--` is passed
    through to the test runner, so a `--all-features` there is an
    argument to `cargo test` and says nothing about what cargo-mutants
    built.
    """
    for word in invocation:
        if word.value == "--" and word.raw == "--":
            return False
        if word.raw == REQUIRED_FLAG:
            return True
    return False


def invocations(path: pathlib.Path) -> list[tuple[int, str, list[Word]]]:
    """Every `cargo mutants` invocation in one file.

    Returns (1-based line where the logical line starts, the command as
    written, the invocation's words). The text of a command an
    invocation shares a line with cannot make it carry a flag.

    A command that mentions cargo-mutants and will not lex is a
    `CannotRun` rather than something to test leniently. The previous
    answer — fall back to a whitespace split — re-created by the back
    door the substring behaviour the split into commands removed: an
    unbalanced quote in front of `# keep --all-features later` made the
    comment's words into the command's own, and the gate went green on an
    invocation with no flag. A typo must not be a way of passing.
    """
    try:
        text = path.read_text()
    except OSError as error:
        raise CannotRun(f"cannot read {path}: {error}") from error
    found: list[tuple[int, str, list[Word]]] = []
    for number, line in logical_lines(text):
        if not INVOCATION.search(line):
            continue
        for command in commands(line):
            try:
                here = invocations_in(command)
            except ValueError as error:
                raise CannotRun(
                    f"{path}:{number}: cannot lex a command that mentions "
                    f"cargo-mutants ({error}):\n    {command}\n\n"
                    "The gate refuses rather than guessing. Reading this "
                    "with the quoting ignored would let the text after an "
                    "unbalanced quote — a trailing comment, say — supply "
                    f"{REQUIRED_FLAG} to a command that does not have it. "
                    "Balance the quotes and run it again."
                ) from error
            for invocation in here:
                found.append((number, command, invocation))
    return found


def default_paths() -> list[pathlib.Path]:
    """Everything that can run an invocation, not two names.

    Hard-coding `mise.toml` and `.github/workflows/ci.yml` made a third
    executable copy — a second workflow, a release job, a matrix moved to
    a file of its own — invisible while the gate went on reporting OK on
    the two it knew about. Globbing `.github/workflows/` removed one
    third of that class and left the rest: a composite action under
    `.github/actions/` runs `run:` steps exactly as a workflow does, and
    a shell script under `.github/scripts/` is what a job calls. Each of
    those was executed with an unflagged invocation in it and each
    reported OK. So the universe is every `*.yml`, `*.yaml` and `*.sh`
    under `.github/`, at any depth.

    `mise.toml` stays a literal name so that deleting it is a read error
    rather than a file that silently stops being checked; the rest is a
    glob because which file holds an invocation is not fixed.
    """
    return [DEFAULT_MISE] + sorted(
        path
        for pattern in GITHUB_GLOBS
        for path in DEFAULT_GITHUB_DIR.glob(pattern)
    )


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Check that every cargo-mutants invocation carries "
                    f"{REQUIRED_FLAG}.")
    parser.add_argument(
        "paths", nargs="*", type=pathlib.Path,
        help="files to check; defaults to mise.toml plus every "
             "*.yml, *.yaml and *.sh under .github/")
    parser.add_argument(
        "--min-invocations", type=int, default=MINIMUM_INVOCATIONS,
        help="how many invocations the files must hold between them "
             f"(default {MINIMUM_INVOCATIONS}); fewer is exit 2, because "
             "a gate checking less than it was written to check is not a "
             "pass")
    args = parser.parse_args()

    paths = args.paths or default_paths()

    checked = 0
    offenders: list[tuple[pathlib.Path, int, str]] = []
    try:
        for path in paths:
            for number, command, invocation in invocations(path):
                checked += 1
                if not carries_flag(invocation):
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
        if checked < args.min_invocations:
            # The union above cannot see this: one invocation left out of
            # two keeps the union non-empty, so every check passes while
            # half of what the gate was written to compare has walked out
            # of the file set. Only a count notices.
            raise CannotRun(
                f"found {checked} `cargo mutants` invocation(s) in "
                f"{', '.join(str(path) for path in paths)}, and expected "
                f"at least {args.min_invocations}. An invocation this "
                "gate can no longer see is one it can no longer keep in "
                "step, and the files it reads still look fine — which is "
                "why the number is asserted rather than inferred. If an "
                "invocation moved somewhere this gate does not read, "
                "point it there; if one was deliberately removed, lower "
                f"{MINIMUM_INVOCATIONS!r} at MINIMUM_INVOCATIONS in "
                "this script in the same change."
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
            "this gate does not check and which should say the same "
            "thing.\n"
            "\nOne case where the remedy is not the flag: this gate joins "
            "continued lines on a trailing backslash and nothing else, so "
            "an invocation rewritten as a YAML folded scalar (`run: >-`, "
            "with the argument lines joined by YAML rather than by `\\`) "
            "reads here as several commands and the one holding `cargo "
            "mutants` loses the arguments under it. If an invocation "
            "above is spread over lines with no backslashes, the fix is "
            "to put the backslashes back — a block scalar (`run: |`) with "
            "explicit continuations — not to add a second copy of the "
            "flag.",
            file=sys.stderr)
        return 1

    print(f"OK: {checked} cargo-mutants invocation(s) carry {REQUIRED_FLAG} "
          f"({', '.join(str(path) for path in paths)})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
