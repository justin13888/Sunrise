# Old-vault fixtures

Encrypted SQLCipher vaults written at historical `STORAGE_V`s, so that the
migration chain can be run against real old data rather than against a schema
this build just created. Read
[`../src/vault_fixtures.rs`](../src/vault_fixtures.rs) — it generates these
files, states their contents, and asserts what survives.

| File | `storage_v` | Why this version |
|---|---:|---|
| `vault_storage_v13.db` | 13 | The oldest supported version ([ADR-0018](../../../docs/11-adr/0018-storage-baseline-reset.md)'s baseline). Exercises 0014's `sort_order` backfill and everything 0017 moves. |
| `vault_storage_v17.db` | 17 | 0019 reads an `identity` row, and a vault that started at 13 has none — 0017 creates that table empty. Without this file, the migration that can blank the only copy of `ID_D_priv` in existence is never run against a vault that has one. |

## The key is committed, deliberately

Both files are keyed with a vault root of **thirty-two `0x7e` bytes**, spelled
out as `FIXTURE_VAULT_ROOT` in `../src/vault_fixtures.rs`.

That is not a leak and it is not an oversight. A checked-in encrypted fixture
is worthless without its key, and the fixture is worthless unencrypted — the
whole point is that it goes through the same `PRAGMA key` path a real vault
does. Nothing is behind it: the rows are invented, no user, device or account
has ever been keyed with it, and it has never been anywhere but this
repository. Please do not "fix" it by rotating it or moving it to a secret
store; the replacement would be exactly as public, and the fixtures would have
to be regenerated to no benefit.

## Regenerating

```
mise run storage-fixtures
```

Rarely, and for a reason. A fixture regenerated at today's schema is no longer
old, and the chain it exists to exercise then runs over nothing — the test
`the_committed_v13_fixture_is_a_sealed_vault_stamped_at_the_baseline` fails
when that happens, which is what it is for.

The bytes are not reproducible: SQLCipher writes a random salt into the file
header, so every regeneration produces a different file with the same contents.
Expect a whole-file diff, and review the source change rather than the binary.
