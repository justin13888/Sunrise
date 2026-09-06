---
status: living
---

# Log event catalog

Every log record carries an `ev` field naming the event. This file is the
catalogue: adding a new `ev` value requires a one-line entry here so analysts
can `grep` for meaning.

That rule is **enforced**, not aspirational.
`crates/sunrise-log/tests/event_catalog.rs` reads every `tracing::*!` call in
shipped source and fails if an event it emits is missing from this file, or if
a name in either place violates the grammar in [`logging.md`](./logging.md) §3.
The file set is not a directory walk: it comes from `cargo metadata` plus the
dep-info the compiler wrote, so a module reached by `#[path]`, a raw-identifier
module and generated code under `target/` are all covered. That test's module
doc enumerates what it does **not** see.

Do not add an event name here to satisfy that test when the emission is a
`#[cfg(test)]` unit test inside a shipped file. This catalogue is the record of
what an operator can see in production; a test that needs an event should reuse
a name already listed.

The **Implemented** tables below are what the workspace emits today. The
**Reserved** tables are names held for surfaces that do not log yet; they are
a design contract, not a claim about running code, and the catalogue test does
not require anything to emit them.

See [`logging.md`](./logging.md) for the record schema and grammar, and
[ADR-0010](../11-adr/0010-logging-strategy.md) for why the transport is
`tracing`.

---

## Implemented

### `srv` — `sunrise-server`

| Event | Level | Meaning |
|---|---|---|
| `srv.start` | info | Listener bound. Carries `bind`, `mode` (`single_tenant`/`multi_tenant`), `app_v`, and the `wire_v`/`doc_v`/`crypto_v` protocol versions — the one place per process those versions appear. |
| `srv.start.single_tenant` | warn | Self-host mode: every connection maps to one account. Loopback only. |
| `srv.start.metrics_withheld` | warn | `/metrics` was not mounted because the listener is not loopback; `bind`. The operation is absent from the OpenAPI description too, so the document does not advertise a surface this deployment refuses to serve. The operator surfaces are loopback-only per [`../06-server/overview.md`](../06-server/overview.md), so a public bind serves `404` there. Answers "why does my scrape 404". |
| `srv.start.refused` | error | Config could not be resolved, read, parsed, or validated; the process is exiting 78 (`EX_CONFIG`) rather than serving. |
| `srv.start.failed` | error | The listener could not bind; `bind`, `cause`. Distinct from `srv.start.refused`: the config was fine and the address was not available. |
| `srv.stop` | info | The server returned; listener closed. |
| `srv.stop.failed` | error | The server returned an error; `cause`. |
| `srv.req.start` | debug | HTTP request received. The span carries `method` and a templated `endpoint`. |
| `srv.req.end` | debug (warn on 5xx) | Request served; `status`, `lat_ms`, `result`. The level split is what makes a default `info` deployment show failures and nothing else. |
| `srv.auth.device_sig_rejected` | warn | A `header_sig_v2` binding was present, resolved to an active device of the account, and did not check out; `err_code` (`AUTH_DEVICE_SIG_INVALID`) and `cause`, which names which way (stale `Date`, unparseable key, bad signature). The *client* is told only `401`: the distinction is useful here and to nobody probing which devices exist. |
| `srv.store.failed` | error | A storage call failed and the request became a `500`. Carries `cause` because the operator needs it; the response never does, since a SQLite message can name columns and constraints. |
| `srv.sync.session_open` | info | A sync session was established; `account_h`. Replaces `srv.ws.connect`: ADR-0023 split the socket's one negotiation into `POST /sync/session`, so establishing a session and opening a stream are now separate events. |
| `srv.sync.negotiate_refused` | warn | `POST /sync/session` could not agree a wire version, crypto suite or required capability; `cause`. The client receives a `400` naming the same thing, unlike the socket it replaces, where a closed connection left an operator as the only party who could diagnose it. |
| `srv.sync.stream_open` | info | `GET /sync/events` opened; `account_h`, `resumed` (whether a `Last-Event-ID` was presented). |
| `srv.sync.stream_closed` | info | The event stream ended, whether by the client leaving or by the server closing it. `account_h`. |
| `srv.sync.subscribe` | debug | The session's stream set was replaced; `n_streams`. |
| `srv.sync.device_revoked` | warn | The session's device is no longer an active row on its account; the stream is closed with `AUTH_DEVICE_REVOKED`. `account_h`. Distinct from `srv.sync.token_expired` on purpose: that one means "renew and reconnect", this one means "access was withdrawn, ask the user". |
| `srv.sync.token_expired` | warn | The session's bearer passed its `exp`; the stream is closed with `AUTH_TOKEN_EXPIRED`. `account_h`. Answers "why did a working client drop hourly". |
| `srv.sync.refreshed` | debug | A refresh verified; the session's deadline moved out with no reconnect. `account_h`. |
| `srv.sync.refresh_rejected` | warn | A refresh token failed verification; `account_h`, `cause`. The session keeps its current credential — this is recoverable. Never the token. |
| `srv.sync.refresh_identity_mismatch` | warn | A refresh token verified but names a different principal or device than the session; the session is ended. `account_h`. A session handed another user's token is not a mistake to keep serving. |
| `srv.relay.fanout` | debug | `OpBatch` republished to a channel; `stream_h`, `n_bytes`. The relay never decrypts, so shape is all it can report. |
| `srv.relay.batch_duplicate` | debug | A batch this channel already holds arrived again, so nothing was stored and nothing was republished; `stream_h`, `batch_id`, `first_seen_ms`. The ack carries the stored `first_seen_ms`. Debug rather than warn: a re-send is what a reconnect is *supposed* to do — the client's counter restarts per session, so it cannot know which batches landed. |
| `srv.relay.append_failed` | error | The durable op log rejected a write, so the batch is not acked; `stream_h`, `err_code`, `cause`. The client keeps the op and retries — the one failure that must never be answered with an `Ack`. |
| `srv.relay.replay_failed` | error | The durable op log could not be read, so the session ends without a `CaughtUp`; `stream_h`, `err_code`, `cause`. Never followed by a completeness claim the server cannot back. |
| `srv.relay.cursor_gap` | warn | A subscriber's cursor for a device is below what the ring still holds, so the ops between are gone; `stream_h`, `device_h`, `cursor`, `evicted_through`. Recoverable but never retryable — re-subscribing cannot reproduce them. |

### `db` — `sunrise-storage`

| Event | Level | Meaning |
|---|---|---|
| `db.migrate.start` | info | Schema work beginning; `from_v`, `to_v`, `mode` (`fresh`/`upgrade`). |
| `db.migrate.ok` | info | Schema at `to_v`. |
| `db.migrate.failed` | error | A migration statement failed; `from_v`, `to_v`, `err_code`, `cause`. The one storage failure that leaves a user unable to open a vault at all. |
| `db.migrate.refused` | error | The vault predates the `BASELINE_STORAGE_V = 13` baseline (ADR-0018) and cannot be upgraded; `from_v`, `to_v`, `err_code`. Terminal: the remedy is a fresh vault. |

### `sync` — `sunrise-core::sync_driver`, `sunrise-cli::livesync`

| Event | Level | Meaning |
|---|---|---|
| `sync.session.opening` | info | Sync driver started against a relay; `relay` (host only). |
| `sync.session.opened` | info | Every subscribed stream caught up and the outbox drained — the driver is `Live`; `n_streams`. |
| `sync.session.closed` | info | Session ended; `result` distinguishes a clean shutdown from a drop. |
| `sync.session.error` | warn | Connect or start failed; `err_code`, `cause`. Answers "why is my client not syncing". |
| `sync.session.off` | info | No relay configured; running offline. |
| `sync.credential.renewed` | debug | A renewed bearer is being sent to the relay in a `0x12 RefreshToken` frame, on the live session. |
| `sync.credential.renewed.deferred` | debug | A renewal landed but the relay did not negotiate `SrvTokenRefresh`; the new bearer waits for the next reconnect. |
| `sync.credential.accepted` | debug | The relay acknowledged the refreshed bearer (`0x13`); `expires_at_ms` is the deadline the relay adopted, which is authoritative over the client's own reading of `exp`. |
| `sync.backoff` | debug | Waiting before reconnect; `attempt`, `delay_ms`. A reconnect storm is visible as a run of these. |
| `sync.op.retransmit` | debug | An op batch went unacked and was sent again; `batch_id`, `attempt`, `n_ops`. A run of these on one `batch_id` is a link that stays up but is not carrying our ops. |
| `sync.gap` | warn | The relay reported ops it can no longer supply; the session latches `Degraded` and stops claiming to be up to date. `cause` carries the relay's diagnostic. Unlike every other sync warning this one is not retryable — re-subscribing cannot produce the ops. |
| `sync.loss_evidence` | debug | The session saw evidence the link is dropping data and pulled its resync forward; `cause` is `retransmit`, `undecodable_frame`, or `corrupt_op`. Nothing acks an inbound frame, so this is the only trace inbound loss leaves. |

### `ui` — `sunrise-cli`

| Event | Level | Meaning |
|---|---|---|
| `ui.start` | info | Client starting; `app_v` and the protocol versions. Emitted by `sunrise-cli`; the macOS app will emit the same name. |
| `ui.pair.payload_exported` | info/warn | Dev pairing-payload export step of the two-vault demo; `result`. Never the path. |
| `ui.pair.payload_adopted` | info/warn | Dev pairing-payload adoption step, at `Core::open`; `result`, `err_code` on failure. |

---

## Reserved

These names are held for surfaces that do not emit yet. They stay here because
the shape of what those surfaces should say has been decided; nothing enforces
them until code uses them.

### `core` (sunrise-core)

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
| `core.device.cert_rejected` | warn | A `device_cert` op was not applied; `reason` (`undecodable` / `names_another_device` / `binding`), `sender_h`, and `subject_h` when the cert names someone else. The delivery itself still succeeds — the envelope verified and the sender is a member — so without this the row keeps a NULL `d_d_pub`, the device is never a `key_envelope` recipient, and its peers' ops park forever with nothing said. |
| `core.device.revoke_refused` | warn | A `device_revoke` was not applied to the register; `reason` (`self`), `sender_h`. A device must not move its own cut: the register is last-writer-wins, so its own op would win and undo somebody else's revocation of it. |
| `core.key.epoch_refused` | warn | A `key_envelope` named an epoch more than `MAX_EPOCH_LEAP` above this vault's live one and was not absorbed; `stream_h`, `epoch`, `live`. `MAX(epoch)` is what makes a key live, so absorbing an absurd one would strand rotation and redirect this device's own writes. |
| `core.op.deferred_evicted` | warn | The parked-op buffer hit its cap and the oldest rows were dropped; `n`, `stream_h`. Ordinary traffic never reaches it: a legitimate park is released by the very next absorbed key. |

### `crypto` (sunrise-crypto)

Deliberately unimplemented in v1. Per-envelope logging in the crypto path is
the highest-risk, lowest-yield instrumentation in the workspace: it sits in the
hot loop, and every field it could add is either a constant or one refactor
away from being a plaintext handle.

| Event | Level | Meaning |
|---|---|---|
| `crypto.kdf.start` | debug | KDF run started; `lat_ms` on the matching ok. |
| `crypto.kdf.ok` | debug | KDF run completed. |
| `crypto.envelope.encrypt` | debug | Op envelope sealed. |
| `crypto.envelope.decrypt` | debug | Op envelope opened. |
| `crypto.envelope.reject` | warn | Envelope rejected; `err_code` carries the reason. |
| `crypto.rotate.start` | info | Key rotation started. |
| `crypto.rotate.complete` | info | Rotation finished. |
| `crypto.sig.verify.failed` | warn | Signature verification failed. |

### `db` / `blob` / `compact` (sunrise-storage, beyond migrations)

| Event | Level | Meaning |
|---|---|---|
| `db.tx.commit` | debug | Transaction committed; `lat_ms`. |
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

### `sync` (frame-level)

| Event | Level | Meaning |
|---|---|---|
| `sync.frame.recv` | debug | Wire frame received. |
| `sync.frame.send` | debug | Wire frame sent. |
| `sync.batch.applied` | debug | Op batch applied. |
| `sync.batch.rejected` | warn | Op batch rejected. |
| `sync.snapshot.req` | debug | Snapshot requested. |
| `sync.snapshot.applied` | debug | Snapshot applied. |
| `sync.transport.fallback` | warn | Reserved for a future fallback transport; unused in v1. There is one transport — an SSE stream downstream and typed POSTs upstream ([ADR-0023](../11-adr/0023-sse-sync-transport.md), which supersedes ADR-0005 and the WebSocket-plus-long-poll pair it specified) — and nothing falls back off it. |

### `srv` (auth outcome and push)

Held names, none of them emitted. `srv.auth.ok` and `srv.auth.rejected` sat in
the Implemented table for the whole of v1 while nothing in
`crates/sunrise-server/src` produced either: the bearer path logs nothing on
success, and a refusal is visible as the `srv.req.end` record's status. They are
worth keeping as names — an operator asking "who authenticated" is a real
question — but not as a claim about running code.

`srv.quota.warning` and `srv.quota.exceeded` are **deleted rather than
reserved**: ADR-0027 takes per-account quotas out of v1, and the codes they
would have carried are gone from the registry with their ids burned.

The push events are unimplemented because the feature is: the only provider is
`LoggingProvider`, which increments a metric.

| Event | Level | Meaning |
|---|---|---|
| `srv.auth.ok` | debug | Bearer accepted and account resolved; `account_h`, `tier`. Never the token. |
| `srv.auth.rejected` | warn | Bearer rejected or account not resolved; `err_code`, `status`. Never the token. |
| `srv.push.send.ok` | info | Push delivered; `provider`, `n_devices`. |
| `srv.push.send.failed` | warn | Push delivery failed. |

### `ui` (interaction)

Per-interaction UI logging is deliberately absent. `ui.input.lat` and
`ui.action` would fire on the keystroke path of a GUI whose log is a file on
the user's own disk; the cost is real and the debugging value is close to zero,
because every decision a keystroke makes is either a `Command` the core already
logs or a pure function that unit-tests without any of it.

| Event | Level | Meaning |
|---|---|---|
| `ui.view.open` | info | View opened (no entity content). |
| `ui.view.close` | info | View closed. |
| `ui.action` | info | User action; `action_kind`. |
| `ui.error.shown` | warn | User-facing error toast displayed. |
| `ui.input.lat` | debug | Keystroke-to-paint latency sample. |

### `int` (sunrise-integrations)

No provider is wired in v1.

| Event | Level | Meaning |
|---|---|---|
| `int.run.start` | info | Integration sync run begin. |
| `int.run.ok` | info | Integration sync run complete. |
| `int.run.failed` | warn | Integration sync run failed. |
| `int.auth.refresh` | info | OAuth refresh. |
| `int.auth.expired` | warn | OAuth refresh failed; user re-auth needed. |
| `int.rate_limited` | warn | External API returned 429. |
