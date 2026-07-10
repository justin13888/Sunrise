---
status: accepted
---

# Storage — Overview

Each device persists three things:

1. **Op log** — the canonical, append-only history of all ops the device has produced or received.
2. **Materialized state** — denormalized current view of the domain entities, kept in sync with the op log; used for fast queries.
3. **Blob store** — encrypted attachment chunks.

These are bundled into a single **Vault** directory.

```
$VAULT/
├── meta.json           # vault id, schema version, encryption metadata, no secrets
├── vault.db            # SQLite (SQLCipher), holds ops + materialized state + FTS index
├── blobs/
│   ├── 0a/0a3f.../     # content-addressed encrypted chunks
│   └── …
└── tmp/                # staging area for incoming sync batches
```

## Why one SQLite for both ops and state

- Atomic transactions across op insertion and materialized state update — no two-store reconciliation.
- One file to back up, restore, or move.
- SQLCipher provides whole-database encryption with low overhead.

## Why encrypted-at-rest *and* op-level encryption?

The op envelope is encrypted (per [`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md)) for transport. The vault DB is *also* encrypted at rest because:

- The materialized state (decrypted entities) is in the DB.
- We don't want a stolen-disk image to reveal anything without the OS-keystore-protected wrapping key.

## Vault paths per platform

| Platform | Vault root |
|---|---|
| macOS | `~/Library/Application Support/Sunrise/<account>/` |
| Linux | `${XDG_DATA_HOME:-~/.local/share}/sunrise/<account>/` |
| Windows | `%LOCALAPPDATA%\Sunrise\<account>\` |
| iOS | App container `Library/Sunrise/` (excluded from iCloud, included in iOS device backup if user opts in) |
| Android | `Context.filesDir/sunrise/<account>/` (private) |
| Web | OPFS root + per-origin IndexedDB; same logical structure |
| TUI | `${XDG_DATA_HOME:-~/.local/share}/sunrise/<account>/` (shared with Linux desktop client when present) |

## Multi-account

A single device may host multiple accounts (work + personal). Each account is a separate Vault with its own keys.

## Sub-specs

| Spec | Topic |
|---|---|
| [`local-database.md`](./local-database.md) | SQLite schema for materialized state |
| [`op-log.md`](./op-log.md) | Op log table layout and access patterns |
| [`blob-store.md`](./blob-store.md) | Encrypted blob layout |
| [`compaction.md`](./compaction.md) | When and how the op log shrinks |
| [`migrations.md`](./migrations.md) | Migration framework |
