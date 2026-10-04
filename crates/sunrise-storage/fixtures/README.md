# Old-vault fixtures

Encrypted SQLCipher vaults written at historical `STORAGE_V`s, so that the
migration chain can be run against real old data rather than against a schema
this build just created. Read
[`../src/vault_fixtures.rs`](../src/vault_fixtures.rs) — it generates these
files, states their contents, and asserts what survives.

There is one file for every `storage_v` from 13 to the version before the
current `STORAGE_V`, named `vault_storage_v<N>.db`. The test
`every_migration_has_a_fixture_at_the_version_before_it` fails when a
migration is appended without the fixture at the version before it, and
`every_fixture_keeps_every_row_through_every_later_migration` opens each one
with `Db::open` and checks every row it held is still there.

Two of them were written with seeds of their own:

| File | `storage_v` | Why this version |
|---|---:|---|
| `vault_storage_v13.db` | 13 | The oldest supported version ([ADR-0018](../../../docs/11-adr/0018-storage-baseline-reset.md)'s baseline). Exercises 0014's `sort_order` backfill and everything 0017 moves. |
| `vault_storage_v17.db` | 17 | 0019 reads an `identity` row, and a vault that started at 13 has none — 0017 creates that table empty. Without this file, the migration that can blank the only copy of `ID_D_priv` in existence is never run against a vault that has one. |

Every other file holds the rows in `../src/vault_fixtures/seed.rs`: at least
one in every table that exists at its version, with a value in nearly every
column.

## The key is committed, deliberately

Every file is keyed with a vault root of **thirty-two `0x7e` bytes**, spelled
out as `FIXTURE_VAULT_ROOT` in `../src/vault_fixtures.rs`.

That is not a leak and it is not an oversight. A checked-in encrypted fixture
is worthless without its key, and the fixture is worthless unencrypted — the
whole point is that it goes through the same `PRAGMA key` path a real vault
does. Nothing is behind it: the rows are invented, no user, device or account
has ever been keyed with it, and it has never been anywhere but this
repository. Please do not "fix" it by rotating it or moving it to a secret
store; the replacement would be exactly as public, and the fixtures would have
to be regenerated to no benefit.

## Writing one

```
mise run storage-fixtures
```

It writes only the fixtures that are missing, which after appending migration
`N` is the one at `N - 1`, and never rewrites a committed file. A fixture
rewritten at today's schema is no longer old, and the chain it exists to
exercise then runs over nothing. To rewrite one on purpose, delete it first.
`every_committed_fixture_is_sealed_and_stamped_at_its_version` fails if a file
is stamped at any version but the one its name says.

The bytes are not reproducible: SQLCipher writes a random salt into the file
header, so every write produces a different file with the same contents.
Review the source change rather than the binary.
