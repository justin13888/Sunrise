---
status: accepted
---

# Identifiers

Every entity has a globally unique, sortable, opaque ID.

## Format

**ULID** (128-bit, Crockford-base32, 26 chars), namespaced by entity kind:

```
tsk_01HZX3KJ2P8M9V5R7W4QC8N1AB
str_01HZX3K7Q9YJ4N3M6S2T1V0DEF
ctx_01HZX2W8P6E5Q4R7Y9N1M0AGHI
…
```

Prefixes (`tsk_`, `str_`, `ctx_`, `rtn_`, `blk_`, `not_`, `att_`, `prs_`, `dev_`, `idn_`, `fcs_`, `rvw_`).

## Why ULID, not UUID

- **Sortable.** Time-ordered prefix; we use this for op-log indexing and for "natural" UI sort fallback.
- **Compact.** 26 chars vs 36; easier to log and share for debugging.
- **Random tail.** Cryptographically random low 80 bits prevent enumeration.

## Why namespaced prefix

- Cheap discrimination in code paths and log output.
- Allows a single `EntityRef` enum to be parsed back from a string without external schema info.
- Prevents accidental cross-type ID reuse in code.

## ID generation

- Generated **client-side** (devices), never server-side. The server has no concept of "next ID."
- Each device draws the ULID's random tail from its own OS CSPRNG, but reaches it through the injected `CoreConfig::rng` seam rather than calling it directly: `sunrise-id` takes caller-supplied randomness (`Ulid::from_timestamp_and_random`) and has no RNG dependency at all, `Engine::fresh_id` fills those ten bytes from `CoreConfig::rng`, and the production binding `SystemRng` is `OsRng` — i.e. `getrandom`. That indirection is determinism rule 1, and it is what lets a test mint reproducible ids without a fake clock fighting a real RNG.
- ULID has 80 bits of randomness; with `n` IDs minted in a single millisecond on a single device, the per-millisecond birthday-bound collision probability is ≈ `n² / 2^81`. At `n = 10` (a heavy capture burst on one device): ≈ 4.1 × 10⁻²². Across devices, distinct millisecond-prefix windows make cross-device collision strictly less likely than the worst-case same-device bound. Treat collisions as cryptographically impossible; do not write fallback paths for them. v1 does not implement runtime collision detection; if a collision is ever reported in the wild, it is a sev-1 incident, and recovery uses the audit-log root to identify which entity is the original.

## Stability

- IDs are immutable for the lifetime of the entity.
- Deletion is logical; the ID is retained as a tombstone for op-log convergence (see [`../04-storage/compaction.md`](../04-storage/compaction.md) for when tombstones are pruned).

## EntityRef

Code uses a typed reference, not raw strings, except at I/O boundaries:

```rust
pub struct EntityRef { kind: EntityKind, bytes: [u8; 16] }

pub enum EntityKind { Task, Stream, Context, Routine, Block, Note, Attachment, Person, Device, Identity, FocusSession, ReviewSnapshot }
```

Both fields are **private**; `kind()`, `bytes()` and `ulid()` read them and
`new()` / `from_ulid()` build one. It was specified here as a tuple struct with
positional public fields and has never been written that way.

Parsing a string ID:

- `EntityRef::parse(s, expected)` validates that the prefix matches the expected kind; `EntityRef::parse_any(s)` accepts any known prefix. There is no `EntityRef::any`.
- Decode Crockford base32 of the 26-char body to 16 bytes.

## Foreign IDs

**No foreign id is stored alongside a Sunrise id today.** This section used to
say external ids are "stored alongside the Sunrise ID, never replacing it" when
integrating with Google Calendar or CalDAV. All three parts of that were
untrue: CalDAV is an explicit v1 non-goal
([`../00-product/non-goals.md`](../00-product/non-goals.md)), Google Calendar is
implemented but wired to nothing and deferred from the v1 MUST set
([`../09-integrations/google-calendar.md`](../09-integrations/google-calendar.md)),
and the `external_id` field that would hold such an id has not landed —
`crates/sunrise-domain/src/import.rs:7-9` says so at the point it matters.

What the one shipping integration does instead — `.ics` import — is fold the
foreign id **into** the Sunrise id. `sunrise_domain::imported_block_id(source,
uid)` hashes `(source, uid)` into the Block's 16 bytes, exactly as a
materialized routine occurrence hashes `(routine, occurrence)` into a Task's.
Two properties fall out of that, and both are the point:

- **Idempotence.** Re-importing the same file computes the same id, so the
  write updates the Block already there instead of minting a second one. No
  side table, nothing to keep in step with the vault.
- **Convergence.** Two devices importing the same file independently compute
  the same id, so their ops merge under the ordinary entity-level LWW rule
  ([ADR-0014](../11-adr/0014-entity-level-lww-merge.md)) rather than leaving
  the user with two copies of every event. A device-local dedup table could not
  do this.

The cost, stated plainly: the derivation is one-way, so a **foreign** `UID` is
not recoverable from the Block it produced. Sunrise's own exports are exempt —
`block_uid` writes `<block-id>@sunrise.invalid` and `imported_block_id`
recognises that form and returns the id unhashed, which is what makes export →
re-import the identity — but an event that arrived from someone else's calendar
cannot be handed its original `UID` back.

Closing that is what `external_id` is for, added by
[ADR-0025](../11-adr/0025-integration-account-entity.md) and **not yet built**.
Note what it is not, even once it lands: it carries a foreign id back *out*, and
dedup stays keyed on the derived id — see
[`time-blocks.md`](./time-blocks.md#external_id-is-not-the-dedup-key).

See [`../09-integrations/`](../09-integrations).

## Display in UI

- Internal IDs are not shown to users in normal flows.
- They are visible in: developer console, debug exports, error reports (with user consent).
- "Copy permalink" copies a `sunrise://entity/<EntityRef>` URI for in-app deep linking; not a sharable URL.
