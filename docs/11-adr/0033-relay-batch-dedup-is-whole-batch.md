# 0033 — The relay dedups whole batches, and a re-partitioned re-send is accepted

**Status:** accepted

**Amends:** [`../05-sync/wire-protocol.md`](../05-sync/wire-protocol.md)
(§Ack semantics and idempotency: the uncovered re-send shape stops being
"tracked separately" and becomes a stated, bounded guarantee with a measurement
attached).

## Context

### The question

[#73](https://github.com/justin13888/Sunrise/issues/73) asks whether the relay's
batch dedup should be re-keyed per op. It is a question and not a defect report:
nothing is incorrect today, and the issue says so. What is open is whether the
uncovered case is worth a storage shape, a second retention rule and a change to
what an `Ack` means.

### What the relay actually keys on

`batch_ops_hash` (`crates/sunrise-server/src/api/sync.rs:484`) is a
domain-separated BLAKE3 over the op count and each op's length-prefixed bytes.
`Store::relay_append` (`crates/sunrise-server/src/relay_log.rs:143`) looks that
hash up in `relay_batches` inside the append transaction and returns
`Appended::Duplicate` on a hit, which the handler answers with the **first**
copy's `server_first_seen_ms` and no fan-out
(`crates/sunrise-server/src/api/sync.rs:451`).

`relay_batches` (`crates/sunrise-server/src/store.rs:214`) is keyed
`(account_h, stream_id, ops_h)` and holds `frame_id` as a
`REFERENCES relay_frames(id) ON DELETE CASCADE`. That is the property worth
naming: the dedup window and the replay window are the same window by
construction, with no second sweep and nothing to keep in step by hand.

### The case it cannot catch

`Core::sync_outbox_grouped` (`crates/sunrise-core/src/core.rs:715`) groups
**every** unacked op for a stream into one batch. There is no size cap, so the
partition is "everything unacked at this instant", and the fresh-session drain
(`crates/sunrise-core/src/sync_driver.rs:705`) re-runs it with an empty
`inflight_ops` skip set. So:

1. Session 1 sends `[O1]`; the relay appends and acks; the ack dies with the
   session.
2. The user edits something and `O2` lands in the outbox.
3. Session 2 sends `[O1, O2]`.
4. `batch_ops_hash([O1, O2]) != batch_ops_hash([O1])`. Fresh append. `O1` is
   stored a second time and fanned out a second time.

The batch key cannot close this **by construction** — the hash is over the
partition, and any op authored between attempts changes the partition. The
length-prefixing is not the problem and is doing its own job: it is what stops a
*differently* partitioned batch colliding with another one, which the test
`a_batch_with_different_ops_is_never_deduped`
(`crates/sunrise-server/src/api/sync.rs`) guards.

### What the cost of not fixing it actually is

Correctness does not rest on the relay's dedup at all. It rests on the
**receiver's**, whose key is the op id: `OpLog::insert` is an
`INSERT OR IGNORE INTO ops` (`crates/sunrise-storage/src/oplog.rs:53`) under a
deterministic `remote_op_id(stream_id, device_id, seq)` plus
`UNIQUE(stream_id, device_id, seq)`, and the receive path gates on
`tx.changes() == 0`. A second copy materializes nothing and raises no event.

So the uncovered shape costs disk on one relay channel and one redundant fan-out
per lost ack. It is bounded twice over: by how many ops are unacked at the moment
an ack is lost (which is bounded by how long a session survives), and by
retention — 30 days and 256 MiB per channel
(`DEFAULT_MAX_AGE_MS` / `DEFAULT_MAX_BYTES`,
`crates/sunrise-server/src/relay_log.rs:54`), after which the frame and its
`relay_batches` row are evicted together.

There is no measurement of how often it happens.
`sunrise_relay_batch_duplicate_total` (`crates/sunrise-server/src/api/sync.rs:452`)
counts the re-sends the batch key *did* catch, and nothing counts the ones it did
not — a re-partitioned re-send is indistinguishable, at the relay, from ordinary
new work.

## Decision

**The relay keeps a whole-batch content key. A re-partitioned re-send is
accepted, and the guarantee is written down in the wire protocol rather than
left as a tracked shortfall.**

The guarantee, stated positively:

> The relay dedups a re-sent batch when the re-send carries **exactly** the ops
> the first attempt carried. That covers an in-session retransmit and an idle
> reconnect. A client that authored between a lost ack and the reconnect re-sends
> a different batch, which is stored and fanned out again. Duplicate delivery is
> not a correctness event — the receiver's `op_id` dedup is what correctness
> rests on — and its cost is bounded by the unacked depth at the moment the ack
> was lost, and by retention thereafter.

## Alternatives considered

**A per-op key in the relay.** Rejected for v1, on the shape of the change
rather than on principle.

The relay stores **frames**, not ops. `relay_frames` holds one encoded
`OpBatch` frame and `relay_frame_heads` the per-device `(device_id, max_seq)`
derived from it. Keying dedup per op therefore does not, on its own, save
anything: a batch `[O1, O2]` with `O1` already seen still has to be stored and
fanned out for `O2`'s sake, so the disk this was meant to save is still spent.
To actually save it the relay would have to **filter `O1` out and re-encode the
frame** — which it is technically able to do, since the REST path already
rebuilds the frame server-side (`crates/sunrise-server/src/api/sync.rs:391-401`)
— and that is where the cost lands:

- `Appended` becomes three-valued, because "partly fresh" is now a real answer,
  and every caller of `relay_append` has to decide what it means.
- `frame_heads` has to be recomputed after filtering, or the heads claim seqs the
  stored frame no longer carries — and the head is exactly what
  `relay_replay`'s skip test reads. Over-claiming a head is data loss
  (`crates/sunrise-server/src/relay_log.rs:228`).
- A per-op table needs the `ON DELETE CASCADE` tie to `relay_frames` that
  `relay_batches` has, or the dedup window and the replay window drift apart —
  and forgetting an op id is precisely what lets a legitimate replay through.
  Getting that wrong is silent, permanent op loss, not wasted disk.
- Row count goes from one per batch to one per op inside a 30-day / 256 MiB
  window.

That is a correctness-sensitive rewrite of the append path to save disk on a case
nobody has measured. The trade is wrong in that order.

**Cap the batch size in `sync_outbox_grouped`.** Rejected: it does not address
the mechanism. A cap makes partitions smaller, not stable — the tail batch still
grows by whatever was authored between attempts, and the same hash mismatch
follows.

**Persist the partition client-side.** Not taken now, and **named as the
preferred fix if the revisit trigger fires.** If the outbox remembered which
partition an op was last sent in, session 2 would re-send `[O1]` and then `[O2]`,
and the existing batch key would catch the first exactly. This closes the case
where it is caused, needs no relay change and no second retention rule, and
leaves `Ack` meaning what it means today. The cost is a column on `outbox`
(`op_id, stream_id, enqueued_at_ms, acked_at_ms` today —
`crates/sunrise-storage/src/sync_local.rs:51`) and therefore a storage baseline
edit under [ADR-0018](./0018-storage-baseline-reset.md), plus driver changes to
write it and to re-drain by partition. It is cheaper than the relay-side fix and
strictly better placed; it is simply not worth spending before the measurement
exists.

**Record nothing and leave #73 open.** Rejected. The limitation is already
described accurately in three places — `wire-protocol.md`, the `//` comment above
the `ops` handler, and `batch_ops_hash`'s own doc — and describing a shortfall
repeatedly is not the same as deciding it is acceptable. An open issue that
nobody intends to act on is a claim that the current behaviour is provisional,
and it is not.

## Consequences

- **`wire-protocol.md` states a guarantee rather than a shortfall.** Its
  §Ack-semantics paragraph keeps the mechanism and stops promising that a per-op
  key is coming.
- **The receiver's dedup is the load-bearing one, and the docs say so first.**
  That ordering is the point: relay dedup is a bandwidth and disk optimisation,
  and reading it as a correctness control is how someone later removes the
  `INSERT OR IGNORE` that is actually holding.
- **No code changes.** The handler comment and `batch_ops_hash`'s doc gain a
  citation of this ADR in place of "tracked separately".
- **The gap in observability is now a stated gap.** Nothing counts a
  re-partitioned re-send, which is exactly why the revisit trigger below has to
  name a proxy measurement rather than a direct one.

## What would force revisiting this

1. **A measurement that the uncovered shape dominates.** The proxy available
   today is `sunrise_relay_batch_duplicate_total` against the total append rate
   and the reconnect rate: if reconnects far exceed caught duplicates, the
   re-sends being missed are the re-partitioned ones. A direct measurement is
   better and cheap — count appends whose ops overlap an already-stored batch
   without equalling it — and building that counter is the first step of
   revisiting, not the fix.
2. **A channel hitting its 256 MiB or 30-day bound on re-sent duplicates.**
   Eviction is what turns wasted disk into a `CursorGap` and a latched data-loss
   warning, which is a user-visible outcome rather than an operator-visible one.
   That converts this from an efficiency question into a correctness-adjacent
   one and reverses the trade above.
3. **`sync_outbox_grouped` gaining a size cap for another reason.** A cap does
   not fix this, but it changes the arithmetic — with a cap, the number of
   distinct partitions a given op can appear in is bounded, and a client-side
   persisted partition becomes a smaller change than it is today.
4. **The relay learning to filter a frame for any other reason.** The argument
   against the per-op key is that filtering and re-heading a frame is a
   correctness-sensitive rewrite nobody is otherwise doing. If something else
   makes the relay do it — per-recipient filtering, say — the marginal cost of a
   per-op key collapses and it should be reconsidered on its own merits.
