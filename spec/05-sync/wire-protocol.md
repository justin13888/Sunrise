---
status: accepted
---

# Sync Wire Protocol

A binary, length-prefixed message framing over any reliable byte stream (WebSocket, HTTP/2 long-poll, raw TCP-with-TLS).

## Message layout

```
Frame := uint16 len_prefix || msg_kind: uint8 || payload: bytes (CBOR)
```

`msg_kind` is the discriminator. Payloads are deterministic CBOR.

## Message catalog

```
ClientHello { account_id, device_id, supported_compressions }
ServerHello { server_id, server_time, accepted_compression }
;   The OIDC access token is carried as a `Authorization: Bearer …` header
;   on the WebSocket upgrade request (or `?access_token=` query param for
;   browsers without header support); there is no separate Auth message.
;   The server validates the token before accepting ClientHello and uses
;   X-Sunrise-Device to bind the connection to a registered device row.
AuthFail    { reason }   ; sent then `Bye` if token validation fails post-upgrade

Subscribe   { streams: [{ stream_id, since_cursors: { device_id => seq }}] }
Unsubscribe { streams: [stream_id] }

OpBatch     { stream_id, ops: [OpEnvelopeBytes], end_of_batch: bool }
Ack         { stream_id, up_to: { device_id => seq } }

Push        { stream_id, op: OpEnvelopeBytes }   ; relayed live op

Snapshot    { stream_id, snapshot_op: OpEnvelopeBytes }
SnapshotReq { stream_id, since: { device_id => seq } }

ControlOp   { kind, payload: OpEnvelopeBytes }   ; share grants, revokes, device certs

Ping        { client_time }
Pong        { server_time, client_time_echo }

Throttle    { scope: "stream" / "account", target_id?: bstr, retry_after_ms, reason_code }
            ; sent by server when a quota or rate limit is reached; non-terminal

Error       { code, message_safe_for_logs }
Bye         { reason }
```

The server **never** sees op contents; it only sees envelopes (which are opaque ciphertext to it) and routes them.

## Connection lifecycle

```
Client opens WS upgrade with Authorization: Bearer <oidc_jwt> + X-Sunrise-Device
Server validates token + device → 101 Switching Protocols (or 401 / Bye)
Client → ClientHello → Server → ServerHello
Client → Subscribe(streams)
Server → OpBatch... (history since cursors) → Ack
Server → Push(...) live ops as they arrive from peers
Client → OpBatch (locally generated ops) ← Ack (server echoes after persisting)
…
Client → Bye (or socket close)
```

## Cursors

A device maintains, per (Stream, device_id) pair, the highest `seq` it has applied. On reconnect, it sends these cursors; server replies with everything after.

The server itself maintains a per-(account, device, stream) cursor representing what it has already delivered, so it can resume mid-batch on reconnect.

## Backfill from snapshot

If a device's cursor is older than the most recent snapshot for a Stream, the server sends `Snapshot` first, then ops since the snapshot.

## Atomic batches

`OpBatch` may carry multiple ops. The receiver applies them transactionally: either all are persisted+applied or none. Useful for multi-op user actions (move task across streams = delete + create, must arrive together).

## Ordering guarantees on the wire

The server preserves the order in which it received ops from a given originating device. Receivers see ops from device D in D's emission order; cross-device order is not guaranteed (and CRDT doesn't need it).

## Compression

Per-message frame compression negotiated in the handshake. Default: `zstd` level 3 if both sides support; else identity. CBOR + envelope is already compact; compression mostly helps for repeat-text content.

## Versioning

The wire protocol is pinned at **v1** for the entire v1 release line. Clients and server within a major release MUST speak the same version; there is no in-band version negotiation. A future major version coordinates with a client release ≥30 days in advance and migrates everything atomically per [`../06-server/overview.md`](../06-server/overview.md). Mismatch between a client and server (e.g., a self-hoster running an older binary) returns a clean `Error { code: "PROTOCOL_VERSION_MISMATCH" }` then `Bye`, and the client surfaces "please update Sunrise (client or server)."

## Security at the protocol layer

- Wrapped in TLS 1.3.
- Connection auth is an OIDC bearer token validated at WS upgrade; see [`../06-server/auth.md`](../06-server/auth.md).
- Op envelopes are independently authenticated (signed by the originating device's `D_S_priv`) and encrypted at the application layer. Protocol-layer auth is for connection access only; content trust comes from the envelope signatures, which the server cannot forge regardless of token compromise.

## Self-host vs managed

The protocol is identical. Self-host operators may disable certain endpoints (push, sharing) by configuration; clients query capabilities at handshake.

## Error model

Errors are *terminal* (server sends `Error` then `Bye`) or *recoverable* (server sends `Error` for a specific Subscribe but keeps the connection open). Client retries with exponential backoff bounded at 60s with jitter.
