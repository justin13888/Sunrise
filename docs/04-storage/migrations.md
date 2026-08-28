---
status: accepted
---

# Migrations

> **Pre-1.0 baseline.** As of `STORAGE_V = 13` the migration *list* is a single
> file, `crates/sunrise-storage/migrations/0013_baseline.sql`. Migrations
> 0001–0012 were collapsed into it and deleted, and a vault stamped
> `0 < storage_v < 13` is **refused** (`DbError::StorageVPreBaseline` →
> `STORAGE_V_TOO_OLD`) rather than upgraded. Nothing below changes: the runner,
> the ordering rule, the single-transaction guarantee, and the append-only rule
> for new migrations are all still in force, and the next schema change appends
> file 0014 exactly as it always would have. See
> [ADR-0018](../11-adr/0018-storage-baseline-reset.md) for why this was done
> once, and why it does not happen again after 1.0.

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

### Minimum-version discovery

The "minimum `DOC_SCHEMA_V` across all known devices" is determined from device-cursor heartbeats. Each cursor op carries `doc_schema_v_max` (the highest version that device can write).

- After all active devices' cursors report `doc_schema_v_max ≥ N+1`, a 7-day grace begins.
- After grace, the **server's `doc_schema_floor`** advances to `N+1`, refusing inbound ops with `doc_schema_v < N+1`.
- A device that has not heartbeat in 30 days is considered abandoned and excluded from the minimum.

## Materialized state rebuild

If a migration is irrecoverable (a corrupt index, a bug), the user can trigger "rebuild from log":

1. Wipe materialized tables.
2. Re-apply ops in causal order.
3. Rebuild FTS index.

This is an internal capability used by the app on first launch after major-version upgrades when materialization logic changes.

## Migration testing

Today, with one migration in the list, `crates/sunrise-storage/src/db.rs`
asserts the two facts that exist to assert:

- a fresh vault applies the baseline and lands at `STORAGE_V`, with the tables
  the collapse was supposed to preserve and without the schema it was supposed
  to drop;
- every `storage_v` in `1..13` is refused with `StorageVPreBaseline`, and a
  `storage_v` above `STORAGE_V` with `StorageVTooNew`.

When migration 0014 lands, the fixture regime below applies to it and to every
migration after it:

- Each migration ships with a "before" fixture (a small vault file) and an "after" expected state.
- CI runs every migration over every prior fixture to ensure forward migration is correct.
- A "fuzz" job randomly orders migrations against random data states.
