---
status: accepted
---

# Sync Wire Protocol

A binary, length-prefixed message framing over a reliable byte stream, implemented in `crates/sunrise-wire-protocol/`.

> **[ADR-0023](../11-adr/0023-sse-sync-transport.md) replaced the transport under
> this protocol, and the move has landed.** Sync is an SSE stream downstream and
> typed `POST` operations upstream; the WebSocket this document was written
> against is gone (see [`transports.md`](./transports.md)). The **payload types
> and their canonical CBOR encoding survived unchanged** — `Hello::negotiate`,
> the capability bitfield and the frozen fixtures all carried over. The frame
> header below did not survive on the wire: magic, versions, `msg_kind`, flags
> and length are subsumed by HTTP framing and SSE event types. It survives
> *inside the client*, because `SseTransport` is the seam — the sync driver
> still hands it whole encoded frames and still reads whole encoded frames back
> — and it survives on the relay's own storage, which appends the rebuilt
> `OpBatch` frame verbatim and streams those same bytes out again. So the
> layout below still describes real bytes; it no longer describes a wire
> protocol two peers speak to each other.

## Frame layout

Every frame begins with the protocol-versioning magic prefix per [`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md) §3, then a length, flags, and payload:

```
0:2     magic        "SR" (ASCII)
2:3     kind         u8                      ; framing kind = 1 for the wire frame
3:5     version      u16 big-endian          ; wire protocol version (1 in v1)
5:6     msg_kind     u8                      ; the message-kind discriminator (table below)
6:7     flags        u8                      ; bit 0 = compressed (zstd); bits 1-7 reserved, IGNORED on read
7:11    len_prefix   u32 big-endian          ; length of `payload` BEFORE compression (the decompressed length)
11:N    payload      bytes                   ; deterministic CBOR (per §03.6); zstd-compressed iff flag bit 0 set
```

`msg_kind` is the discriminator. Payloads are canonical CBOR (deterministic per [`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md) §6).

**`sunrise_cbor::{encode_canonical, decode_canonical}` MUST be the only
encoder/decoder for wire payloads** — not `sunrise_crypto::canonical_cbor`,
which does not exist. The pair is applied by the `canonical_codec!` macro in
`crates/sunrise-wire-protocol/src/payloads.rs`, which gives nine payload types
an `encode`/`decode` pair: `OpBatchPayload`, `AckPayload`, `NackPayload`,
`SubscribePayload`, `CaughtUpPayload`, `ErrorPayload`, `ClosePayload`,
`RefreshTokenPayload` and `RefreshTokenAckPayload`. `decode_canonical` rejects
non-canonical input and trailing garbage, which is what makes
`CRYPTO_NON_CANONICAL_CBOR` enforceable rather than aspirational.

The rule covers **all** payloads including the handshake. `Hello` and
`HelloAck` are the exception in the tree, not in the contract, and after
[ADR-0023](../11-adr/0023-sse-sync-transport.md) the exception is entirely
client-side: `crates/sunrise-core/src/sync_driver.rs` encodes the `Hello` frame
with `ciborium::ser::into_writer`, and `SseTransport` reads it back with
`ciborium::de::from_reader` and writes the `HelloAck` frame the same way
(`crates/sunrise-sync/src/sse.rs:320-366`). Both bypass the canonicality check
every other payload gets. The server sees neither frame — `POST /sync/session`
takes a typed body and rebuilds a `Hello` from its fields
(`crates/sunrise-server/src/api/sync.rs`) — so the handshake's two hops through
`ciborium` now happen on one side of the wire, between the driver and the
adapter that speaks HTTP for it.

**Closing it is a wire-format change, and it is deliberately deferred to
[ADR-0023](../11-adr/0023-sse-sync-transport.md)'s transport move.** `Hello`'s
field declaration order is not canonical order — `trace` (5 bytes) and
`capabilities` (12) both sort ahead of `client_app_v` — so routing it through
`encode_canonical` produces *different bytes* for the same value, which would
invalidate the frozen `tests/fixtures/hello/*.cbor` vectors and desynchronise
any client still encoding in declaration order. Since ADR-0023 moves the
handshake off the frame protocol and into a typed `POST /sync/session`, paying
for a format break on a frame that is being retired buys nothing. The nine
payloads that *do* use the canonical codec are now guarded by
`declaration_order_is_canonical_order`, which asserts declaration order already
equals canonical order — and which fails when pointed at `Hello`, which is how
the claim above was checked rather than assumed.

### Frame size limit

A reader **ignores** flag bits it does not define rather than refusing the
frame. Adding a flag is a minor change per
[`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md) §6,
and a reader that refused one would make every such addition breaking: every
deployed peer would reject every frame from a newer one over a bit it could
safely have skipped. An unknown message *kind* is still refused — the payload
cannot be interpreted at all — and the difference between the two rules is the
whole rule: ignore what you can safely ignore, refuse what you cannot. A flag a
future version makes load-bearing therefore needs a `WIRE_PROTO_V` bump, not
merely a new bit.

`len_prefix` is the **decompressed** length, not the on-the-wire byte count:
`encode_frame` writes `payload.len()` before compressing, and `decode_frame`
refuses a frame whose decompressed body does not match it exactly
(`PayloadTruncated`). A reader therefore learns the allocation it needs before
it decompresses, which is what makes the bomb check below a bound rather than a
report.

Two separate caps, and they are checked against different quantities
(`crates/sunrise-wire-protocol/src/frame.rs`):

| Cap | Constant | Measured against |
|---|---|---|
| **4 MiB** | `MAX_FRAME_BYTES` | the whole encoded frame — 11-byte header plus the *compressed* body |
| **16 MiB** | `MAX_DECOMPRESSED_BYTES` | `len_prefix`, checked **before** decompression, and the decompressed bytes again after |

Larger payloads are split at the application layer into multiple OpBatches; v1 has no multi-frame buffering protocol. Over the frame cap is a protocol error: server closes with `PROTOCOL_FRAME_TOO_LARGE`. Over the decompressed cap is `PROTOCOL_DECOMPRESS_BOMB`.

### Compression

Compression is per-frame, optional. Scope is the `payload` only.

- Server SHOULD compress when `payload >= 1 KiB` and zstd level 3 reduces size by ≥ 10%.
- Decompressed size cap: 16 MiB. A frame whose decompressed size exceeds the cap is `PROTOCOL_DECOMPRESS_BOMB`.

**zstd is implemented and never enabled.** `encode_frame` and `decode_frame`
both handle `FrameFlags::ZSTD` and the round trip is unit-tested, but
**no call site outside those tests ever sets the bit** — every `encode_frame`
call in the server, in `SseTransport` and in the sync driver passes
`FrameFlags::EMPTY`. Every frame on the wire today is uncompressed. ADR-0023
retires the flag with the frame header rather than carrying it forward unused,
so the "SHOULD compress" rule above describes a policy no build has ever
executed.

## Message catalog

The OIDC access token is carried as an `Authorization: Bearer …` header on every
sync request — `POST /sync/session` and each operation after it. The server
validates the token before running `Hello::negotiate` and uses `X-Sunrise-Device`
to bind the session to a registered device row. There is no separate Auth
message.

A `?access_token=` query parameter, for browsers that cannot set a header, is
**not implemented**: no route reads one. What keeps it from becoming a logged
bearer if it ever is added is the server's request log
(`crates/sunrise-server/src/api/observe.rs`), which is handed the matched route
rather than the request's URI and so has no query string in reach of it at all.
Treat the parameter as reserved, not available.

### Message kinds (canonical)

```
KIND  NAME                  DIRECTION   PAYLOAD CDDL TAG
0x01  Hello                 C → S       see protocol-versioning §4
0x02  HelloAck              S → C       see protocol-versioning §4
0x03  OpBatch               C ↔ S       OpBatch
0x04  Ack                   S → C       Ack
0x05  Nack                  S → C       Nack
0x06  Subscribe             C → S       Subscribe
0x07  StreamUpdate          S → C       CaughtUp (see below)
0x08  SnapshotReq           C → S       SnapshotReq
0x09  SnapshotResp          S → C       SnapshotResp
0x0A  PresenceBeacon        C → S       PresenceBeacon
0x0B  PresenceUpdate        S → C       PresenceUpdate
0x0C  Ping                  C ↔ S       empty
0x0D  Pong                  C ↔ S       empty
0x0E  Error                 S → C       Error
0x0F  Close                 C ↔ S       Close { code: ErrorCode, reason: tstr }
0x12  RefreshToken          C → S       RefreshToken
0x13  RefreshTokenAck       S → C       RefreshTokenAck { expires_at_ms: uint }
```

`0x10` and `0x11` are unassigned. `RefreshToken` takes `0x12` so the original
v1 block `0x01..=0x0F` stays contiguous and a later addition is visibly one.
`MsgKind` in `crates/sunrise-wire-protocol/src/messages.rs` is the authority on
the discriminators; `MsgKind::from_byte` refuses anything not listed.

### Which kinds are live

Defining a kind and handling one are different things, and the gap is
load-bearing for anyone reading the code:

| Kind | Status |
|---|---|
| `Hello`, `HelloAck`, `OpBatch`, `Ack`, `Subscribe`, `Ping`, `Pong`, `Error`, `Close`, `RefreshToken`, `RefreshTokenAck` | Handled end to end. |
| `StreamUpdate` (`0x07`) | Carries `CaughtUpPayload`, **not** an `OpBatch`. The relay forwards live op frames **verbatim**, so they keep `msg_kind = OpBatch`; `StreamUpdate` is otherwise unused, which is why `CaughtUp` reuses it rather than claiming a fresh discriminator in a fully-allocated kind space. |
| `Nack` (`0x05`) | Defined, typed (`NackPayload`), and **never sent by the server**. Every server-side rejection goes out as `Error`. Switching is blocked on the *client*, not the server: `sync_driver` treats `Nack` as non-fatal and ignores it, so emitting one today would convert a rejection the client currently surfaces into one it silently discards. The client half lands with ADR-0023's transport move. |
| `SnapshotReq` / `SnapshotResp` (`0x08`/`0x09`), `PresenceBeacon` / `PresenceUpdate` (`0x0A`/`0x0B`) | Defined, unimplemented, and now **refused with a typed `Error`** rather than dropped. Dispatch is exhaustive over `MsgKind`: a client-sent server-to-client kind ends the session, and a defined-but-unserved kind is answered with `SYNC_OP_INVALID`. Adding a kind to `MsgKind` no longer compiles until it is placed on one side of that line. |

**The contract, now implemented.** Dispatch MUST be exhaustive over
`MsgKind`. A kind the server has decided not to serve MUST be
refused with a typed `Error` naming it — silence is the one response a client
cannot act on, because it is also what a dropped frame, an overloaded relay and
a hung task look like. ADR-0023 lists the `_ => true` arm among the five
defects it records and fixes in place, independently of the transport move.

`RefreshToken` presents a fresh bearer token on an **already open** session. It
is out of band on purpose: a token expires mid-session because that is what
tokens do, and without this frame the only remedy is to close the socket and
renegotiate — dropping the subscription, re-running the handshake, and losing
every op in flight. The server closes with `AUTH_TOKEN_EXPIRED` only when the
client fails to refresh, not merely because time passed.

`RefreshTokenAck` is the positive half of that exchange, and it is gated on
capability bit 8 (`SRV_TOKEN_REFRESH`); a client MUST NOT send `0x12` unless it
saw that bit come back agreed. Without the ack, success is *silence* — and so
is a server too old to know the frame, which calls for the opposite response.
The ack carries `expires_at_ms`, the deadline the server actually adopted:
it can differ from the client's own reading of the token's `exp` by the
server's configured leeway, and the server's is the one that ends the session.
`expires_at_ms = 0` means no deadline, which is what the self-host verifier
issues. A rejected refresh still answers with `Error`, so every outcome —
accepted, rejected, unsupported — is now distinguishable.

`AUTH_TOKEN_EXPIRED` is deliberately distinguishable from `AUTH_TOKEN_INVALID`
and `AUTH_DEVICE_REVOKED`: the first is recoverable by the client on its own,
the other two require the user. A client that cannot tell them apart either
re-prompts every hour or retries forever against a revoked device.

Frames are capped at 4 MiB; if a payload would exceed that, the application splits into multiple OpBatches. v1 has no multi-frame protocol.

The server **never** sees op contents; it only sees envelopes (which are opaque ciphertext) and routes them.

### Error codes (canonical enum)

`crates/sunrise-error/src/codes.rs` is the enum, and it is the authority — this
list mirrors it and MUST be updated with it. Adding a code is non-breaking;
reusing or removing a code is a breaking change requiring an ADR.

The codes reachable on the sync path:

```
PROTOCOL_BAD_MAGIC              SYNC_PROTOCOL_VERSION_MISMATCH
PROTOCOL_FRAME_TOO_LARGE        SYNC_BATCH_TOO_LARGE
PROTOCOL_DECOMPRESS_BOMB        SYNC_OP_INVALID
CRYPTO_NON_CANONICAL_CBOR       SYNC_STREAM_NOT_FOUND
CRYPTO_SUITE_MISMATCH           SYNC_CURSOR_GAP
AUTH_TOKEN_EXPIRED              DOC_SCHEMA_TOO_OLD
AUTH_TOKEN_INVALID              CAPABILITY_REQUIRED_MISSING
AUTH_DEVICE_REVOKED             RELAY_GRANT_REVOKED
AUTH_QUOTA_EXCEEDED             RELAY_STORAGE_UNAVAILABLE
                                FATAL_INTERNAL
```

Nine names this document used previously are **not** in the enum and MUST NOT be
quoted: `PROTOCOL_VERSION_MISMATCH` (it is `SYNC_PROTOCOL_VERSION_MISMATCH`),
`PROTOCOL_NON_CANONICAL_CBOR` (it is `CRYPTO_NON_CANONICAL_CBOR`),
`SERVER_INTERNAL` (it is `FATAL_INTERNAL`), `SERVER_OVERLOADED`,
`PROTOCOL_INVALID_CBOR`, `PROTOCOL_UNKNOWN_KIND`, `AUTH_RATE_LIMITED`,
`SYNC_OP_DEP_MISSING_TIMEOUT` and `SYNC_NOT_SUBSCRIBED`. An unknown `msg_kind`
maps to `SYNC_OP_INVALID` and every other header failure to
`PROTOCOL_BAD_MAGIC` (`FrameError::as_error_code`).

### Partial OpBatch on disconnect

A batch arrives as one `POST /api/v1/sync/ops`, so a connection that fails mid-request leaves the server with an incomplete body and no handler run at all; nothing is persisted. The relay never *applies* anything — it rebuilds the `OpBatch` frame from the request's base64 op envelopes, reads `stream_id` and the cleartext per-device heads out of it, appends the frame bytes verbatim to the durable relay log, and only then acks (`ops` in `crates/sunrise-server/src/api/sync.rs`). A storage failure answers `503` with nothing acked. Durable-before-ack is deliberate: the client drops an acked batch from its outbox, so acking an uncommitted batch would lose it on both sides at once.

Client-side: an outbound OpBatch is held in the persistent outbox until the server acks it (`Ack { batch_id, stream_id, server_first_seen_ms }`). On reconnect, unacked batches are re-sent; `batch_id` is the idempotency key the server dedups on. There is no `applied_seq_range` on the wire — the client learns nothing about server-side sequencing from an `Ack` beyond "this batch landed".

### OpBatch and Ack payloads

The authority is `crates/sunrise-wire-protocol/src/payloads.rs`. Fields are
declared in canonical map-key order (key length, then bytewise), so an
independent canonical encoder reproduces the same bytes.

```cddl
OpBatch = {
  ops:       [* bstr],   ; opaque OpEnvelope bytes, one CBOR byte string each
  batch_id:  uint,       ; client-generated idempotency key for the batch
  stream_id: bstr .size 16,
}

Ack = {
  batch_id:             uint,           ; echoes the acked batch
  stream_id:            bstr .size 16,
  server_first_seen_ms: uint,
}
```

Three differences from what this section used to claim, all of them
consequential:

- **`batch_id` is a `uint`, not a `bstr`.** It is not a ULID; the client mints
  a `u64` idempotency key.
- **`ops` is a flat array of byte strings**, not an array of maps. There is no
  per-op `server_first_seen_ms` on the wire.
- **`stream_id` is present on both payloads.** A batch targets exactly one
  stream, which is what lets the relay route and cursor-filter without opening
  an envelope.

### Server timestamp annotation

When the server first sees a batch it stamps `server_first_seen_ms =
relay_clock`. This is **not** part of the signed envelope, and it rides on the
`Ack` — **once per batch**, not once per op. Receivers persist it; it is the
value used for clock-skew clamping in
[`../03-crypto/audit-and-tamper-evidence.md`](../03-crypto/audit-and-tamper-evidence.md).

## Connection lifecycle

```
Client opens WS upgrade with Authorization: Bearer <oidc_jwt> + X-Sunrise-Device
Server validates token + device → 101 Switching Protocols (or 401 / Close)
Client → Hello → Server → HelloAck
Client → Subscribe(streams)
Server → OpBatch ... (retained frames, replayed verbatim)
Server → StreamUpdate{CaughtUp(stream_id)} (backlog for that stream is drained)
Server → OpBatch(...) live frames as they arrive from peers
Client → OpBatch (locally generated ops) ← Ack (server echoes after persisting)
…
Client → Close (or socket close)
```

Replay and live fan-out are the **same** frame kind: the relay republishes
retained and live frames byte-for-byte, so a client cannot tell them apart from
the header and does not need to. `CaughtUp` on `StreamUpdate` is the only
boundary marker, and it is per-stream.

## Cursors

A device maintains, per (Stream, device_id) pair, a cursor `n` meaning **"I have applied every op through `seq` n, with no holes."** On reconnect, it sends these cursors in `Subscribe`; the server replies with everything after.

### A cursor is a contiguous prefix, not a high-water mark

This is a correctness invariant, not an implementation detail, and it is the single easiest thing to get wrong here.

Ops genuinely do arrive out of order: a dropped frame followed by a later one leaves the op log holding seqs `{1, 3}`. A cursor defined as `MAX(seq)` over what has been applied would then claim `3` while `2` is still missing. Defined as a contiguous prefix, it correctly claims `1`.

The distinction is invisible for as long as the relay ignores cursors and replays its whole ring — every hole refills on the next reconnect by accident. It stops being free the moment the relay honours cursors (the filtering described above): a max-based cursor of `3` makes the relay skip the frame carrying `2` on *every* subsequent subscribe, so the hole never refills and nothing ever notices. Cursor filtering and prefix semantics have to land together — filtering alone converts an accidental, self-healing data-loss window into a permanent and silent one.

Two consequences follow, and both are load-bearing:

- **Compute the prefix from the op log; never track it incrementally.** A counter advanced at apply time can drift from what was actually applied. Derived from the log it cannot, and because op rows are only ever inserted it can only grow.
- **A device's own ops advance its own cursor.** A device has certainly applied what it authored. Omitting them means every `Subscribe` claims nothing about the sending device, and a cursor-filtering relay hands a device its entire own history back on every reconnect.

Under-claiming is always safe — it costs one idempotent re-apply. Over-claiming is data loss. When in doubt, claim less.

The relay reads each retained frame's per-device `(device_id, max_seq)` from the op envelope's **cleartext routing header** — fields 2/3/4, which are signed and AEAD-associated but not secret. Reading them is in scope; reading op *contents* is not, and remains an explicit non-responsibility ([`../06-server/overview.md`](../06-server/overview.md)).

Filtering is one-sided: a frame is skipped only when *every* device head in it is at or below the subscriber's cursor. A frame whose header the relay cannot read is always replayed. Over-delivery costs the client one idempotent no-op; under-delivery is data loss.

### Cursor gaps

Replay is served from the relay's **durable** op log, bounded per channel by age (30 days) and size (256 MiB) — see [`../06-server/relay-and-blob-storage.md`](../06-server/relay-and-blob-storage.md). The in-memory ring in front of it is live fan-out only, so passing *its* bound costs nothing and a relay restart loses no history. When durable retention deletes a frame, the channel raises an `evicted_through` watermark for each device it carried. A subscriber whose cursor for such a device is **below** that watermark is missing ops the relay can no longer produce, and receives:

```
Error { code: "SYNC_CURSOR_GAP", reason: "…device <h> cursor N < evicted_through M…" }
```

before the (partial) replay and before `CaughtUp`, so a client cannot read "caught up" as "complete". This is recoverable but not *retryable*: re-sending the same `Subscribe` will never produce the missing ops. The client resyncs from a peer.

Being caught up past everything evicted is **not** a gap — otherwise every healthy long-lived session would be told to resync each time retention turned over. Watermarks are durable, so a restart no longer resets them: a fresh relay reports exactly the gaps that are real. (While they lived only in memory, a restart cleared frames and watermarks together and the relay reported *no* gap at all — telling a returning device it was complete when it was not.)

A client that receives this must not treat the following `CaughtUp` as completeness. The reference client latches a degraded sync state for the rest of the session and never reports `Live`, because no retry on that connection can produce the missing ops.

## Backfill from snapshot — target state

*Not implemented.* `SnapshotReq`/`SnapshotResp` have discriminators and nothing
else — no payload types, no handler — and a device whose cursor has fallen below
the relay's `evicted_through` watermark gets `SYNC_CURSOR_GAP` and resyncs from
a peer instead (see [Cursor gaps](#cursor-gaps)). The intent: if a device's
cursor is older than the most recent snapshot for a Stream, the server sends
`SnapshotResp` first, then ops since the snapshot.

## Atomic batches

`OpBatch` may carry multiple ops, and a batch is the unit of *relay durability*
and of *acknowledgement*: the relay appends the whole frame or none of it, and
one `Ack` covers one batch.

**It is not, today, the unit of application.** The receiving client's driver
decodes an inbound `OpBatch` and calls `Core::apply_remote` **once per
envelope**, each in its own vault transaction
(`crates/sunrise-core/src/sync_driver.rs`), so a batch can be half-applied if
the process dies mid-loop. Every apply is idempotent and entity-level LWW, so
the surviving state converges — but a multi-op user action is not atomic on the
receiving side, and nothing in v1 makes it so.

Transactional all-or-nothing application remains the intended contract for
multi-op user actions (move task across streams = delete + create, must arrive
together). Until `apply_remote` grows a batch entry point, do not rely on it.

## Ordering guarantees on the wire

The server preserves the order in which it received ops from a given originating device. Receivers see ops from device D in D's emission order; cross-device order is not guaranteed (and the merge rule does not need it — the LWW key is carried on the op, not implied by arrival order).

## Versioning

The wire protocol is pinned at **v1** for the entire v1 release line, but **in-band negotiation exists and is the mechanism that enforces the pin.** `Hello::negotiate` in `crates/sunrise-wire-protocol/src/negotiation.rs` takes the client's `wire_proto_supported`, `crypto_suite_supported`, `doc_schema_max` and capability bitfield against the server's own sets, picks `max(intersection(…))` for wire proto and crypto suite, and returns the `HelloAck` — or a `NegotiationError` the server surfaces as a typed `Error` before closing. The rules and their error codes are specified in [`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md) §4, and every failure path has a frozen fixture (below).

With one version on each side the intersection is trivially `{1}`, which is why the pin holds without any coordination — not because negotiation is absent. A future major version coordinates with a client release ≥30 days in advance per [`../06-server/overview.md`](../06-server/overview.md). An empty intersection (e.g. a self-hoster running an older binary) returns a clean `Error { code: "SYNC_PROTOCOL_VERSION_MISMATCH" }` then `Close`, and the client surfaces "please update Sunrise (client or server)."

### Frozen fixtures

The negotiation surface is byte-pinned. `tests/fixtures/` at the **workspace
root** — not under the crate — holds `hello/v1.cbor`, `hello/ack_v1.cbor`, and
one fixture per negotiation error path (`version-mismatch/wire-mismatch.cbor`,
`crypto-mismatch.cbor`, `doc-schema-too-old.cbor`, `capability-missing.cbor`).
`crates/sunrise-wire-protocol/tests/version_fixtures.rs` decodes the **fixture**
and asserts the error it produces, rather than round-tripping a freshly built
value; the directory sits at the root because `sunrise-crypto` reads the
forward-compat envelope from the same place. Regeneration is behind
`SUNRISE_REGEN_FIXTURES=1`, so drift fails a test instead of being absorbed by
the next `cargo test`. A failure there is not a test bug — it means a
byte-visible change landed in the v1 wire protocol, and per
[`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md) §12
that needs an ADR.

## Security at the protocol layer

- Wrapped in TLS 1.3.
- Connection auth is an OIDC bearer token validated at WS upgrade; see [`../06-server/auth.md`](../06-server/auth.md).
- Op envelopes are independently authenticated (signed by the originating device's `D_S_priv`) and encrypted at the application layer. Protocol-layer auth is for connection access only; content trust comes from the envelope signatures, which the server cannot forge regardless of token compromise.

## Self-host vs managed

The protocol is identical. Self-host operators may disable certain endpoints (push, sharing) by configuration; clients query capabilities at handshake.

## Error model

Errors are *terminal* (server sends `Error` then `Close`) or *recoverable* (server sends `Error` for a specific Subscribe but keeps the connection open). Client retries with exponential backoff bounded at 60 s with jitter (start 500 ms, cap 60 s, jitter ±20%).
