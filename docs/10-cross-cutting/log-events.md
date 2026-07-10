# Log event catalog

`status: living`

This file catalogs every `ev` value emitted across the workspace. Adding a
new event name requires a one-line entry here so analysts can `grep` for
meaning. The per-package ev-catalog snapshot test references this list.

See [`logging.md`](./logging.md)
for the structured-record schema and grammar.

---

## `log` (sunrise-log self events)

| Event | Level | Meaning |
|---|---|---|
| `log.throttled` | warn | Token-bucket rate limit dropped one or more records for an `(ev, lv)` pair. `ctx.n_dropped` carries the count; `ctx.op_kind` is the throttled event name. |
| `log.bootstrap.late` | error | A log call landed before `sunrise_log::init` ran. (Reserved; not yet emitted by this crate.) |

## `core` (sunrise-core; reserved)

| Event | Level | Meaning |
|---|---|---|
| `core.open.start` | info | Vault open lifecycle. |
| `core.open.ok` | info | Vault opened. |
| `core.open.failed` | error | Vault open failed. |
| `core.unlock.attempt` | debug | Unlock material received. |
| `core.unlock.ok` | info | Unlock succeeded. |
| `core.unlock.failed` | warn | Unlock failed; counts attempts, never logs the passphrase. |
| `core.submit.queued` | debug | Command queued. |
| `core.submit.applied` | debug | Command applied. |
| `core.submit.rejected` | warn | Command rejected. |
| `core.query.slow` | warn | Read query exceeded performance budgets p99. |
| `core.shutdown.start` | info | Shutdown initiated. |
| `core.shutdown.ok` | info | Shutdown complete. |

## `crypto` (sunrise-crypto; reserved)

| Event | Level | Meaning |
|---|---|---|
| `crypto.kdf.start` | debug | KDF run started; `ctx.lat_ms` on the matching ok. |
| `crypto.kdf.ok` | debug | KDF run completed. |
| `crypto.envelope.encrypt` | debug | Op envelope sealed. |
| `crypto.envelope.decrypt` | debug | Op envelope opened. |
| `crypto.envelope.reject` | warn | Envelope rejected; `err.code` carries reason (e.g. `CRYPTO_AAD_MISMATCH`, `CRYPTO_NON_CANONICAL_CBOR`). |
| `crypto.rotate.start` | info | Stream/device/identity key rotation started. |
| `crypto.rotate.complete` | info | Rotation finished. |
| `crypto.sig.verify.failed` | warn | Signature verification failed. |

## `db` (sunrise-storage; reserved)

| Event | Level | Meaning |
|---|---|---|
| `db.migrate.start` | info | Schema migration begin; `ctx.from_v`/`to_v`. |
| `db.migrate.ok` | info | Schema migration complete. |
| `db.migrate.failed` | error | Schema migration failed. |
| `db.tx.commit` | debug | Transaction committed; `ctx.lat_ms`. |
| `db.tx.rollback` | debug | Transaction rolled back. |
| `db.query.slow` | warn | Query exceeded budget. |
| `blob.upload.start` | debug | Blob upload begin. |
| `blob.upload.ok` | debug | Blob upload complete. |
| `blob.upload.failed` | warn | Blob upload failed. |
| `blob.fetch.start` | debug | Blob fetch begin. |
| `blob.fetch.ok` | debug | Blob fetch complete. |
| `blob.fetch.failed` | warn | Blob fetch failed. |
| `compact.start` | info | Compaction begin. |
| `compact.ok` | info | Compaction complete. |
| `compact.failed` | error | Compaction failed. |

## `sync` (sunrise-sync; reserved)

| Event | Level | Meaning |
|---|---|---|
| `sync.session.opening` | debug | Sync session opening. |
| `sync.session.opened` | info | Sync session live. |
| `sync.session.closed` | info | Sync session closed cleanly. |
| `sync.session.error` | warn | Sync session terminated with error. |
| `sync.frame.recv` | debug | Wire frame received. |
| `sync.frame.send` | debug | Wire frame sent. |
| `sync.batch.applied` | debug | Op batch applied. |
| `sync.batch.rejected` | warn | Op batch rejected. |
| `sync.snapshot.req` | debug | Snapshot requested. |
| `sync.snapshot.applied` | debug | Snapshot applied. |
| `sync.transport.fallback` | warn | Transport fell back from T-1 to T-2 to T-3. |
| `sync.backoff` | warn | Entered exponential backoff. |

## `srv` (sunrise-server; reserved)

| Event | Level | Meaning |
|---|---|---|
| `srv.req.start` | debug | HTTP request begin. |
| `srv.req.end` | debug | HTTP request end; `ctx.lat_ms`, `ctx.status`, `ctx.endpoint`. |
| `srv.ws.connect` | info | WebSocket connection opened. |
| `srv.ws.disconnect` | info | WebSocket connection closed. |
| `srv.auth.ok` | debug | OIDC auth accepted. |
| `srv.auth.rejected` | warn | OIDC auth rejected. |
| `srv.quota.warning` | warn | Quota soft cap reached. |
| `srv.quota.exceeded` | warn | Quota hard cap exceeded. |
| `srv.relay.fanout` | debug | Op fanned out to peers. |
| `srv.push.send.ok` | info | Push delivered. |
| `srv.push.send.failed` | warn | Push delivery failed. |

## `ui` (clients; reserved)

| Event | Level | Meaning |
|---|---|---|
| `ui.view.open` | info | View opened (no entity content). |
| `ui.view.close` | info | View closed. |
| `ui.action` | info | User action; `ctx.action_kind`. |
| `ui.error.shown` | warn | User-facing error toast displayed. |
| `ui.input.lat` | debug | Keystroke-to-paint latency sample. |

## `int` (integrations; reserved)

| Event | Level | Meaning |
|---|---|---|
| `int.run.start` | info | Integration sync run begin. |
| `int.run.ok` | info | Integration sync run complete. |
| `int.run.failed` | warn | Integration sync run failed. |
| `int.auth.refresh` | info | OAuth refresh. |
| `int.auth.expired` | warn | OAuth refresh failed; user re-auth needed. |
| `int.rate_limited` | warn | External API returned 429. |
