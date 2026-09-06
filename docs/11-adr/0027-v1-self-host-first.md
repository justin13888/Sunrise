# 0027 — v1 is self-host-first: managed cloud, billing, quotas, presence, Android and sharing are post-v1

**Status:** accepted

**Amends:** seven specs demoted to `status: proposed`, and **twenty-four
`accepted` specs materially edited**. Both lists are enumerated in
§Consequences; nothing this ADR changes is left to be discovered by diffing.
The heaviest single amendment is
[`../03-crypto/audit-and-tamper-evidence.md`](../03-crypto/audit-and-tamper-evidence.md)
§Merkle fold order, where the relay-derived clamp is deleted (§Decision
clause 6).

## Context

*Every `file:line` in this section is read at commit `310e377`, the base this ADR
was written against. The reconciliation it authorises moves many of them; the
section headings named alongside each citation are the durable half.*

The design tree describes a product with two deployment profiles, a paid tier,
per-account quotas, live presence, an Android client and cross-user sharing.
The tree implements one server shape and none of the rest. That gap has been
papered over with per-file "not implemented" banners on documents whose
frontmatter still says `status: accepted` — which, per
[`../README.md`](../README.md) §Conventions, means *frozen design under change
control*. A reader cannot tell a frozen contract from an aspiration, and five
of these documents export concrete numbers that other accepted specs then cite
as if they were binding.

This ADR does the scoping decision once, so the banners stop being the only
signal and `status:` carries it.

### Billing does not exist, and its numbers have leaked outward

[`../06-server/billing.md`](../06-server/billing.md) specifies Stripe
subscriptions, a `processed_stripe_events` table, a webhook route and
110 %-cap quota enforcement. Its own banner (`billing.md:9-20`) concedes that
none of it exists. Grep agrees: there is no Stripe client in the workspace, no
webhook route, no quota accounting in `crates/sunrise-server`. The only trace
is `accounts.tier`, a `TEXT NOT NULL DEFAULT 'free'` column
(`crates/sunrise-server/src/store.rs:136`) that `resolve_account` sets to
`"free"` at provisioning (`store.rs:268`) and that is read exactly once, to
echo onto `AccountInfo` (`api/accounts.rs:72`). Nothing gates on it.

The numbers do not stay in that file:

* [`../05-sync/backpressure-and-quotas.md`](../05-sync/backpressure-and-quotas.md)`:11-18`
  publishes a free/paid resource table; `:49` cites `AUTH_RATE_LIMITED`, which
  `../05-sync/wire-protocol.md:233-238` lists among the nine names the typed
  error surface does **not** contain; `:50` cites an `srv.quota.warning` event
  that no source file emits.
* [`../06-server/api.md`](../06-server/api.md)`:291` documents a `429
  AUTH_QUOTA_EXCEEDED` row, and `:370-378` a whole §Quota responses.
* [`../06-server/observability.md`](../06-server/observability.md)`:123`
  exports `sunrise_quota_exceeded_total`, `:144` allowlists a `plan_tier`
  label, `:209` audits "Plan changed.", `:213` splits retention by profile.
* [`../05-sync/multi-device.md`](../05-sync/multi-device.md)`:116-125`,
  [`../06-server/overview.md`](../06-server/overview.md)`:33-34,128`,
  [`../05-sync/overview.md`](../05-sync/overview.md)`:67`,
  [`deployment-topologies.md`](../01-architecture/deployment-topologies.md)`:19`
  and [`../02-domain/attachments.md`](../02-domain/attachments.md)`:90` each
  carry a tier claim sourced from `billing.md`.

A specification nobody has built is a legitimate artifact. A specification
nobody has built whose numbers are quoted as fact by eight other accepted
specs is a defect.

### Presence is architecturally blocked, not merely unbuilt

[`../05-sync/presence.md`](../05-sync/presence.md)'s banner (`presence.md:7-33`)
records two things this ADR ratifies. First, [ADR-0023](./0023-sse-sync-transport.md)
removed the only frame that could carry a beacon: `PresenceBeacon` (`0x0A`) and
`PresenceUpdate` (`0x0B`) are bare discriminators in
`crates/sunrise-wire-protocol/src/messages.rs:54,56` with no payload type, and
the five typed sync operations in `crates/sunrise-server/src/api/sync.rs` are
one-per-purpose, so nothing would accept one. Second, and larger: presence is
specified unencrypted, and it would be **the only user data the relay reads in
the clear**. That is a posture change — the server moving from "sees no content"
to "sees behaviour: who is online, when, and which stream cohort they are
looking at, at 30 s resolution" — and ADR-0023 never weighed it, because
ADR-0023 was about transport.

### Android has a CI matrix and no code

[`../07-clients/mobile-android.md`](../07-clients/mobile-android.md)`:103-111`
names a five-device tested skin matrix — Pixel 7, Galaxy S22, Redmi Note 12,
Xiaomi 13T, OnePlus 11 — under the heading "Device used in CI / QA". There is
no Android source, no Gradle or SDK configuration, no Android CI job and no
device anywhere in the repository. The complete set of Android traces in the
tree is: the string `"android"` in the device-platform allowlist
(`crates/sunrise-server/src/api/devices.rs:19`, `store.rs:82`,
`crates/sunrise-relay-client/src/bootstrap.rs:34`), two comments saying Kotlin
bindings would be generated "when Android arrives"
(`crates/sunrise-core-bindings/src/lib.rs:5`, `Cargo.toml:10`), and
`.github/workflows/release.yml:314`, which says outright: "Google Play — there
is no Android app."

### Cross-user shared documents assert enforcement the relay cannot perform

[`../05-sync/shared-documents.md`](../05-sync/shared-documents.md)`:27,55,60`
says the relay rejects ops from non-editor identities, withholds rotated keys
from revoked recipients, and enforces role checks as defence in depth. The
relay is structurally incapable of all three:
[`trust-and-server-role.md`](../01-architecture/trust-and-server-role.md)`:44-47`
records that `sunrise-server` has no `sunrise-crypto` dependency and that the
only envelope type it can reach is `EnvelopeHeader`, which carries
`{stream_id, device_id, seq}` and no payload, no signature and no nonce.

Its blob path cannot work either. `crates/sunrise-server/src/api/blobs.rs:29-36`
roots both the pending area and the committed store under a per-account
directory keyed by a BLAKE3 of the account id — deliberately, so that content
addressing cannot become a cross-tenant read primitive. A grantee naming an
owner's `blb_…` therefore gets `404 BLOB_NOT_FOUND` ("No committed blob under
that id **for this account**", `api.md:295`). Sharing is already deferred by
[ADR-0020](./0020-v1-must-demotions.md) §(a); this file is the part of the
design that was left saying otherwise.

### Compaction states its own blocker

[`../04-storage/compaction.md`](../04-storage/compaction.md)`:49-57` says
`doc_state` **is undecided**, and that this is "the main reason this document
is not buildable as written". An accepted spec cannot contain an undecided
field in its wire format.

## Decision

**1. v1 ships one server shape: the self-host single binary.** Managed cloud is
not a v1 deliverable. The two-profile table in
[`trust-and-server-role.md`](../01-architecture/trust-and-server-role.md)`:58-61`
and topology T1 in
[`deployment-topologies.md`](../01-architecture/deployment-topologies.md)`:9-21`
are retained as post-v1 targets, marked as such.

**2. No plan tiers, no billing, no per-account quotas in v1.** The limits v1
enforces are fixed operator constants, not per-account accounting:

| Limit | Value | Source |
|---|---|---|
| Request body | 2 MiB (`[server] max_body_bytes`) | `crates/sunrise-server/src/config.rs:76-77` |
| Blob chunk | 1 MiB ciphertext | `crates/sunrise-server/src/api/blobs.rs:53` |
| Blob chunk count | 4096 | `api/blobs.rs:57` |
| Blob size | 100 MB | `api/blobs.rs:61` |
| Relay-log retention | 30 days, 256 MiB per channel | `crates/sunrise-server/src/relay_log.rs:54,63` |

No `plan_tier` metric label. No `AUTH_QUOTA_EXCEEDED` or `AUTH_RATE_LIMITED` on
the typed error surface, and none planned for v1. (`sunrise-error`'s
`codes.toml` still *declares* two quota codes that nothing produces; removing
them is a code change, not a documentation one, and is filed separately.)

**3. Presence is out of v1**, and cannot return without an ADR that states the
behavioural-metadata leak outright and amends
[`../06-server/overview.md`](../06-server/overview.md)`:35-41`'s
non-responsibilities list in the same change. Restoring a transport for it is
the smaller half of that work.

**4. Android is out of v1.** `mobile-android.md` is retained as a design target,
not a commitment. No CI or QA device matrix is claimed while no such CI exists.

**5. Cross-user shared documents are out of v1** — consistent with ADR-0020 §(a)
and with [ADR-0024](./0024-key-hierarchy.md), which built the key hierarchy
sharing would need without building the per-recipient distribution on top of it.

**6. The Merkle fold order loses its relay input.**
[`../03-crypto/audit-and-tamper-evidence.md`](../03-crypto/audit-and-tamper-evidence.md)
previously folded concurrent ops in `(hlc_clamped, device_id_lex, seq)`, where
`hlc_clamped` clamped the signed HLC to ±5 min around the relay's
`server_first_seen_ms`. The fold order becomes `(hlc, device_id, seq)` —
byte-identical to the comparison key already governing entity-level LWW
([ADR-0016](./0016-hlc-timestamps.md),
`crates/sunrise-storage/migrations/0013_baseline.sql:97-99`). The reason is
narrow and sufficient: the clamp gave the relay an input into the ordering of
the one structure whose entire purpose is detecting what the relay did. An
adversary who can shift `server_first_seen_ms` can shift the fold, and
therefore the root, which makes a divergent root deniable.

`Ack.server_first_seen_ms` stays on the wire
(`crates/sunrise-wire-protocol/src/payloads.rs:93`, stamped at
`api/sync.rs:419`) as an advisory clock-skew hint. It MUST NOT influence merge
order, fold order or acceptance.

**7. The relay evaluates no role, no grant, no revocation and no expiry.** Every
such check is a signature check performed by a receiving client. Documentation
asserting relay-side enforcement is corrected rather than annotated, because an
"also, the server checks" sentence in an E2EE spec is a security claim a reader
will rely on.

## Alternatives considered

| Option | Why rejected |
|---|---|
| **Delete the six specs** | Loses the design of record for work that is deferred, not abandoned. ADR-0020 took exactly this view when it kept `Person`, `Note` and `sunrise-integrations` rather than deleting them (`0020:17-21` of the alternatives table). |
| **Leave them `accepted` with banners only** | This is the status quo, and it is what produced the leak: `README.md:29` makes `accepted` a change-controlled contract, so eight other specs were entitled to cite these numbers. The signal has to be in `status:`, which is machine-checkable, not in prose a citing author never reads. |
| **Keep `billing.md` accepted but delete the numbers** | Yields a specification whose enforcement section describes nothing. The numbers are the only content; it is the *status* that is wrong. |
| **Mark them `superseded`** | Nothing supersedes them. No replacement design exists, and none is being written. `proposed` is the accurate state: design of record, not scheduled. |
| **A separate ADR for the Merkle fold order** | Considered. It is a distinct decision and would ordinarily earn its own record, but it is the same freeze pass and the surrounding ordinals are claimed. Recorded here as clause 6 and amended in place in the spec, so it is findable from both directions. |
| **Ship a reduced managed cloud in v1** | The deferred work is not the Stripe integration; it is per-account accounting, a plan model, quota enforcement paths through every write route, and the operational surface behind them. None exists. Self-host needs none of it. |

## Consequences

**Demoted to `status: proposed`,** each carrying the same four-part banner
(status, what exists in the tree, why it is not v1, what still holds):

* [`../06-server/billing.md`](../06-server/billing.md)
* [`../05-sync/backpressure-and-quotas.md`](../05-sync/backpressure-and-quotas.md)
* [`../05-sync/presence.md`](../05-sync/presence.md)
* [`../07-clients/mobile-android.md`](../07-clients/mobile-android.md)
* [`../05-sync/shared-documents.md`](../05-sync/shared-documents.md)
* [`../04-storage/compaction.md`](../04-storage/compaction.md)

[`../05-sync/crdt-design.md`](../05-sync/crdt-design.md) is demoted in the same
pass, on a different ground: it is a per-field CRDT type catalogue for a merge
model ADR-0014 replaced. Seven files carry `status: proposed` after this ADR;
every other file under `docs/` is `accepted` or `living`.

### `accepted` specs materially edited

Still `accepted`, but changed in ways a reader who cited them needs to know
about. Grouped by what changed.

**Plan-tier and quota claims removed** (§Decision clause 2):
[`../06-server/api.md`](../06-server/api.md),
[`../06-server/observability.md`](../06-server/observability.md),
[`../06-server/overview.md`](../06-server/overview.md),
[`../05-sync/multi-device.md`](../05-sync/multi-device.md),
[`../05-sync/overview.md`](../05-sync/overview.md),
[`deployment-topologies.md`](../01-architecture/deployment-topologies.md),
[`../02-domain/attachments.md`](../02-domain/attachments.md).

**Relay-enforcement claims corrected** (§Decision clause 7):
[`trust-and-server-role.md`](../01-architecture/trust-and-server-role.md),
[`../02-domain/people-and-sharing.md`](../02-domain/people-and-sharing.md),
[`../03-crypto/key-rotation.md`](../03-crypto/key-rotation.md),
[`../03-crypto/sharing-with-others.md`](../03-crypto/sharing-with-others.md).

**Wire and crypto contracts** (each a byte-level statement, so each is listed
individually):
[`../03-crypto/audit-and-tamper-evidence.md`](../03-crypto/audit-and-tamper-evidence.md) —
the Merkle fold order loses its relay-derived clamp and carries an in-place
"Amended (ADR-0027)" block, per §Decision clause 6; this is the amendment named
in §Amends;
[`../05-sync/wire-protocol.md`](../05-sync/wire-protocol.md) —
`Ack.server_first_seen_ms` is declared advisory and barred from influencing
merge order, fold order or acceptance;
[`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md) —
gains the `blob_key` MUST below;
[`../04-storage/blob-store.md`](../04-storage/blob-store.md) — one blob identity,
`BlobChunkId` and the `BlobMeta` CDDL deleted;
[`../02-domain/notes.md`](../02-domain/notes.md) and
[`../01-architecture/threat-model.md`](../01-architecture/threat-model.md) — the
in-app reference and redaction shapes are consolidated on the keys the shipped
codec emits.

**The `conflict` view state is deleted** (consequence below):
[`../07-clients/shared-ui-system.md`](../07-clients/shared-ui-system.md) and the
five feature specs that cited it —
[`focus-mode.md`](../08-features/focus-mode.md),
[`inbox-and-capture.md`](../08-features/inbox-and-capture.md),
[`planning-views.md`](../08-features/planning-views.md),
[`search.md`](../08-features/search.md),
[`time-blocking.md`](../08-features/time-blocking.md) — plus the cross-reference
in [`../05-sync/conflict-resolution.md`](../05-sync/conflict-resolution.md).

**Rewritten to the as-built state**, because the gap was large enough that
patching sentences would have been dishonest:
[`../05-sync/offline-queue.md`](../05-sync/offline-queue.md) (no `attempts` or
`next_retry_at` column; no `applied_seq_range` on the `Ack`; retry state is
in-memory only) and
[`../06-server/observability.md`](../06-server/observability.md) (the metric and
event sets are extracted from source, with the extraction command recorded above
each).

**Testing and versioning contracts**, both changed by the same rewrite and both
normative:
[`../10-cross-cutting/logging.md`](../10-cross-cutting/logging.md) — §6.3
declares `crates/sunrise-server/tests/logging.rs` a MUST and that file does not
exist; the test list and the §11 conformance table now name
`crates/sunrise-server/src/api/observe.rs`'s in-module suite as what carries the
guarantee, which is a change to a testing *requirement*, not a citation fix.
[`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md)
— its published server-metric list named four counters the tree does not define;
it is replaced by the same source extraction, and points at
[`../06-server/observability.md`](../06-server/observability.md) for the
provenance comment.

**Beyond the lists above**, one commit in this change set — the stale-reference
pass — touches twenty-five files (twenty-four `accepted`, one `proposed`), eight
of which are also listed above. What it changes are stale *claims*: statements
the tree refutes, each fixed in place with the evidence, none altering a
contract this ADR decides. They are not enumerated here because the list is that
commit's own `git diff --name-only`; a reader computing the diff of this change
set will find more files than the sections above name, and that is why.

### New normative rules this ADR carries

**A `blob_key` MUST NOT seal two different byte sequences.** The blob chunk
nonce is derived from `blob_key ‖ u32_be(chunk_idx)` and carries no randomness,
so one key over two distinct plaintexts at the same `chunk_idx` is an
XChaCha20-Poly1305 nonce reuse — forfeiting confidentiality of both and leaking
the Poly1305 key. Every sealed byte sequence gets a fresh 32-byte random
`blob_key`. This is a **new MUST in a byte-exact design of record**, not a
clarification: the previous text specified key-sharing dedup and a thumbnail
sharing its parent's key, both of which are the forbidden thing. Its consequence
is that dedup by key sharing does not exist at any scope.

**`Attachment.blob_id` is symmetric, not server-assigned.** Both sides derive it
as the first 16 bytes of BLAKE3 over the concatenated ciphertext chunks; the
relay re-derives from disk rather than trusting the claim
(`crates/sunrise-server/src/api/blobs.rs:222-247,416-425`). This reconciles
`crates/sunrise-domain/src/attachment.rs:42-44` ("assigned by the creating
device") with the relay's content addressing, which had been read as two
competing identities and specified as two.

**The view contract has three states, not four.** The `conflict` state is
deleted rather than left unbuilt: its data source was removed by
[ADR-0018](./0018-storage-baseline-reset.md), and
`baseline_omits_the_dead_schema` fails if `merge_journal` returns. A view cannot
raise a toast about a loss nothing records, so the state cannot be built without
an ADR superseding 0018's removal.

**The status legend gains a third value.** `README.md:38` now reads
`accepted` / `living` / `proposed`, the last defined as "design of record for
work not scheduled in v1".

**`AccountInfo.tier` stays on the wire**, documented as always `"free"`, never
read for a decision, retained for wire compatibility. Removing a response field
is a protocol change and is not worth one here.

**A reader can no longer cite a tier number as a contract.** That is the point
of the demotion, and it is the check this ADR is meant to make cheap: if a
number's home file says `proposed`, quoting it in an `accepted` file is a
defect a grep can find.

**What this does not decide.** Whether managed cloud ships at all; what the
plan model would be if it did; whether presence is worth its leak. Each of
those is a later decision with its own record. This ADR only says none of them
is v1.

## What would force revisiting this

1. **An operator asking to be billed** — that is, managed cloud acquiring a
   user. The re-entry point is clause 2, and the work is per-account accounting
   before it is Stripe.
2. **Multi-device users asking who else is looking at a stream.** Answering it
   requires the leak ADR named in clause 3; presence cannot come back through a
   transport change alone.
3. **Sharing landing** (ADR-0020 §(a)'s trigger 1). `shared-documents.md`
   returns to `accepted` only once the relay-enforcement sentences have been
   replaced by a design that works with a blind relay, and the cross-account
   blob path has an answer that is not "remove the per-account root".
