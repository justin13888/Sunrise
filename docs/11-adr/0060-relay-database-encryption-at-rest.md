# 0060 — The relay database is SQLCipher-encrypted under an operator's key file, and backed up online through SQLite's backup API

**Status:** accepted

**Built by** [#360](https://github.com/justin13888/Sunrise/issues/360).

**Depends on** the versioned schema and WAL mode of
[`../06-server/relay-and-blob-storage.md`](../06-server/relay-and-blob-storage.md)
§Schema versions and pragmas, and on the `sunrise-server admin` CLI
([`../06-server/self-hosting.md`](../06-server/self-hosting.md) §Admin CLI).
**Supersedes** the per-token ChaCha20-Poly1305 scheme
[`../06-server/push-notifications.md`](../06-server/push-notifications.md)
specified for push tokens and never built.

## Context

The self-host relay keeps its whole state in one SQLite file:
accounts with their emails and OIDC subjects, devices with their nicknames,
push tokens, and the relay log with its routing ids
([`../06-server/relay-and-blob-storage.md`](../06-server/relay-and-blob-storage.md)
§What the relay database actually holds). `rusqlite` is built with
`bundled-sqlcipher` for the whole workspace, but only the client vault applied
a key, so the relay file was plain SQLite, and so was every backup of it.

The only supported backup also needed care. `admin backup` copied the database
with `VACUUM INTO`, which under SQLCipher writes a file under whatever key the
target is attached with, and the documented alternative was to stop the relay
and `tar` the data dir.

Four things needed settling before code: the mechanism, where the key comes
from, how it rotates, and what a new or an existing install gets by default.

## Decision

### 1. The whole file, with SQLCipher; no field-level encryption on top

The relay database is encrypted as a whole with SQLCipher, keyed with a 32-byte
raw key (`PRAGMA key = "x'…'"`), so SQLCipher runs no passphrase KDF. Every
page is encrypted, the header included, so the file starts with its random
salt and not `SQLite format 3\0`.

No column is additionally encrypted, and that includes `accounts.email` and
`push_tokens.token`. The threat a field-level layer would add protection
against is someone who holds the file **and** the key, and the only party
holding both is the running relay, which must decrypt those fields to use them:
it sends the email to nobody, but it hands every push token to APNs. A process
that can read the key can read the field key beside it. What remains is the
file, or a backup of it, taken without the key, and whole-file encryption
covers all of that, including the routing ids and nicknames a field list would
have left out.

The ChaCha20-Poly1305 push-token scheme in `push-notifications.md` was the
earlier answer to the same threat: a key file excluded from backups, so a
leaked backup leaks no tokens. The database key file is that key file, for the
whole database, so the scheme is superseded rather than built.

### 2. The key is a file the operator names, outside the data dir

```toml
[storage]
encrypt  = true
key_file = "/etc/sunrise/db.key"   # 64 hex digits; openssl rand -hex 32
```

The key file holds 64 hex digits, surrounding whitespace allowed. The relay
refuses to start, with exit 78 (`EX_CONFIG`) and a typed `StoreError`, when:

| Condition | Refusal |
|---|---|
| `encrypt = true` without `key_file`, or `key_file` without `encrypt = true` | `KeyConfig` |
| The key file is missing, unreadable, or not 64 hex digits | `KeyFile` |
| The group or others can read it (any of mode `0o077`) | `KeyPermissions` |
| It lies inside the data dir, compared canonically | `KeyInDataDir` |
| The database is encrypted and encryption is off | `KeyRequired` |
| The key does not open the database | `WrongKey` |

The key is never derived from anything stored in the data dir, and may not be
stored there, so a copy of the data dir is never also a copy of its key.

An environment variable is not a key source. A process's environment is
readable through `/proc/<pid>/environ` by its own user, is inherited by every
child it spawns, and shows in `docker inspect`. The orchestrators that offer
secrets (Docker, Kubernetes, systemd's `LoadCredential=`) all offer them as
mounted files, which is the source this decision takes.

A KMS is not built. The key is resolved in one place,
`DbKey::for_storage` in `crates/sunrise-server/src/store/cipher.rs`, and a
KMS source is a second branch there that returns the same 32 bytes. The store
takes a `DbKey` and nothing about where it came from, so adding one changes
neither the store nor the file format.

### 3. Rotation is `admin rekey`, with the relay stopped

`sunrise-server admin rekey <new_key_file>` checks the new file under the same
rules as `key_file`, then re-encrypts every page with SQLCipher's
`PRAGMA rekey`. It runs out of WAL mode, and switching out of WAL is refused
while another connection holds the file, which is how it proves the relay is
stopped: a relay still holding the old key would fail on every page it read
afterwards. The command does not edit the config. The procedure is: stop the
relay, write the new key file, run `admin rekey`, point `key_file` at the new
file, start, and then destroy the old key, since every backup taken before the
rotation still needs it.

### 4. Encryption is off by default, and turning it on migrates once

`encrypt` defaults to `false`. A config that names no key cannot encrypt, and
a relay that generated its own key would have to keep it somewhere the
operator never chose, which, for a default install, is the data dir. An
operator sets the two keys; the configuration reference in
[`../06-server/self-hosting.md`](../06-server/self-hosting.md) shows them and
recommends them for every deployment that keeps real accounts.

An install that turns encryption on is migrated by the first process that
opens it under the key, the relay or an `admin` command, before any other step
of the open:

1. the plaintext file is refused if a newer release wrote it, then taken out of
   WAL mode, which checkpoints every committed write into it and fails while
   another process holds the file;
2. `sqlcipher_export` writes every table, index and row into
   `sunrise.db.encrypting` under the key, and the schema version after it;
3. the plaintext file is hard-linked (copied where the filesystem cannot) to
   `sunrise.db.pre-encryption`;
4. the encrypted file is renamed over `sunrise.db`, the one atomic step.

A crash before step 4 leaves the plaintext database, and the next start
retries. The migration is logged as `srv.store.encrypted`, and `admin doctor`
fails its `encryption` check while `sunrise.db.pre-encryption` exists: it is
the plaintext the migration was for, kept only so the operator can verify the
result before deleting it.

The step is not a numbered schema migration. Those run inside one transaction
on the open connection and share their numbering with a future Postgres
backend, and a file-format change is neither: it replaces the file, and
Postgres has its own at-rest story. Decryption is not offered; an encrypted
database stays encrypted.

### 5. Backups are online, through SQLite's backup API, under the same key

`sunrise-server admin backup <dest_dir>` copies the database with SQLite's
online backup API (`rusqlite`'s `backup` feature, enabled for
`sunrise-server` only), into a new file keyed with the live database's key, in
a **single step over every page**. One step reads one snapshot, so the copy is
one committed instant; in WAL mode a reader never blocks a writer, so the
relay's appends proceed throughout. A copy taken in page ranges would release
the snapshot between ranges, and the backup API restarts from the first page
whenever another process writes the source in between, so under a steady
append load it would not finish. The cost of one step is WAL growth for the
copy's duration, since a checkpoint cannot pass a snapshot still being read.

The blob tree follows the database, manifests last: the manifests present are
listed first, every other file is copied, then the listed manifests are copied
if they still exist. A manifest is written after its chunks and removed before
them — by blob collection, account erasure, and the orphan sweep alike, each
of which removes an account's `manifests/` before the rest of its tree — so
every manifest in the backup names chunks that are in it. The blobs a backup
omits were finalized after its database copy, so no op in it names them;
collected during it, so their tombstones are in it; or belong to an account
erased during it, which the backup still holds and a restore brings back with
those blobs missing, reading as never uploaded.

Restore is the reverse: stop the relay, replace `sunrise.db` and `blobs/` with
the backup's, keep the same `key_file`, start. A client whose cursor the
backup covers resumes from it with no gap. Frames the relay acknowledged after
the backup are not in it: the backup is a recovery point, and the interval
between backups is the relay history an operator accepts losing.

### 6. Blob files are out of scope

Blob chunks are ciphertext the client sealed under a per-blob key the relay
never holds, so encrypting them again would protect nothing. Their paths are a
BLAKE3 hash of the account id and the blob id, itself a hash of the
ciphertext. The blob tree
is not encrypted by this decision, and a backup copies it as it is.

## Alternatives considered

- **Field-level AEAD for `email` and `push_tokens.token` only.** Leaves the
  nicknames, platforms, OIDC subjects and routing ids readable, and protects
  against nothing whole-file encryption does not (§1).
- **Default on, with a generated key.** The key would land in the data dir or
  a path the operator never chose, and an upgrade would encrypt a working
  install without the operator holding the key to its backups.
- **`VACUUM INTO` for the backup.** Already in place, and consistent, but it
  is not the backup API the issue names, it rewrites the file rather than
  copying pages, and its encryption depends on how the target is attached.
- **A stepped backup with a pause between page ranges.** Restarts under every
  concurrent write from another process (§5).
- **Litestream as the backup.** It opens the database through its own SQLite
  build, which has no SQLCipher codec, so it cannot run against an encrypted
  install. It stays a documented option for a plaintext one.

## Consequences

- A stolen disk, data-dir copy or backup without the key file reveals nothing
  of the relay database. The relay still reads everything at runtime: this
  protects data at rest, not from the operator
  ([`../01-architecture/trust-and-server-role.md`](../01-architecture/trust-and-server-role.md)).
- Losing the key file loses the database and every backup of it. Operators
  back the key up separately from the data dir, which `self-hosting.md` says.
- `sqlite3` on the host cannot read an encrypted relay database; the
  `sqlcipher` shell with the key can.
- Continuous replication of an encrypted install is a scheduled `admin backup`,
  not WAL shipping.
- Every open reads page 1 once more than before, to turn a wrong key into
  `WrongKey` rather than an error from the first statement that happened to
  run.

## Revisit if

1. A managed or multi-node deployment needs a KMS-held key: add the source in
   `DbKey::for_storage` (§2).
2. The relay starts holding a field whose disclosure to the running process
   matters, which whole-file encryption cannot help with.
3. A replication tool gains SQLCipher support, which would make WAL shipping
   available to encrypted installs.
