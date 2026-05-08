---
status: draft
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
ClientHello { protocol_version, account_id, device_id, supported_compressions }
ServerHello { protocol_version, server_id, server_time, accepted_compression }
Auth        { device_sig over (server_nonce || account_id || device_id) }
AuthOk      | AuthFail { reason }

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

Error       { code, message_safe_for_logs }
Bye         { reason }
```

The server **never** sees op contents; it only sees envelopes (which are opaque ciphertext to it) and routes them.

## Connection lifecycle

```
Client connects → ClientHello → ServerHello → Auth → AuthOk
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

`protocol_version = 1`. The server must support N and N-1; older clients receive a clean error.

## Security at the protocol layer

- Wrapped in TLS 1.3.
- `Auth` proves device-key possession via a fresh server nonce signed by `D_S_priv`.
- Op envelopes are independently authenticated (signed) and encrypted; protocol-layer auth is for connection access, not content trust.

## Self-host vs managed

The protocol is identical. Self-host operators may disable certain endpoints (push, sharing) by configuration; clients query capabilities at handshake.

## Error model

Errors are *terminal* (server sends `Error` then `Bye`) or *recoverable* (server sends `Error` for a specific Subscribe but keeps the connection open). Client retries with exponential backoff bounded at 60s with jitter.
