---
status: accepted
---

# Migrations

> **Pre-1.0 baseline, and three appends on top of it.** `BASELINE_STORAGE_V` is
> **13**: migrations 0001–0012 were collapsed into
> `crates/sunrise-storage/migrations/0013_baseline.sql` and deleted, and a vault
> stamped `0 < storage_v < 13` is **refused**
> (`DbError::StorageVPreBaseline` → `STORAGE_V_TOO_OLD`) rather than upgraded.
> `STORAGE_V` is now **16**, and the whole of `MIGRATIONS` is four files:
> `0013_baseline.sql`, `0014_stream_sort_order.sql`,
> `0015_entity_extra_columns.sql` and
> `0016_stream_description_and_default_context.sql`. 0014 was the first
> migration appended after the reset, and 0013 was not touched to make room for
> it — which is exactly the append-only rule the reset reinstated. 0015 adds the
> `extra BLOB` column to the five column-projected entities whose forward-compat
> unknowns had nowhere to live (see
> [`../02-domain/schema-versioning.md`](../02-domain/schema-versioning.md)
> §Compatibility windows). 0016 gives `Stream.description` and
> `Stream.default_context` the columns their CDDL has always declared — the
> first of which was accepted, carried in the op, and then erased on every
> replica by the next update, because the projection could not hold it. All
> three obey the same rule.
> The runner, the ordering rule and the single-transaction guarantee are
> unchanged. See [ADR-0018](../11-adr/0018-storage-baseline-reset.md) for why
> the collapse was done once, and why it does not happen again after 1.0.

Two kinds of migrations:

1. **Local DB migrations.** Schema changes in SQLite. Run on app launch.
2. **Doc-schema migrations.** Changes to the entity/field shapes (`DOC_SCHEMA_V`). Coordinated across devices.

## Local DB migrations

Each migration:

- Has a numeric version (`db_schema_version`), migrations applied in order.
- Is idempotent (re-running is a no-op).
- Is forward-only. No down-migration. (Restore from backup if a migration is wrong.)
- Lives in `crates/sunrise-storage/migrations/<NNNN>_<name>.sql` *or* a Rust function for non-trivial transforms.

Framework: a custom thin layer (we don't use `refinery` because we want explicit transactions and a per-migration commit checkpoint that includes the version write).

Sample:

```sql
-- 0007_add_task_energy.sql
ALTER TABLE tasks ADD COLUMN energy TEXT;

UPDATE schema_meta SET db_schema_version = 7;
```

Migrations run in a single transaction per migration; failure rolls back; the app refuses to launch on migration failure and surfaces a clear error with a "send diagnostics" path.

### Failure recovery

- Failure mid-migration leaves the previous schema intact (migrations run in a single transaction; ROLLBACK on error).
- The app refuses to launch with `STORAGE_MIGRATION_FAILED { from_v, to_v, error_code }`. Diagnostic-bundle export is allowed; vault data access is not.
- Recovery options surfaced in UI:
  1. **Retry** — re-run the migration; useful for transient I/O errors.
  2. **Restore from backup** — if user has an external backup of the vault directory.
  3. **Reset and resync** — delete local vault, re-pair the device; remote ops are intact.
- Downgrades require an explicit reverse migration with its own ADR; v1 ships no reverse migrations.

## Doc-schema migrations

These are coordinated:

1. **Add a field.** Trivial; old clients don't see it. Bump `doc_schema_version`.
2. **Remove a field.** Two-stage:
   - Stage A: stop reading. Bump version A.
   - Stage B: stop writing. Bump version B (after the A-version is the minimum across all known devices for the user).
3. **Rename a field.** Add new + dual-write + stop reading old + stop writing old. Or keep both forever.
4. **Change a type.** Treated as remove + add. Rare; only with extreme care.

The user's vault stores the highest `doc_schema_version` it has seen any op produced under. Devices on older versions will refuse to *originate* ops under a newer version — they read but don't write past their max.

This produces a property: a v1 device and a v2 device coexist, with the v1 device gracefully degrading.

> **Neither half of that paragraph is implemented.** The vault records
> `storage_v` in `schema_meta` and nothing else: **there is no stored highest
> `doc_schema_version` seen**, and consequently **no gate stopping an older
> build from originating ops under a newer version**. Every op this build emits
> carries `DOC_SCHEMA_V` from `crates/sunrise-cbor/src/version.rs`, and the only
> check on the receive side is the floor. The degradation property holds for a
> different reason — forward-compatible field preservation — not because
> anything refuses to write.
>
> §Minimum-version discovery below is target state for the same reason: there
> are no cursor heartbeats carrying `doc_schema_v_max`, no 7-day grace, and no
> server-side `doc_schema_floor` that advances.

### Minimum-version discovery

The "minimum `DOC_SCHEMA_V` across all known devices" is determined from device-cursor heartbeats. Each cursor op carries `doc_schema_v_max` (the highest version that device can write).

- After all active devices' cursors report `doc_schema_v_max ≥ N+1`, a 7-day grace begins.
- After grace, the **server's `doc_schema_floor`** advances to `N+1`, refusing inbound ops with `doc_schema_v < N+1`.
- A device that has not heartbeat in 30 days is considered abandoned and excluded from the minimum.

## Materialized state rebuild

> **Not implemented, at any layer.** There is no rebuild path in the workspace:
> no command, no seam function, no CLI verb, and no internal helper — `rebuild`
> appears nowhere in `crates/`. The op log genuinely does hold everything needed
> (every op is full-state under
> [ADR-0014](../11-adr/0014-entity-level-lww-merge.md), and `ops.envelope` keeps
> the sealed bytes), so the capability is buildable; nothing has built it. Today
> the recovery answer for a corrupt projection is §Failure recovery's option 3,
> "reset and resync".

If a migration is irrecoverable (a corrupt index, a bug), the user can trigger "rebuild from log":

1. Wipe materialized tables.
2. Re-apply ops in causal order.
3. Rebuild FTS index.

This is an internal capability used by the app on first launch after major-version upgrades when materialization logic changes.

## No op-log migration

**The op log is not migrated, and cannot be.** Ops are decoded by whatever
`InnerOp` definition this build was compiled with; there is no stored op
version, no rewrite pass, and no translation layer. That splits into two rules
with opposite consequences:

- **A new field is forward-compatible.** An older build decodes the op, keeps
  the field it does not model in the entity's `unknown` map, and re-emits it —
  see [`../02-domain/schema-versioning.md`](../02-domain/schema-versioning.md).
  This is the ordinary `DOC_SCHEMA_V` bump.
- **A new op FAMILY is not.** A build handed an `InnerOp` variant it does not
  know cannot decode it at all — a new variant is not a new field — and reports
  it as an invalid remote op rather than applying it wrongly. There is no
  migration that can help, because the bytes describe an operation this binary
  has no code for.

Both [ADR-0024](../11-adr/0024-key-hierarchy.md) (`key_envelope`,
`device_revoke`) and [ADR-0025](../11-adr/0025-integration-account-entity.md)
(the `IntegrationAccount` family) add op families, so both are breaking changes
to the op vocabulary. That is acceptable pre-1.0 under
[ADR-0018](../11-adr/0018-storage-baseline-reset.md), where no older build
exists — and it is recorded here rather than discovered later, because after
1.0 the same change needs a flag day.

## Migration testing

With three migrations in the list, `crates/sunrise-storage/src/db.rs` and
`migrations.rs` assert:

- a fresh vault applies all of them and lands at `STORAGE_V`, with the tables
  the collapse was supposed to preserve and without the schema it was supposed
  to drop;
- every `storage_v` in `1..13` is refused with `StorageVPreBaseline`, and a
  `storage_v` above `STORAGE_V` with `StorageVTooNew`;
- 0014 adds `streams.sort_order` defaulting to the "never ordered" empty
  string, and its backfill hands every pre-existing row a key **in the order
  those rows were already being displayed** — asserted by replaying 0013 and
  then 0014 by hand, because a fresh vault has no rows for a backfill to touch,
  which is precisely the case a fresh-vault test cannot cover;
- migration ids strictly ascend, and `current_storage_v()` equals the
  `STORAGE_V` constant, so the list and the constant cannot drift apart.

The fixture regime below is **specified and not implemented**. No migration in
the tree ships a before/after fixture — there is no fixture directory under
`crates/sunrise-storage/` — and no CI job runs migrations over prior fixtures or
fuzzes their ordering. The 0014 backfill assertion above is the closest thing
that exists, and it is a hand-replayed unit test rather than a fixture regime.
What is written below is what should apply to 0014, 0015 and every migration
after them:

- Each migration ships with a "before" fixture (a small vault file) and an "after" expected state.
- CI runs every migration over every prior fixture to ensure forward migration is correct.
- A "fuzz" job randomly orders migrations against random data states.
