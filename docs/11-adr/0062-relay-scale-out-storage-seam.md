# 0062 — The relay scales out behind four storage traits, fans out over Postgres `LISTEN/NOTIFY` with the durable log as the only cursor, and keeps sync sessions in a shared table

**Status:** accepted

**Built by** [#364](https://github.com/justin13888/Sunrise/issues/364), in the
slices listed under "Delivery order".

**Depends on** the versioned migrations of
[`../06-server/relay-and-blob-storage.md`](../06-server/relay-and-blob-storage.md)
§Schema versions and pragmas (#358), the drain and readiness probe of
[`../06-server/self-hosting.md`](../06-server/self-hosting.md) §Stopping, health
and readiness (#357), the per-node metrics of
[`../06-server/metrics.md`](../06-server/metrics.md) (#356), and the
`LimiterStore` seam in `crates/sunrise-server/src/api/ratelimit/store.rs` (#355).
**Amends** the "Storage layout (managed)" table of `relay-and-blob-storage.md`:
op envelopes stay in the metadata database instead of moving to the object
store. **Leaves** [ADR-0060](./0060-relay-database-encryption-at-rest.md) as the
SQLite backend's at-rest story; a Postgres backend's is the operator's.

## Context

Every piece of relay state is process-local, so exactly one process can serve
an account:

- **Metadata.** One SQLite `Connection` behind a `parking_lot::Mutex`
  (`store/mod.rs`, `Store`), shared by accounts, devices, push tokens, cursors,
  tombstones and the durable relay log (`relay_log.rs`). Handlers call `Store`
  methods directly, and each method is one transaction under that lock, which
  is what makes "revocation takes effect in the request's transaction" true.
- **Blobs.** A filesystem root (`ServerState::blob_root`) with per-account
  directories, written by `std::fs` in `api/blobs.rs` and swept by
  `admin/maintenance.rs`.
- **Fan-out.** `RelayHub` (`relay.rs`) is an in-process map of
  `tokio::sync::broadcast` channels. An op accepted on node A never reaches an
  event stream held on node B.
- **Sessions.** `SessionStore` (`sync_session.rs`) is an in-memory map with a
  15-minute idle bound. A session opened on one node is unknown to every other,
  and all of them are lost on restart.
- **Rate limits** already sit behind `LimiterStore`, with one in-process
  implementation. **Background work** (the maintenance pass in `main.rs`) runs
  on every process that starts.

The documents describe a Postgres and S3 deployment that was never designed
past a table of backends. Five things need settling before any of it is built:
where the seams are, how a frame accepted on one node reaches a subscriber on
another, where a session lives, how rate state and background work are shared,
and what stays true for the single-binary deployment.

## Decision

### 1. Four seams, each a trait with the single-binary implementation first

| Trait | Covers | Single-binary implementation | Scaled implementation |
|---|---|---|---|
| `MetadataStore` | accounts, devices, push tokens, cursors, blob tombstones, deletion tokens, **and the durable relay log** | SQLite (`Store`) | Postgres |
| `SessionBackend` | live sync sessions | in-process map (`MemorySessions`) | Postgres table |
| `BlobBackend` | pending uploads, committed chunks, manifests | filesystem under `blob_root` | S3-compatible object store |
| `FanoutBus` | telling other nodes a channel has new frames | none: the local `RelayHub` is the whole bus | Postgres `LISTEN/NOTIFY` |

`LimiterStore` is the fifth seam and already exists; section 5 gives it its
scaled implementation.

**The traits are async**, with `async_trait`, as `LimiterStore` is: a shared
backend is across a network. The single-binary implementations never await, so
moving behind the trait changes no timing on the default deployment.

**Each `MetadataStore` method is one transaction**, the shape `Store` already
has, rather than a `begin()` handle the caller threads through statements.
Revocation, account erasure, and relay append with its dedup and retention
sweep each touch several tables and must commit or roll back together; a method
per unit of work keeps that inside one implementation, where a conformance test
can see it, instead of in every handler. Under Postgres each method runs at
`READ COMMITTED` in one transaction, so a revocation is visible to every request
that starts after it commits, on any node; a request whose device check ran
before the commit may finish, which is also what the SQLite mutex gives today.

**The relay log is part of `MetadataStore`, not a fifth trait.** Account
erasure deletes an account's relay frames in the same transaction as its rows
(`store/lifecycle.rs`), and a backend split would make erasure a two-system
write with no transaction.

**Op envelopes stay in the metadata database.** The design target put them in
the object store under `ops/<stream>/<op_id>`. A frame is one op batch, bounded
by the request body limit; the append writes it, its routing heads, its dedup
row and its retention eviction in one transaction. Moving the bytes to S3 would
make every append a write to two systems with no transaction between them, and
every replay one `GET` per frame. Postgres holds `bytea` rows of this size
without strain, and the 256 MiB per-channel bound of `relay_log.rs` still
applies. The object store holds blobs only.

**Errors are typed and retryable.** A scaled backend can fail where the
in-process one cannot. A trait error maps to `503 RELAY_STORAGE_UNAVAILABLE`
on a request path and to a retryable `closed` event on an event stream, as a
durable-log read failure already does. Each trait states its error type in its
signature from the first slice, so the scaled implementation adds no
signature change.

**One conformance suite per trait** is a generic `async fn` over `&dyn Trait`,
run against every implementation. The Postgres and S3 runs need a database and
a MinIO, and run where a CI job provides them (`SUNRISE_TEST_POSTGRES_URL`,
`SUNRISE_TEST_S3_ENDPOINT`); they skip with a printed reason elsewhere. The
suites cover what the issue names: revocation atomicity, relay-log ordering and
eviction, and blob two-phase commit.

### 2. Fan-out: Postgres `LISTEN/NOTIFY` as a wake-up, the durable log as the payload

The node that accepts a batch appends it to the durable log and, **in the same
transaction**, issues `NOTIFY sunrise_relay, '<account_h>:<stream_id>:<frame_id>'`.
Postgres delivers a notification only when its transaction commits, so no node
is ever told about a frame that rolled back. The accepting node also publishes
into its own `RelayHub` exactly as today.

A node that receives a notice for a channel it has live subscribers on reads
`relay_frames` for that channel after the highest frame id it has already
published locally, in id order, and publishes each into its own `RelayHub`. A
notice for a channel it has no subscriber on is dropped. The notice carries no
frame bytes, so the 8000-byte `NOTIFY` payload limit never binds.

What is guaranteed, against the existing cursor and `CursorGap` contract:

- **Cursors come from the durable log, never from the bus.** An SSE event id is
  a `relay_frames.id`, on every node, for replayed and live frames alike: a live
  frame carries its log id (`RelayFrame` gains the field). A client that
  reconnects to a different node resumes on that id or on its per-device
  cursors, and the replay is the same query on any node.
- **Ordering** is frame-id order per channel. Across channels nothing is
  ordered, as today.
- **Delivery is at least once.** A frame can reach a subscriber through the
  live path and the replay path, or through a notice and a catch-up read. A
  duplicate is an idempotent no-op at the client's op-log gate, which the
  current subscribe order already relies on (`api/sync/stream.rs`, "Live
  receiver FIRST"). A missed frame is data loss, so every choice here fails
  towards duplicates.
- **A lost notice is recovered by reading, not by the bus.** When a node's
  `LISTEN` connection drops, it reconnects and re-reads every channel it has
  subscribers on from its local high-water. A subscriber's own reconnect
  replays from its cursor regardless.
- **A gap is still the durable log's.** `CursorGap` comes from
  `relay_evicted`, which is in the shared database, so every node reports the
  same gap for the same cursor.

`ConnId` becomes a random 64-bit value per session instead of a per-process
counter. The live path drops a session's own frames by comparing
`frame.from` with `session.conn`, and two counters on two nodes would collide
at once; a random id collides with probability about 2⁻⁶⁴ per pair.

Account erasure and revocation need no bus message. Erasure's `forget_account`
on the accepting node frees that node's channels; every other node's channels
for the account hold no subscriber after its streams end, and an empty channel
costs a map entry. Revocation is in section 4.

**Rejected: Redis or NATS pub/sub.** Either is a second stateful service to
run, back up and secure beside Postgres, and its publish is not tied to the
append's commit: a node could announce a frame whose transaction then rolled
back, or a subscriber could read before the commit it was told about.

**Rejected: each node tailing the relay log on a timer.** The poll interval adds
straight to fan-out latency. Holding the sync p99 under the 500 ms the bench
measures
([`performance-budgets.md`](../10-cross-cutting/performance-budgets.md) §Sync
propagation) would mean every node
polling every active channel several times a second, whether anything changed
or not.

### 3. Sessions: one shared table, keyed by a hash of the id

A scaled deployment keeps sync sessions in a Postgres table. Its key is
`BLAKE3(session_id)`, not the id: the id is bearer-equivalent, so a read of the
table, or a backup of it, must not yield a usable one. The row holds what
`Session` holds today, and `X-Sunrise-Session` keeps every rule
`sync_session.rs` gives it: a header and never a query parameter, 16 random
bytes of entropy, bounded by the bearer's `exp`, collected after 15 idle
minutes. `update` is a read-modify-write under `SELECT … FOR UPDATE`, so two
nodes applying a subscribe and a refresh to one session serialise.

**Rejected: stateless signed session tokens.** A session is mutable: subscribe
replaces its streams and sets `subscribe_unserved`, refresh moves its
deadline, and every operation touches `seen_ms`. A token would have to be
reissued on each of those, so the `X-Sunrise-Session` value would change
mid-session, which is a wire change for every client. And a session is ended
by the server, on a refresh naming another principal and on device revocation,
which a signed token cannot express without a shared deny-list: that is a
shared table again, with a harder invariant.

### 4. Revocation reaches a stream on another node within `device_recheck_ms`

The live loop already re-reads the session's device from the store every
`device_recheck_ms` (`ServerConfig`, default 30 s; not a `sunrise.toml` key
today) and ends the stream with
`AUTH_DEVICE_REVOKED` when it is gone. With a shared `MetadataStore` that read
sees a revocation committed on any node, so a stream held on node 2 ends within
one recheck interval of a revocation on node 1. That interval is the documented
bound. A bus message for revocation is not added: it would shorten the bound
only for the nodes that received it, and the recheck must stay as the floor
for the ones that did not.

### 5. Shared rate limits and one leader for background work

- **Rates.** `LimiterStore` gets a Postgres implementation over one table of
  token-bucket cells, charged by a single `INSERT … ON CONFLICT DO UPDATE …
  RETURNING` per request, so the decision and the charge are one statement.
  The concurrency gates (open streams, open uploads) stay per node, as
  `ratelimit/store.rs` already states: they count what one process holds, so a
  cluster's ceiling is the per-node cap times the node count, and the
  self-hosting page says so.
- **Background work.** The maintenance pass runs on the node that takes
  `pg_try_advisory_lock(<constant key>)` for the length of the pass; a node that
  does not get it skips the pass. A session-level advisory lock is released by
  Postgres when the connection that held it dies, so a crashed leader cannot
  hold the role. Relay-log retention needs no leader: it is applied inside
  every append's transaction. On SQLite the single process is always the
  leader. A lease table with a TTL was rejected: it is a clock-and-renewal
  protocol written by hand, and the advisory lock is that protocol, already
  correct.

### 6. Configuration, migrations and encryption

- The backend is chosen by `[storage] backend = "sqlite" | "postgres"` and
  `[blobs] backend = "filesystem" | "s3"`. Both default to the single-binary
  value, and the `[storage] mode` / `postgres_url` / `s3_*` keys earlier
  drafts used stay refused.
- Credentials come from files the operator names (`postgres_url_file`,
  `s3_credentials_file`), never from the environment, for the reason
  ADR-0060 §2 gives for the database key: a process's environment is readable
  by more than the process.
- Postgres carries its own numbered, append-only migration list, the same
  shape as `store/migrations` (#358), recording its version in a
  `schema_version` table and refusing a database a newer binary migrated.
- `[storage] encrypt` applies to the SQLite backend only, and is refused beside
  `backend = "postgres"`. At-rest encryption of a Postgres or an object store is
  the operator's, through that service's own mechanism; the relay holds only
  ciphertext envelopes and blobs either way, and the metadata list in
  `relay-and-blob-storage.md` §What the relay database actually holds is the
  same list on either backend.

### 7. Blobs on an object store

Object keys keep the per-account isolation the filesystem layout has, so
content addressing cannot name another account's blob:

```
blobs/<account_h>/pending/up_<upload_id>/<chunk_idx>
blobs/<account_h>/committed/<blob_key>/<chunk_idx>
blobs/<account_h>/committed/manifests/<blob_key>
```

Each chunk is its own object of at most 1 MiB, so no multipart upload is ever
started and none can be left half-finished. Finalize re-reads every pending
chunk and re-hashes it with BLAKE3 against the client's per-chunk hashes and
`content_hash`, never trusting an ETag; copies each chunk to its committed key
server-side; and writes the manifest **last**. As on the filesystem, the
manifest is what makes a blob readable, so an interrupted finalize leaves an
invisible partial that the maintenance pass sweeps. A chunk `PUT` is confirmed
by the store before the relay answers.

### 8. The single-binary deployment does not change

SQLite, the filesystem blob root and the in-process hub stay the default and
need no new configuration, no new dependency at runtime, and no new process.
Every existing test runs against them unchanged. A scaled backend is an
operator's opt-in.

## Delivery order

1. This ADR, and the session seam: `SessionBackend` with `MemorySessions`, its
   conformance suite, and typed errors on every session call site.
2. `MetadataStore` and `BlobBackend`, with SQLite and the filesystem moved
   behind them unchanged, each with its conformance suite.
3. The Postgres backends (metadata, sessions, the `LISTEN/NOTIFY` bus,
   `LimiterStore`, the maintenance lock), random `ConnId`, log ids on live
   frames, and the two-node test: client A publishes on node 1, client B on
   node 2 receives it, a session opened on node 1 is honoured on node 2, and
   killing node 1 lets B resume on node 2 with no gap and no duplicate applied.
4. The S3-compatible blob backend, run against MinIO.

## Consequences

- A scaled deployment is Postgres plus an S3-compatible store and nothing else.
  No Redis, NATS or lease service.
- Fan-out latency across nodes is one commit, one notification and one indexed
  read. The p99 the bench measures covers it once the two-node test runs it.
- A cross-node revocation can take up to `device_recheck_ms` to end a stream.
  Making it faster means lowering that setting, at one indexed read per stream
  per interval.
- Per-node concurrency caps multiply with the node count.
- The single-binary relay pays one virtual call per session operation and
  nothing else.
