#!/usr/bin/env python3
"""Fail when a refusal below `apply_remote_all`'s idempotence gate raises.

Why this gate exists
--------------------

`Engine::apply_remote_all` opens one transaction, inserts the op row, and
checks `tx.changes()`: that is its idempotence gate. Everything below it -- the
control dispatch into `apply_control_op` (and from there
`apply_device_revoke`) and the entity materialization in `lww.rs` -- runs
inside the same `with_tx`.

`upsert_sync_cursor`'s doc run in `crates/sunrise-core/src/engine/oplog.rs`
rests on a premise about that span: "The op row went in at the idempotence
gate before the control op was dispatched", and a refusal there declines a
register write, a hint row or a payload, never the delivery. So the sync
cursor counts the op whatever answer it got.

That premise holds only because every refusal on those paths returns `Ok(..)`
and logs. One `?` or `return Err(..)` added to a policy branch -- the ordinary
way to write a new refusal -- rolls the transaction back, takes the op row with
it, and falsifies the paragraph. Every unit stays green, because each confirms
one refusal and none asserts the absence of the next; `mise run citations` and
`mise run doc-comments` stay green, because the doc is still well-formed. This
gate is what goes red instead (issue #298).

What it reads
-------------

`SPANS` below: the bodies of `apply_control_op` and `apply_device_revoke`, and
the whole of `lww.rs`. Comments and the contents of string and char literals
are blanked first, so prose that names `Err(` is not code that returns one.

What counts as a violation
--------------------------

1. **An error minted in the span.** `Err(..)` as a value (`return Err(..)`, a
   tail `Err(..)`, a match arm that evaluates to one), `bail!`, `ensure!`,
   `anyhow!`, `.ok_or(..)`, `.ok_or_else(..)`, `.map_err(..)`, and a
   `..Error::Variant` spelled outside an `Err(..)` pattern. `Err(..)` as a
   *pattern* -- `if let Err(e) = ..`, `Err(e) => ..` -- is how a failure is
   caught and logged, which is the house rule, and is allowed.
2. **A panic in the span.** `panic!`, `unreachable!`, `todo!`,
   `unimplemented!`, `assert!`/`assert_eq!`/`assert_ne!`, `.unwrap()` and
   `.expect(..)`. A panic unwinds out of `with_tx` and rolls it back exactly as
   an error does. `debug_assert!` is allowed: `materialize_remote` uses one as
   a routing tripwire that is compiled out of the shipped build.
3. **A `?` on a callee nobody classified.** Most `?` in the span propagate a
   storage failure -- a statement that could not run -- and that is not a
   refusal: the delivery failed, and the relay resends it. The issue that asked
   for this gate named "no `?` on a fallible call" and that cannot be the rule,
   because the spans hold some seventy of them and every one is a SQLite
   statement or a helper around one. So each callee a `?` propagates from is
   named in `PROPAGATES` with what it is, and a `?` on any other name fails.
   Adding a name is the moment somebody has to answer "can this return an
   error that is a policy decision?" -- and if it can, the premise above is
   what breaks.

What it does not see
--------------------

A refusal minted *inside* a classified callee, or in a callee reached as a
tail expression rather than through `?` (`apply_control_op`'s
`DeviceRevoke` arm reaches `apply_device_revoke` that way, which is why that
function is a span of its own). `materialize_remote` in `lww.rs` tail-returns
into two such callees that no span reads: `materialize_focus_remote`
(`focus.rs`) and `insert_review_snapshot_row` (`review.rs`). Neither holds a
policy branch -- each is an `INSERT OR IGNORE` of an append-only record -- and
`insert_review_snapshot_row`'s one `.map_err` is an encoding failure mapped
onto a rusqlite error, the kind of conversion a span would have to exempt; so
they stay outside `SPANS`, and a refusal added to either is not seen here.
Following every call transitively was tried
and rejected: the closure is some 150 functions across the keychain and the
entity writers, with dozens of legitimate encoding-failure conversions and
same-named methods in unrelated modules, and a gate that has to be taught
that much noise stops being read. The regression unit
`a_losing_lww_write_keeps_its_op_row_and_is_counted` in `engine/tests.rs`, and
the two cursor units it sits beside, hold the behaviour end to end for the
refusals that exist today.

Exit codes
----------

0 clean; 1 a violation, or a span the gate could not find (a renamed function
would otherwise make this read nothing and pass).

Run it with `mise run apply-refusal`; `mise run apply-refusal-gate-test`
asserts it can still say no.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ENGINE = Path("crates/sunrise-core/src/engine")

# (file under ENGINE, function name or None for the whole file).
SPANS: tuple[tuple[str, str | None], ...] = (
    ("sync.rs", "apply_control_op"),
    ("revocation.rs", "apply_device_revoke"),
    ("lww.rs", None),
)

# Every callee a `?` in SPANS may propagate from, and what it is. None of them
# returns an error for a policy reason today; each returns rusqlite's error
# for a statement that could not run, or an encoding failure mapped onto one.
PROPAGATES: dict[str, str] = {
    # rusqlite itself.
    "execute": "rusqlite statement",
    "query_row": "rusqlite statement",
    "optional": "rusqlite QueryReturnedNoRows -> None adapter",
    "get": "rusqlite column read",
    # Keychain rows, read and written inside the apply transaction.
    "max_epoch_tx": "keychain row read",
    "absorb_stream_key": "keychain row write",
    # Identity chain storage (identity.rs).
    "current_identity": "identity chain read",
    "chain_identities": "identity chain read",
    "admit_sibling": "identity chain write; a refused sibling is Ok(false)",
    "purge_unverifiable_siblings": "identity chain write",
    "recompute_identity_head": "identity chain write; a bad link is logged",
    "apply_roster": "roster write",
    # Revocation register and its tables (revocation.rs).
    "is_read_bounded": "device_read_bounds read",
    "record_envelope_recipient": "key_envelope_recipients hint write",
    "cap_device_revoke_targets": "device_revoke_ops count; a capped target is Ok(true)",
    "refold_device_revocations": "register fold",
    "compact_device_revoke_ops": "device_revoke_ops delete",
    "release_orphan_read_bounds": "device_read_bounds delete",
    # Feature folds (features.rs). A malformed id is skipped and logged.
    "apply_vault_requires": "vault_required_features write",
    "apply_device_features": "device_features write",
    # Entity rows (lww.rs and the per-kind writers it calls).
    "read_row_lww": "entity row read",
    "ensure_stream_row": "streams row write",
    "insert_stream_row": "streams row write",
    "update_stream_row": "streams row write",
    "insert_task_row": "tasks row write",
    "update_task_row": "tasks row write",
    "insert_task_contexts": "task_contexts write",
    "replace_task_contexts": "task_contexts write",
    "replace_task_blockers": "task_blockers write",
    "ftsr_upsert_task": "full-text index write",
    "ftsr_delete_task": "full-text index write",
    "insert_context_row": "contexts row write",
    "update_context_row": "contexts row write",
    "purge_context_from_tasks": "task_contexts delete",
    "insert_routine_row": "routines row write",
    "update_routine_row": "routines row write",
    "upsert_block_row": "blocks row write",
    "replace_block_tasks": "block_tasks write",
    "upsert_attachment_row": "attachments row write",
}

PREMISE = (
    "upsert_sync_cursor's doc (crates/sunrise-core/src/engine/oplog.rs) rests on "
    '"The op row went in at the idempotence gate before the control op was '
    'dispatched": a refusal below apply_remote_all\'s idempotence gate declines '
    "a register write, a hint row or a payload, and returns Ok. An error or a "
    "panic there rolls the transaction back, deletes the op row, and leaves the "
    "sync cursor short of an op the relay has already delivered."
)

MINTED = re.compile(
    r"\bbail!|\bensure!|\banyhow!|\.ok_or(?:_else)?\s*\(|\.map_err\s*\("
    r"|\b(?:panic|unreachable|todo|unimplemented)!"
    r"|(?<![\w])assert(?:_eq|_ne)?!"
    r"|\.unwrap\s*\(\s*\)|\.expect\s*\("
)
ERR_CALL = re.compile(r"\bErr\s*\(")
ERROR_VARIANT = re.compile(r"\bError::[A-Z]")
FN_DEF = r"\bfn\s+{}\b"


def strip_code(text: str) -> str:
    """Blank comments and literal contents, keeping every offset and newline."""
    out = list(text)
    n = len(text)
    raw = re.compile(r'b?r(#*)"')

    def blank(a: int, b: int) -> None:
        for k in range(a, min(b, n)):
            if out[k] != "\n":
                out[k] = " "

    i = 0
    while i < n:
        c = text[i]
        if text.startswith("//", i):
            j = text.find("\n", i)
            j = n if j < 0 else j
            blank(i, j)
            i = j
            continue
        if text.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            blank(i, j)
            i = j
            continue
        m = raw.match(text, i)
        if m and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
            close = '"' + m.group(1)
            j = text.find(close, m.end())
            j = n if j < 0 else j
            blank(m.end(), j)
            i = j + len(close)
            continue
        if c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            blank(i + 1, j)
            i = j + 1
            continue
        if c == "'":
            # A char literal ('x', '\n', '\u{..}') or a lifetime ('a, '_).
            if i + 1 < n and text[i + 1] == "\\":
                j = text.find("'", i + 2)
                j = n if j < 0 else j
                blank(i + 1, j)
                i = j + 1
                continue
            if i + 2 < n and text[i + 2] == "'":
                blank(i + 1, i + 2)
                i += 3
                continue
        i += 1
    return "".join(out)


def close_of(s: str, i: int, o: str, c: str) -> int:
    """Index of the bracket closing the one at `i`, or -1."""
    depth = 0
    for j in range(i, len(s)):
        if s[j] == o:
            depth += 1
        elif s[j] == c:
            depth -= 1
            if depth == 0:
                return j
    return -1


def open_of(s: str, i: int, o: str, c: str) -> int:
    """Index of the bracket opening the one closing at `i`, or -1."""
    depth = 0
    for j in range(i, -1, -1):
        if s[j] == c:
            depth += 1
        elif s[j] == o:
            depth -= 1
            if depth == 0:
                return j
    return -1


def fn_body(s: str, name: str) -> tuple[int, int] | None:
    """(start, end) of the braced body of the one `fn name`, else None.

    The signature is walked at bracket depth zero, because
    `-> Result<Vec<([u8; 16], u32)>>` holds a `;` that is not the end of a
    declaration.
    """
    found = [m for m in re.finditer(FN_DEF.format(re.escape(name)), s)]
    if len(found) != 1:
        return None
    depth = 0
    for j in range(found[0].end(), len(s)):
        ch = s[j]
        if ch in "([":
            depth += 1
        elif ch in ")]":
            depth -= 1
        elif depth == 0 and ch == ";":
            return None
        elif depth == 0 and ch == "{":
            end = close_of(s, j, "{", "}")
            return None if end < 0 else (j, end)
    return None


def is_pattern(s: str, open_paren: int) -> bool:
    """Whether the `Err(` whose paren opens at `open_paren` is a pattern."""
    close = close_of(s, open_paren, "(", ")")
    if close < 0:
        return False
    rest = s[close + 1 :].lstrip()
    if rest.startswith("=>") or re.match(r"if\b", rest):  # an arm, guarded or not
        return True
    if rest.startswith("=") and not rest.startswith("=="):
        return True
    return rest.startswith("|") and not rest.startswith("||")


def callee_of(s: str, q: int) -> str | None:
    """The function a `?` at `q` propagates from, or None if it is no call."""
    j = q - 1
    while j >= 0 and s[j].isspace():
        j -= 1
    if j < 0 or s[j] != ")":
        return None
    j = open_of(s, j, "(", ")") - 1
    while j >= 0 and s[j].isspace():
        j -= 1
    if j >= 0 and s[j] == ">":  # turbofish: name::<T>(..)
        j = open_of(s, j, "<", ">") - 1
        if s[j - 1 : j + 1] != "::":
            return None
        j -= 2
    end = j + 1
    while j >= 0 and (s[j].isalnum() or s[j] == "_"):
        j -= 1
    name = s[j + 1 : end]
    return name if re.fullmatch(r"[A-Za-z_]\w*", name) else None


def violations(src: str, start: int = 0, end: int | None = None) -> list[tuple[int, str]]:
    """(offset, what) for every violation in stripped `src[start:end]`."""
    end = len(src) if end is None else end
    body = src[start:end]
    hits: list[tuple[int, str]] = []
    exempt: list[tuple[int, int]] = []
    for m in ERR_CALL.finditer(body):
        paren = m.end() - 1
        if is_pattern(body, paren):
            exempt.append((paren, close_of(body, paren, "(", ")")))
        else:
            hits.append((m.start(), "an error minted as a value: Err(..)"))
    for m in ERROR_VARIANT.finditer(body):
        if not any(a < m.start() < b for a, b in exempt):
            hits.append((m.start(), "an error variant constructed: Error::.."))
    for m in MINTED.finditer(body):
        hits.append((m.start(), f"an error or panic minted: {m.group(0).strip()}"))
    for q, ch in enumerate(body):
        if ch != "?":
            continue
        name = callee_of(body, q)
        if name is None:
            hits.append((q, "a `?` on something that is not a call"))
        elif name not in PROPAGATES:
            hits.append((q, f"a `?` on `{name}`, which PROPAGATES does not classify"))
    return sorted((start + off, what) for off, what in hits)


def main() -> int:
    if not ENGINE.is_dir():
        print(f"::error::apply-refusal: {ENGINE} does not exist; gate cannot run.")
        return 1
    found: list[str] = []
    for rel, name in SPANS:
        path = ENGINE / rel
        label = f"{path}#{name}" if name else str(path)
        if not path.is_file():
            print(f"::error::apply-refusal: {path} does not exist; the span {label} reads nothing.")
            return 1
        text = path.read_text()
        src = strip_code(text)
        if name is None:
            span = (0, len(src))
        else:
            span = fn_body(src, name)
            if span is None:
                print(
                    f"::error::apply-refusal: no single `fn {name}` with a body in "
                    f"{path}. If it moved or was renamed, update SPANS -- a span "
                    "the gate cannot find is a span it does not check."
                )
                return 1
        for off, what in violations(src, *span):
            line_no = src.count("\n", 0, off) + 1
            line = text.splitlines()[line_no - 1].strip()
            found.append(f"  {path}:{line_no} ({label}): {what}\n      {line}")
    if found:
        print("::error::apply-refusal: a refusal below the idempotence gate can raise.")
        print("\n".join(found))
        print()
        print(PREMISE)
        print(
            "Log the refusal and return Ok, as every existing one does. If the new "
            "`?` propagates a storage failure, name its callee in PROPAGATES with "
            "what it is. If the op really must be declined, the premise above and "
            "upsert_sync_cursor's doc have to change with it."
        )
        return 1
    labels = ", ".join(f"{r}#{n}" if n else r for r, n in SPANS)
    print(f"OK: apply-refusal clean -- nothing below the idempotence gate raises ({labels}).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
