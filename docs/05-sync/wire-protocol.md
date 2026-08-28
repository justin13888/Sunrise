---
status: accepted
---

# Sync Wire Protocol

A binary, length-prefixed message framing over a reliable byte stream. v1 ships WebSocket only (see [`transports.md`](./transports.md)).

## Frame layout

Every frame begins with the protocol-versioning magic prefix per [`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md) §3, then a length, flags, and payload:

```
0:2     magic        "SR" (ASCII)
2:3     kind         u8                      ; framing kind = 1 for the wire frame
3:5     version      u16 big-endian          ; wire protocol version (1 in v1)
5:6     msg_kind     u8                      ; the message-kind discriminator (table below)
6:7     flags        u8                      ; bit 0 = compressed (zstd); bits 1-7 reserved, IGNORED on read
7:11    len_prefix   u32 big-endian          ; length of `payload` AFTER any compression
11:N    payload      bytes                   ; deterministic CBOR (per §03.6); zstd-compressed iff flag bit 0 set
```

`msg_kind` is the discriminator. Payloads are canonical CBOR (deterministic per [`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md) §6); `sunrise_crypto::canonical_cbor` is the only encoder/decoder for wire payloads.

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

Max frame: **4 MiB** at the transport (the `len_prefix` value is the on-the-wire, possibly compressed, byte count). Larger payloads are split at the application layer into multiple OpBatches; v1 has no multi-frame buffering protocol. A frame with `len_prefix > 4 MiB` is a protocol error: server closes with `PROTOCOL_FRAME_TOO_LARGE`.

### Compression

Compression is per-frame, optional. Scope is the `payload` only.

- Server SHOULD compress when `payload >= 1 KiB` and zstd level 3 reduces size by ≥ 10%.
- Decompressed size cap: 16 MiB. A frame whose decompressed size exceeds the cap is `PROTOCOL_DECOMPRESS_BOMB`.

## Message catalog

The OIDC access token is carried as an `Authorization: Bearer …` header on the WebSocket upgrade request (or `?access_token=` query param for browsers without header support). The server validates the token before accepting `Hello` and uses `X-Sunrise-Device` to bind the connection to a registered device row. There is no separate Auth message.

### Message kinds (canonical)

```
KIND  NAME                  DIRECTION   PAYLOAD CDDL TAG
0x01  Hello                 C → S       see protocol-versioning §4
0x02  HelloAck              S → C       see protocol-versioning §4
0x03  OpBatch               C ↔ S       OpBatch
0x04  Ack                   S → C       Ack
0x05  Nack                  S → C       Nack
0x06  Subscribe             C → S       Subscribe
0x07  StreamUpdate          S → C       OpBatch (server-pushed)
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

The full enum lives in `sunrise-error/src/codes.rs`. Adding a code is non-breaking; reusing or removing a code is a breaking change requiring an ADR.

```
PROTOCOL_BAD_MAGIC
PROTOCOL_VERSION_MISMATCH
PROTOCOL_FRAME_TOO_LARGE
PROTOCOL_DECOMPRESS_BOMB
PROTOCOL_INVALID_CBOR
PROTOCOL_NON_CANONICAL_CBOR
PROTOCOL_UNKNOWN_KIND
AUTH_TOKEN_EXPIRED
AUTH_TOKEN_INVALID
AUTH_DEVICE_REVOKED
AUTH_QUOTA_EXCEEDED
AUTH_RATE_LIMITED
SYNC_BATCH_TOO_LARGE
SYNC_OP_INVALID
SYNC_OP_DEP_MISSING_TIMEOUT
SYNC_STREAM_NOT_FOUND
SYNC_CURSOR_GAP
SYNC_NOT_SUBSCRIBED
RELAY_GRANT_REVOKED
SERVER_INTERNAL
SERVER_OVERLOADED
```

### Partial OpBatch on disconnect

The server **buffers** each inbound OpBatch in memory until the full frame is received and CBOR-validates. Only then does it begin the apply transaction. A disconnect mid-frame discards the partial buffer; nothing is persisted.

Client-side: an outbound OpBatch is held in the outbox until the server acks (`Ack { batch_id, applied_seq_range }`). On reconnect, unacked batches are re-sent; the server uses `batch_id` (an idempotency key, ULID generated client-side) to dedup.

### Server timestamp annotation

When the server first sees an op (in an OpBatch), it annotates the op record with `server_first_seen_ms = relay_clock`. This is **not** part of the signed envelope; it's an out-of-band addendum carried in the OpBatch wrapper:

```cddl
OpBatch = {
  batch_id: bstr,
  ops:      [+ { envelope: bstr, server_first_seen_ms: uint }],
  ; ...
}
```

Other receivers see and persist `server_first_seen_ms`; this is the value used for clock-skew clamping in [`../03-crypto/audit-and-tamper-evidence.md`](../03-crypto/audit-and-tamper-evidence.md).

## Connection lifecycle

```
Client opens WS upgrade with Authorization: Bearer <oidc_jwt> + X-Sunrise-Device
Server validates token + device → 101 Switching Protocols (or 401 / Close)
Client → Hello → Server → HelloAck
Client → Subscribe(streams)
Server → StreamUpdate ... (history since cursors) → Ack
Server → StreamUpdate(...) live ops as they arrive from peers
Client → OpBatch (locally generated ops) ← Ack (server echoes after persisting)
…
Client → Close (or socket close)
```

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

## Backfill from snapshot

If a device's cursor is older than the most recent snapshot for a Stream, the server sends `SnapshotResp` first, then ops since the snapshot.

## Atomic batches

`OpBatch` may carry multiple ops. The receiver applies them transactionally: either all are persisted+applied or none. Useful for multi-op user actions (move task across streams = delete + create, must arrive together).

## Ordering guarantees on the wire

The server preserves the order in which it received ops from a given originating device. Receivers see ops from device D in D's emission order; cross-device order is not guaranteed (and CRDT doesn't need it).

## Versioning

The wire protocol is pinned at **v1** for the entire v1 release line. Clients and server within a major release MUST speak the same version; there is no in-band version negotiation. A future major version coordinates with a client release ≥30 days in advance and migrates everything atomically per [`../06-server/overview.md`](../06-server/overview.md). Mismatch between a client and server (e.g., a self-hoster running an older binary) returns a clean `Error { code: "PROTOCOL_VERSION_MISMATCH" }` then `Close`, and the client surfaces "please update Sunrise (client or server)."

## Security at the protocol layer

- Wrapped in TLS 1.3.
- Connection auth is an OIDC bearer token validated at WS upgrade; see [`../06-server/auth.md`](../06-server/auth.md).
- Op envelopes are independently authenticated (signed by the originating device's `D_S_priv`) and encrypted at the application layer. Protocol-layer auth is for connection access only; content trust comes from the envelope signatures, which the server cannot forge regardless of token compromise.

## Self-host vs managed

The protocol is identical. Self-host operators may disable certain endpoints (push, sharing) by configuration; clients query capabilities at handshake.

## Error model

Errors are *terminal* (server sends `Error` then `Close`) or *recoverable* (server sends `Error` for a specific Subscribe but keeps the connection open). Client retries with exponential backoff bounded at 60 s with jitter (start 500 ms, cap 60 s, jitter ±20%).
