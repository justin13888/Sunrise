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
| `srv.auth.step_up_required` | warn | `GET /api/v1/accounts/me/recovery_blob` was refused because the bearer's authentication is not fresh or strong enough; `account_h` and `reason` (`auth_time_missing`, `auth_time_stale`, `acr_missing`, `acr_rejected`, `amr_rejected`). The client is told only `403 AUTH_STEP_UP_REQUIRED`: which requirement failed is what an operator needs to tell "my IdP omits `auth_time`" from "my `acr` list names a value it never emits", and those look identical from outside. |
| `srv.store.failed` | error | A storage call failed and the request became a `500`. Carries `cause` because the operator needs it; the response never does, since a SQLite message can name columns and constraints. |
| `srv.sync.session_open` | info | A sync session was established; `account_h`. Replaces `srv.ws.connect`: ADR-0023 split the socket's one negotiation into `POST /sync/session`, so establishing a session and opening a stream are now separate events. |
| `srv.sync.negotiate_refused` | warn | `POST /sync/session` could not agree a wire version, crypto suite or required capability; `err_code`, `cause`. `err_code` is the refusal's own code — `SYNC_PROTOCOL_VERSION_MISMATCH`, `CRYPTO_SUITE_MISMATCH`, `DOC_SCHEMA_TOO_OLD` or `CAPABILITY_REQUIRED_MISSING` — and the client receives the same one in the `400`, unlike the socket it replaces, where a closed connection left an operator as the only party who could diagnose it. |
| `srv.sync.resume_conflict` | warn | `GET /sync/events` presented a non-zero `Last-Event-ID` on the first stream after a `Subscribe`; the request is refused with `SYNC_RESUME_CONFLICT` and no stream opens. `account_h`, `n_streams`. The id says what the client received and the cursors say what it applied, so serving either one would drop the other's claim. Reaching this means a client is not clearing its resume point on `Subscribe`. |
| `srv.sync.stream_open` | info | `GET /sync/events` opened; `account_h`, `resumed` (whether a `Last-Event-ID` was presented). `resumed` is only readable as a ratio: a step change in cold opens after a deploy is clients *losing* their resume point, which `sunrise_sync_stream_total` counts and cannot distinguish. Emitted before the stream is handed back, so it is present even for a session whose first frame never arrives. |
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
| `sync.device.revoke_relayed` | info | The relay accepted a device revocation and will no longer take that device's uploads or hold its stream; `subject_h`. The vault half of a revocation is an op the relay cannot read, so this is the second, out-of-band half, and it is the one that actually bounds the device's writes. |
| `sync.device.revoke_unknown_to_relay` | warn | The relay holds no active device row carrying this vault device id, so the intent is dropped — retrying cannot make one appear; `subject_h`. **Not a revocation.** It is also the answer for a device that registered before clients sent `vault_device_id`, which that relay is still accepting under a row this vault cannot name. |
| `sync.device.revoke_not_relayed` | warn | The relay refused a device revocation; `subject_h`, `attempt`, `cause`. **The revoked device is still accepted by the relay until this succeeds.** The intent stays queued and is retried on the next session; a rising `attempt` on the same `subject_h` means a relay that keeps saying no, which is an operator-visible problem rather than a transient one. |
| `sync.blob.uploaded` | info | An attachment's ciphertext is committed on the relay and readable by this account's other devices; `blob_h`, `n_chunks`. The moment the attachment stops being local-only. |
| `sync.blob.upload_failed` | warn | An attachment's bytes have not reached the relay; `blob_h`, `attempt`, `cause`, `retryable`. The queue row survives, and the reserved upload id with it, so the retry re-uses the same relay-side pending area. A rising `attempt` on one `blob_h` is a relay that keeps refusing rather than a flaky link. |
| `sync.blob.upload_abandoned` | warn | A queued blob's chunks are missing from this device's own store, so there is nothing to upload and the row is dropped; `blob_h`. |
| `sync.blob.upload_id_unexpected` | warn | The relay committed a blob under an address no reader will ask for, meaning the two sides hashed different ciphertext; `blob_h`. `finalize` should have refused it, so this is a relay disagreeing with its own contract. |
| `sync.blob.fetched` | info | An attachment created on another device is now readable here; `blob_h`, and `attachment_h` + `n_bytes` when it was a client that asked for it rather than the automatic drain. |
| `sync.blob.fetch_failed` | warn | An attachment's bytes could not be fetched and it stays unreadable here; `blob_h`, `cause`. A 404 is *not* this on the automatic drain — that is the ordinary "not committed yet" and is silent. On a *requested* fetch it is, because somebody is waiting: that form also carries `attachment_h`, `attempt` and a `reason` of `not_on_relay` / `rejected` / `not_stored` / `transport`, and the request stands until `sync.blob.fetch_abandoned`. |
| `sync.blob.fetch_rejected` | warn | The relay returned bytes that are not this attachment's — wrong length, a chunk that fails its AEAD tag, or a plaintext that does not match `content_hash`; `blob_h`. Nothing was written. |
| `sync.blob.fetch_not_stored` | warn | An attachment's bytes arrived, checked out, and could not be written to the local blob store; `blob_h`, `cause`. A disk problem rather than a sync one. |
| `sync.blob.fetch_requested` | info | A client asked for one attachment's bytes, past the 10 MiB auto-fetch threshold — the "Download" button in [attachments.md](../02-domain/attachments.md) §Lazy fetch; `attachment_h`, `blob_h`, `n_bytes`. The only `sync.blob.*` event a person caused directly, which is what makes it worth an `info`: everything after it is on their clock. |
| `sync.blob.fetch_cancelled` | info | A client cancelled a download; the request is marked `partial` and a later one restarts from byte 0; `attachment_h`, `blob_h`, `mode` (`discarded` when chunks from an interrupted write had to be swept, `clean` otherwise). `discarded` is the interesting one: it means a previous attempt died inside the blob store's write loop. |
| `sync.blob.fetch_abandoned` | warn | A requested download is out of attempts and is marked `partial`; `attachment_h`, `blob_h`, `reason`. The user was told. Distinct from `sync.blob.fetch_failed`, which is one attempt of many and says the request still stands. |
| `sync.session.opened` | info | Every subscribed stream caught up and the outbox drained — the driver is `Live`; `n_streams`. |
| `sync.session.closed` | info | Session ended; `result` is `ok` for a clean shutdown, `failed` for a drop or a refusal the driver retries, and `stopped` for a terminal relay `Close`. `err_code` is present when the relay named why: the code of a refused stream or of a `Close`. |
| `sync.session.error` | warn | Connect or start failed, the relay refused the event stream, or the relay closed the session; `err_code`, `cause`. Answers "why is my client not syncing". For a close, `err_kind` and `retryable` are the catalogue's for its code. `retryable = false` with `result = "stopped"` is a terminal close (`AUTH_DEVICE_REVOKED`, `AUTH_TOKEN_INVALID`, or a code this build cannot read): the driver is `Stopped` and waits for a new credential. `RELAY_STORAGE_UNAVAILABLE` and `AUTH_TOKEN_EXPIRED` are retryable, so they reconnect. |
| `sync.session.resumed` | info | A new credential arrived while the driver was `Stopped` after a terminal close, so it reconnects; `to_v` is the credential version it will present. |
| `sync.session.off` | info | No relay configured; running offline. |
| `sync.credential.marked_at_connect` | debug | A renewal that landed while the driver was disconnected was carried by this connect's own credential read, so the driver consumed it instead of re-announcing it; `to_v` is the credential version the handle was brought forward to. Emitted once the relay has answered the handshake, so it names the session whose `Authorization` header actually reached the relay — which may be carrying a renewal an earlier attempt consumed and never got to present. A steady reconnect loop with no renewal behind it is silent, so the presence of one is how an operator tells "this connect swallowed a renewal" from "no renewal happened"; read it against the session it sits in rather than as a claim about that session's own handle. |
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
| `ui.pair.offer_written` | info | `sunrise pair offer` wrote message 1; `result`. Never the path — the strings the command prints carry it, and those are not log records. The offer carries no secret, which is why this one has no failure arm worth distinguishing: a write that fails fails the command. |
| `ui.pair.request_written` | info | `sunrise pair request` minted this device's `D_S`/`D_D` and wrote message 2; `result`. |
| `ui.pair.grant_written` | info | `sunrise pair issue` certified a joining device and wrote message 3; `result`. The file it names carries the vault root and every Stream key; the event names neither. |
| `ui.pair.accepted` | info | `sunrise pair accept` adopted a certificate and opened this device's vault for the first time; `result`. |

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
| `core.device.cert_rejected` | warn | A `device_cert` op was not applied; `reason` (`undecodable` / `names_another_device` / `binding` / `rebinds_key`), `sender_h`, and `subject_h` when the cert names someone else. The delivery itself still succeeds — the envelope verified and the sender is a member — so without this the row keeps a NULL `d_d_pub`, the device is never a `key_envelope` recipient, and its peers' ops park forever with nothing said. `rebinds_key` is a cert for a device id this vault already holds that carries a **different** `D_D_pub`: a device's `D_D` is minted once and never replaced, so the column is set-once and a cert that would move it is refused whole, before the backfill that would otherwise seal every held epoch to the new key ([#281](https://github.com/justin13888/Sunrise/issues/281)). A NULL column is still filled. |
| `core.device.revoke_incomplete` | warn | A revocation rotated what it could and **could not rotate every stream**; `subject_h`, `n_streams`. The named count is of distinct `stream_id` column values that are not 16 bytes, so there was no stream to mint a new epoch for and the revoked device still holds whatever key it was last given for them. The revocation is not failed over this — the device being revoked is often the one that is gone — but it is not silent either: the same rows come back to the caller on `CommandResult::unrotated_streams`, and a client must disclose them rather than print "revoked". Same rule as `core.identity.recovery_code_invalidated` below and as the relay half in #160. |
| `core.device.revoke_refused` | warn | A `device_revoke` was stored and **not folded into the register**; `reason` (`self` / `revoked_sender`), `sender_h`, and `subject_h` for the second. `self`: a device must not move its own cut, because the register is last-writer-wins and its own op would win and undo somebody else's revocation of it. `revoked_sender`: the account had already revoked the device that wrote this op, and until [ADR-0041](../11-adr/0041-peer-side-revocation-is-a-fold.md) nothing stopped an expelled laptop expelling every other device in the account, permanently, on every replica. Neither is a refusal to record — the op is kept in `device_revoke_ops` and the register is a fold over it, so the judgement is re-taken every time another revocation lands. **When the discarded op is one this vault wrote itself, the fact also reaches the caller** and does not stop at an operator's NDJSON: `Command::RevokeDevice` reads the register back inside its own transaction and returns `CommandResult::revocation_gated`, and it does not queue the relay's half, because telling the relay to cut a device every replica still shows as current is the disclosure failure [#160](https://github.com/justin13888/Sunrise/issues/160) fixed in the other direction. Same rule as `core.device.revoke_incomplete` below. No second event is minted for that case; this row is the event. |
| `core.device.revocation_unwound` | warn | A re-fold of `device_revocations` **removed** a revocation this replica had already applied; `subject_h`. A revocation of S arriving now skips every row S wrote — whenever S dated them, because the gate reads the whole ledger and not the part of it sorting below the row — so a device S had revoked becomes current again. That is the register being a pure function of the op set rather than a ratchet ([ADR-0041](../11-adr/0041-peer-side-revocation-is-a-fold.md)), and it is **routine** rather than exotic: revocation is retroactive in this one op family, on purpose, because "before its own cut" is a date the sender chose. It is said out loud because a device list that quietly changed back is the one outcome of that design a user could be surprised by. **It does not stop at an operator's NDJSON:** the same fact comes back to the caller on `CommandResult::revocation_unwound`, which names every device the account has stopped recording a removal of, and a client must disclose it — the same rule this table states for `core.device.revoke_incomplete` and `core.device.revoke_refused`. The device's *keys* are not given back: `device_read_bounds` (migration 0028) is monotone, so the row reads `revoked: false, read_bounded: true` on every device list and that is what both clients render. The remedy is to revoke that device again from a device the account still trusts. |
| `core.key.recipient_claim_refused` | warn | A **read-bounded** device's `key_envelope` naming a **third** device as recipient was not recorded in `key_envelope_recipients`; `sender_h`, `subject_h`, `stream_h`, `epoch`. That table is what `backfill_key_envelopes` reads to decide a device has already been served, and the arm files a row on the sender's word alone because it holds no key for that ciphertext — so a claim filed on somebody else's behalf withholds a key from them. The table is a hint rather than state, so declining a row can only cause *more* key distribution and never less ([ADR-0041](../11-adr/0041-peer-side-revocation-is-a-fold.md)). **Read-bounded and not revoked**, and the difference is the reason to grep for this row rather than for the device list: since migration 0028 the gate asks `device_read_bounds`, which is monotone, and not `device_revocations`, which a fold can take a row back out of. The sender of a refused claim may well read `revoked: false` on every list in the account — that unwound device is exactly the member this gate was repointed to catch — so an analyst must not conclude from this event that the register named the sender. |
| `core.key.epoch_refused` | warn | A `key_envelope` named an epoch more than `MAX_EPOCH_LEAP` above this vault's live one and was not absorbed; `stream_h`, `epoch`, `live_epoch`. `MAX(epoch)` is what makes a key live, so absorbing an absurd one would strand rotation and redirect this device's own writes. |
| `core.device.backfill_failed` | warn | Storage failed while sealing this device's held Stream keys to a newly certified one; `reason` (`storage`), `subject_h`, `cause`. The cert itself still applies, so the device is a member with a gap: it holds no key for the epochs minted before its cert arrived, and since `ID_D_priv` no longer travels in a pairing payload there is no identity copy for it to open instead. Nothing retries it — a cert is published once per vault and so applied once per replica — but any other online replica's backfill covers the same gap, and the next rotation of that stream seals a fresh epoch to the device anyway. It regains new content, not the epoch it missed. |
| `core.device.admitted_after_revocation` | warn | A `device_cert` for a device id this vault has never seen was applied in an account that has at least one recorded revocation; `sender_h`, `subject_h`. It is what an ordinary pairing looks like *and* what a revoked device rejoining under a fresh id looks like, and nothing in the vault can separate them: revocation names a device id, while certificates are signed by the account identity's key. **Still reachable, and narrower than it was.** Since [#221](https://github.com/justin13888/Sunrise/pull/221) closed [#105](https://github.com/justin13888/Sunrise/issues/105) a device admitted by pairing holds no `ID_S_priv` and cannot certify a fresh id at all, so the hostile reading is now confined to revoking the device that **created** the account: where the revocation rotated the identity the fresh cert is under a retired link and `core.device.cert_superseded_identity` fires beside this one, and where it could not (`core.identity.rotation_unavailable`) the fresh cert verifies under the head and every other column reads like an honest member's. The cert applies either way — refusing it would diverge replicas — so this exists to be seen rather than to decide. **It no longer only reaches an operator:** the same predicate is written to `devices.admitted_after_revocation` at apply time and carried on `DeviceRow` / `DeviceListRow` to the device list on every client ([#144](https://github.com/justin13888/Sunrise/issues/144)). |
| `core.device.cert_superseded_identity` | warn | A `device_cert` applied, but the chain identity that verified it is not the one in force; `sender_h`, `subject_h`, `issuer_h`, `head_h`. This is what a departed device's rejoin looks like after ADR-0032: it kept `ID_S_priv`, so the cert it signs is genuine — under a link the account has since retired. The row lands and every replica agrees it landed, because applying is unconditional; what the device does not get is membership, which is `devices.identity_id = ` the head, tested at every point of use. Disclosed and never gated: it is also what an honest device looks like when its cert predates a rotation it has not yet applied, and nothing in the vault separates the two at apply time. |
| `core.identity.transition_rejected` | warn | An `identity_transition` was not recorded as a link; `reason` (`id_derivation` / `oversized` / `roster` / `shares` / `roster_binding` / `successor_sig` / `prev_sig` / `siblings`), `sender_h`. Mostly structural — a chosen `to_identity_id` that is not the derivation of its own key, a roster or share list longer than an account may hold, a roster entry that does not decode or names a device twice, a share of the wrong width, a roster cert not issued by the successor identity. `successor_sig` is the one signature checkable this early: every input to `next_sig` is in the payload, and it is checked here because the row is `INSERT OR IGNORE`d on `to_identity_id`, so without it the first copy to arrive, honest or not, owns that key for good. `prev_sig` is checked **only when this replica has already established the predecessor** — it has no key otherwise, and must still store the row, which is what a replica catching up on a chain newest-first depends on. `siblings` is the predecessor's register being full of rows that all outrank this one; see `core.identity.sibling_evicted` for the other outcome. Logged and dropped rather than failing the delivery, like every other rejection in `apply_control_op`. |
| `core.identity.sibling_evicted` | warn | A predecessor was at `MAX_SIBLINGS_PER_PREDECESSOR` successors and an arriving transition outranked the weakest, which was deleted to make room; `issuer_h`. The places are held by rank in the fold's own order rather than by arrival ([ADR-0040](../11-adr/0040-sibling-admission-is-a-rank.md)), so this is a register under pressure and not an error: an honest predecessor has one successor, or two or three when devices rotate concurrently, and never reaches the cap. The evicted row is by construction one the fold's `LIMIT` would never have read. Repeated occurrences on one account mean somebody is spending places. |
| `core.identity.siblings_purged` | warn | An identity this replica has now established was holding successors it never signed; `issuer_h`, `n_dropped`. The converging half of [#232](https://github.com/justin13888/Sunrise/issues/232): a transition naming an unestablished predecessor is stored with `prev_sig` unchecked, so rows nobody verified can accumulate, and the moment the predecessor lands they become decidable. `from_identity_id` determines the key they must verify under, so a row that fails here fails forever and is deleted rather than left holding a place. A non-zero `n_dropped` is a statement that this replica **was** carrying forged rows, not that it is now. |
| `core.identity.roster_entry_rejected` | warn | A roster entry of an applied `identity_transition` was skipped; `reason` (`rebinds_key`), `subject_h`. The entry's cert names a device this vault already holds with a **different** `D_D_pub`. An honest roster copies each survivor's stored key into its re-issued cert, so this is a holder of the outgoing `ID_S_priv` redirecting that device's envelopes; the same set-once rule as `core.device.cert_rejected`'s `rebinds_key` ([#281](https://github.com/justin13888/Sunrise/issues/281)). The transition still applies. The device is not moved onto the successor, which is exactly what an omission from the roster already means: it stays on its old row and reads as not current. |
| `core.identity.recovery_code_invalidated` | warn | A revocation rotated the identity and could **not** carry it forward under the outgoing `ID_D_pub`; `subject_h`, `head_h`. Emitted in exactly one case: the device being revoked is the account's creator, so it holds `ID_D_priv`, and a carry share sealed to that key would hand the successor to the device the rotation exists to exclude. The user's BIP-39 recovery code no longer opens this account and a client must say so — silently invalidating it is worse than refusing the revocation, because the user believes they still have a way back in. |
| `core.identity.adopted` | info | This device opened its share of a new account identity and now signs under it; `head_h`. The ordinary outcome of a rotation for a device that survived it. |
| `core.identity.not_in_roster` | warn | The account's identity moved and this device holds no share of the successor; `head_h`. **Not an error — it is the mechanism.** Only the devices named in a rotation's roster receive a share, so this is what being excluded looks like from the inside: the device keeps signing under a retired identity, every membership test reads it as not-current, and no peer seals it another Stream key ([#105](https://github.com/justin13888/Sunrise/issues/105), ADR-0037). It is also what an honest device sees if it applies the transition before the roster cert that names it, so it is disclosed rather than acted on. |
| `core.identity.adopt_failed` | warn | A share opened but the adoption could not be written; `head_h`, `cause`. Storage-shaped, and it leaves the device signing under the previous identity — recoverable, because the fold runs again on the next open. |
| `core.identity.rotation_unavailable` | warn | A revocation cut every future Stream key but could **not** rotate the account identity, because this device holds no `ID_S_priv`; `subject_h`. Every device admitted by pairing is in that state since [#105](https://github.com/justin13888/Sunrise/issues/105), and for most revocations it costs nothing: a revoked device that was itself paired cannot certify itself back in either way, which is what rotation used to be for. The one case it matters is revoking the device the account was **created** on — that device does hold the key — and the remedy is to run the revocation from there. |
| `core.op.deferred_evicted` | warn | The parked-op buffer hit its cap and the oldest rows were dropped; `n_dropped`, `stream_h`. Ordinary traffic never reaches it: a legitimate park is released by the very next absorbed key. |

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
