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

Prefixes (`tsk_`, `str_`, `ctx_`, `rtn_`, `blk_`, `not_`, `att_`, `prs_`, `dev_`, `idn_`).

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
- Each device uses its own OS CSPRNG (`getrandom`).
- ULID has 80 bits of randomness; with `n` IDs minted in a single millisecond on a single device, the per-millisecond birthday-bound collision probability is ≈ `n² / 2^81`. At `n = 10` (a heavy capture burst on one device): ≈ 4.1 × 10⁻²². Across devices, distinct millisecond-prefix windows make cross-device collision strictly less likely than the worst-case same-device bound. Treat collisions as cryptographically impossible; do not write fallback paths for them.

## Stability

- IDs are immutable for the lifetime of the entity.
- Deletion is logical; the ID is retained as a tombstone for op-log convergence (see [`../04-storage/compaction.md`](../04-storage/compaction.md) for when tombstones are pruned).

## EntityRef

Code uses a typed reference, not raw strings, except at I/O boundaries:

```rust
pub struct EntityRef(EntityKind, [u8; 16]);

pub enum EntityKind { Task, Stream, Context, Routine, Block, Note, Attachment, Person, Device, Identity }
```

Parsing a string ID:

- Validate prefix matches expected kind (or accept any kind for `EntityRef::any`).
- Decode Crockford base32 of the 26-char body to 16 bytes.

## Foreign IDs

When integrating with Google Calendar or CalDAV, external IDs are stored alongside the Sunrise ID, never replacing it. See [`../09-integrations/`](../09-integrations).

## Display in UI

- Internal IDs are not shown to users in normal flows.
- They are visible in: developer console, debug exports, error reports (with user consent).
- "Copy permalink" copies a `sunrise://entity/<EntityRef>` URI for in-app deep linking; not a sharable URL.
