# 0068 — A stream's snapshot record travels as a blob the relay pins per stream, and a cursor gap is answered by applying it

**Status:** accepted

**Answers** [ADR-0059](./0059-client-op-log-compaction.md) §7 (how a snapshot
reaches a device that needs one) and its first revisit trigger.
**Depends on** ADR-0059 §4 (the record) and §5 (the compactor), and on the blob
store's two-phase commit ([`../06-server/api.md`](../06-server/api.md) §Blobs).
**Built by** [#462](https://github.com/justin13888/Sunrise/issues/462).

## Context

ADR-0059 gave each stream a signed, sealed snapshot record and let a replica
write, store and apply one, but nothing carried a record between devices. A
device the relay's op retention had passed (30 days or 256 MiB per channel,
`crates/sunrise-server/src/relay_log.rs#DEFAULT_MAX_AGE_MS`) got
`SYNC_CURSOR_GAP` and latched `Degraded` with nothing to recover from, and so
did a new device joining an old account, whose empty cursors are below
everything retained.

Three facts bound the transport:

- A record is about the size of the stream's merge state, roughly 780 bytes per
  task, so past about five thousand tasks it does not fit in one 4 MiB frame
  (`crates/sunrise-wire-protocol/src/frame.rs#MAX_FRAME_BYTES`).
- The relay never reads an op's contents, only its routing head.
- The blob store already moves large E2EE ciphertext per account, with a
  content-addressed, verified two-phase commit and a chunked, streamed read.

## Decision

### 1. The record is uploaded as a blob, and pinned as its stream's latest

The stream's compactor uploads its latest record through the ordinary blob
routes: `POST /blobs/init`, one `PUT` per chunk of at most 1 MiB, and
`POST /blobs/finalize`. The record is already sealed and signed, so its bytes
are the blob's bytes; no second layer of encryption is added. It then calls
`PUT /api/v1/snapshots/{stream_id}` with the committed `blob_id`, which makes
that blob the stream's latest record. A device that needs it calls
`GET /api/v1/snapshots/{stream_id}` for the pin and `GET /blobs/{blob_id}` for
the bytes, and checks that they hash to the pinned id.

Rejected: a new frame kind. A frame is bounded by the 4 MiB cap, which a
record outgrows, and the relay trims frames with the stream's ops. Rejected:
carrying the record in pairing. It serves only a new device, never a returning
one, and pairing is a few seconds long by design. Rejected: a dedicated byte
store for records. The blob store already is one, with the isolation, quotas,
hashing and streaming a record needs.

### 2. The relay keeps one record per stream, the last one pinned, outside op retention

The pin is a row in the relay's `stream_snapshots` table (relay migration
0006): account, stream, blob, size, the device that pinned it and when. It is
not touched by op retention, so it outlives every op it covers. A new pin
replaces the old one, and the relay then deletes the blob the old pin named,
unless another of the account's pins names it. Maintenance never collects a
pinned blob, even under a tombstone. An account's pins go with the account.

The relay cannot order two records it cannot open, so the latest is the last
one pinned. Only the compactor publishes, and a replica publishes only a record
it wrote, only while it is the compactor (`Engine::is_stream_compactor`), so two
publishers race only while the election changes hands. A reader that applies
the older of two records corrupts nothing, since applying one is a join; at
worst its frontier is below what the relay evicted, and the gap stands (§3).

The relay reads one thing from the blob before it pins it: the five-byte magic
prefix every Sunrise structure carries in clear, and only its kind, `0x04`. A
blob of any other kind is refused with `400`. That is what keeps the deletion
of a replaced pin from ever deleting an attachment a buggy client pinned. It
reads nothing else of the record: not its stream, its writer or its frontier.

Rejected: keeping every record. Storage would grow with the account's age for
no reader, since a record supersedes the ones before it. Rejected: ordering
pins by the record's `generated_at_ms`, sent beside it. The writer's wall clock
would decide, and a device with a fast clock would hold the pin against every
later compactor.

### 3. A cursor gap is answered with the stream's record

The relay's gap event names its stream, and the SSE transport now carries the
stream in the `Error` payload's new optional `stream_id`. On a gap that names a
stream, the driver fetches the pinned record, checks it, and applies it with
`Core::apply_stream_snapshot`, which raises the stream's floors and cursors to
the record's frontier (ADR-0059 §4). It then re-subscribes every stream from
its cursors, and the relay replays only what lies above them. This runs before
the next frame is read, so the replay the gap preceded lands on the raised
floors.

- **Applied.** The stream is caught up. A second gap on it in the same session
  means the record's frontier is still below what the relay evicted, and the
  session latches `Degraded` as before.
- **Deferred.** The record needs a key or a cert this replica does not hold
  yet, or the relay could not be reached. The session latches `Degraded`, and
  the stream is tried again after any other record applies, and on the next gap
  the relay reports (the resync tick re-subscribes). Once every gapped stream
  of the session has been caught up, the state goes back to catching up.
- **Failed.** No record is pinned, the one pinned adds nothing (`Covered`), or
  it is refused. The session latches `Degraded`, as every gap did before.

A new device needs nothing extra: its empty cursors draw a gap on every stream
whose early ops were trimmed, and the vault-meta stream's record brings the
certs and key envelopes the other streams' records need.

### 4. The compactor's duty runs before a session is `Live`

A session about to report `Live` first runs compaction if it is due and
publishes any record this replica should and the relay does not pin. The
resync tick publishes too, after the compaction it already ran. On almost every
session both are nothing; on the one with a record to upload, `Live` waits for
the upload. So a sync that ends at `Live` (the CLI's `sync --once`, an OS
background window) leaves the record on the relay. What was published is kept
in memory per process, so a record is uploaded once, and after a restart the
first publish asks the relay what it pins before uploading anything.

Rejected: publishing on the tick alone. The CLI and an app's background
window end at `Live`, usually before the first tick. Rejected: a client
migration recording what was published. The relay's pin is the record of
that, and asking it costs one request per stream per process.

## Consequences

- **Relay migration 0006** (`stream_snapshots`). No client migration:
  `STORAGE_V` stays 43.
- **Two routes**, `pinStreamSnapshot` and `fetchStreamSnapshot`, in the `Blob`
  rate-limit group, and one metric, `sunrise_snapshot_pin_total`. The upload
  is charged to the blob upload budget like an attachment's, and counted in
  `sunrise_blob_storage_bytes`.
- **The wire `Error` payload** gains an optional `stream_id`, written only
  when set and read as absent when missing, so a payload without it is
  unchanged byte for byte.
- **A record is bounded by the blob ceiling**, 100 MB, which is about 128
  thousand tasks at the measured 780 bytes each.
- **A record is as fresh as its compactor's cadence**, one day by default.
  Under the age bound that always covers what the relay evicted (thirty days).
  Under the size bound, a stream busy enough to evict 256 MiB within a day can
  outrun its record, and a device past it still latches `Degraded`.
- **A returning device that is itself the compactor** finds only the record it
  wrote before it left, since the others defer to it until it has been silent
  for the known-device window. Under the age bound that window has passed by
  the time the relay evicts; under the size bound it may not have.

## Revisit if

- A stream outruns its daily record under the size bound in practice: the
  compactor's cadence becomes "a day or N ops", as the stream digest's is.
- A shared blob backend lands (ADR-0062): the pin's deletion of a replaced
  blob is then a cross-service write and wants the backend's own lifecycle.
