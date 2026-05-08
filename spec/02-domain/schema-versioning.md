---
status: accepted
---

# Schema Versioning

Domain entities evolve. Devices on different versions must keep syncing.

## Three layers, three versions

| Layer | Version | Bump on |
|---|---|---|
| **Wire protocol** | `protocol_version` | Sync protocol changes (new message kinds, framing) |
| **CRDT document schema** | `doc_schema_version` | Entity/field additions or removals |
| **Local DB schema** | `db_schema_version` | SQLite tables/indexes for materialized views |

These are independent. Adding a new task field bumps `doc_schema_version` only. Switching from WebSocket to QUIC bumps `protocol_version` only.

## Compatibility windows

- **Wire protocol:** N and N-1 must interoperate. N-2 is rejected with a clear "please update your client" error.
- **Doc schema:** infinite forward compat. Newer fields on older clients are *preserved* but ignored. Older clients re-emit unknown fields verbatim. (This requires CRDT op encoding to round-trip unknown fields — see [`../05-sync/crdt-design.md`](../05-sync/crdt-design.md).)
- **DB schema:** local-only; runs migrations in-place on first launch of a new version.

## Adding a field

1. Add to the CDDL spec in `02-domain/`.
2. Add a Rust struct field with `#[serde(default)]` and an `Option`/sensible default.
3. Add a migration in `04-storage/migrations.md` (for materialized indexes if needed).
4. Update CRDT codec to round-trip the new field.
5. Bump `doc_schema_version`.

## Removing a field

1. Mark deprecated in CDDL with a comment and a deadline (≥2 minor versions).
2. Stop reading. Continue writing for the deprecation window so older clients that need it still get it.
3. After the window, stop writing. Older clients fall back to the field's default.
4. Bump `doc_schema_version`.

## Renaming a field

Treated as add + remove. Don't reuse names.

## Breaking changes

Avoid. If unavoidable:

- Coordinate a flag-day version that both halves understand for a transition period.
- Provide an automatic migration path on the user's data.
- Coordinate a managed-cloud release that gates older clients to read-only until they update.

## What this means for the v1 launch

- The CDDL specs in `02-domain/` are the authoritative shape at `doc_schema_version = 1`.
- We expect rapid iteration in the first 6 months. Therefore:
  - All entities have `unknown_fields: { * tstr => any }` carve-outs in the CRDT codec.
  - All scalar enums have an "unknown" fallback so a future `state: "delegated"` doesn't crash older clients.

## Encryption granularity

Encryption is at the **op envelope** level, not the field level. The entire CRDT op (any field set, any payload) is encrypted as one unit under the Stream key. There are no field-level encryption sub-keys; "indexable plaintext metadata" does not exist on the server. See [`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md) for the envelope structure. Schema additions therefore never expand the server's ciphertext-visibility surface.
