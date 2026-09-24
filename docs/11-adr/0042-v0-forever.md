# 0042 — Sunrise stays at v0.x; compatibility is carried by versioned surfaces, not a product version

**Status:** accepted

**Amends** [ADR-0020](./0020-v1-must-demotions.md),
[ADR-0027](./0027-v1-self-host-first.md) and
[ADR-0028](./0028-ios-is-a-v1-client.md). Their technical content stands. What
changes is the release framing each one hangs that content on: every "MUST for
v1", "SHOULD for v1", "v1 if feasible, otherwise v1.x", "post-v1" and
"deferred" in them is read from here on as a **rank on the roadmap**
([`../roadmap.md`](../roadmap.md)), not as membership of a release.

**Redefines** the `proposed` document status in
[`../README.md`](../README.md) §Conventions.

**Does not rewrite** any earlier ADR. ADR bodies are history. Where an earlier
record says "v1", "pre-1.0" or "after 1.0", this record is the amendment, and
the older text is read through it.

## Context

The design tree was written toward a release called v1. Three ADRs made that
release load-bearing:

- **ADR-0020** removed three capabilities from "the v1 MUST set" and justified
  doing so because v1 had not shipped: "a v1.0 → v1.1 release cannot remove a
  MUST" governs a released product, and there was not one.
- **ADR-0027** made v1 self-host-first. It demoted seven specs to
  `status: proposed`, which [`../README.md`](../README.md) then defined as
  "design of record for work not scheduled in v1".
- **ADR-0028** added iOS as a v1 client at SHOULD level ("v1 if feasible,
  otherwise v1.x"). It reserved MUST parity for a separate ADR, to be written
  when an iOS release is cut.

Several other records also lean on "pre-1.0" as a licence to break
compatibility. [ADR-0018](./0018-storage-baseline-reset.md) collapsed the local
schema because no vault existed yet at the old versions.
[`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md)
accepted new op families (`DOC_SCHEMA_V` 3, 5 and 6) and a reshaped delete op
(`DOC_SCHEMA_V` 4) that older builds cannot decode, because "no such build
exists".

Two things have changed.

1. **The owner has decided the product version stays at v0.x permanently.**
   There will be no 1.0. Release gates and deferral labels keyed to it describe
   an event that will not happen, so a reader cannot tell a real priority from
   leftover framing.
2. **Builds now exist in the field.** `v0.1.0-rc.1` is tagged, and the macOS
   app has a self-update channel ([ADR-0038](./0038-macos-update-feed.md)).
   "No older build exists" is no longer true of anything. So a version-stage
   licence to break compatibility is not merely unneeded, it is false.

The owner's invariant replaces both:

> **Merging vaults across client versions MUST NEVER break and MUST NEVER lose
> data.**

A product version number cannot enforce that. Only the versioned surfaces the
bytes actually carry can.

## Decision

### 1. The product version is `0.MINOR.PATCH`, forever

Sunrise's app version stays below 1.0 with no end date. `MINOR` and `PATCH`
mean what the release notes say they mean. **They carry no compatibility
meaning.** No component MAY make a compatibility decision from a product
version string. `Hello.client_app_v` and `HelloAck.server_app_v` are
diagnostics
([`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md)
§4). A peer MUST NOT gate, refuse or degrade on them.

Documents MUST NOT describe work as "for v1", "v1.x", "post-v1", "after 1.0",
"pre-1.0" or "the v1 launch", and MUST NOT use "deferred" as a release
category. The order in which work happens is expressed in one place, the
roadmap (§3).

### 2. Compatibility is carried by versioned surfaces

The surfaces are the ones
[`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md)
enumerates:

- `WIRE_PROTO_V`
- `ENVELOPE_FORMAT_V`
- `DOC_SCHEMA_V`, with its floor and, from
  [ADR-0045](./0045-schema-identity-and-feature-gating.md), its fingerprint
- `CRYPTO_SUITE_V`
- `STORAGE_V`

Two finer-grained mechanisms sit on top of them: the capability bits, and the
`vault_requires` feature ids from ADR-0045. Every compatibility question is
answered by one of these, and each has its own evolution rule.

**No version-stage licence exists.** A change MUST NOT be justified by "no
older build exists" or "Sunrise is pre-release". The pre-1.0 arguments in
ADR-0018 and in `protocol-versioning.md` stay true as a record of why those
past changes were made. They MUST NOT be cited as precedent for a new one. A
change that an older build cannot read goes through the parking and
feature-gating rules of ADR-0045. If it cannot go through them, it needs its
own ADR explaining how the invariant above still holds.

### 3. The roadmap is a ranked board, mirrored in `docs/roadmap.md`

The order of work lives in the GitHub Project board **"Sunrise App"**
(<https://github.com/users/justin13888/projects/1>). The board has two fields:

- **Phase**, from `P0` to `P6`:
  1. P0: Integrity
  2. P1: Commit tree and schema
  3. P2: Domain model
  4. P3: macOS complete
  5. P4: Server production
  6. P5: iOS parity
  7. P6: Platforms
- **Rank**, a strict order within the board.

[`../roadmap.md`](../roadmap.md) mirrors the phase order and links the board.
When the two disagree, the board is authoritative for order, and `roadmap.md`
is corrected.

Applied to the three amended ADRs:

| ADR | Its framing | Read as |
|---|---|---|
| [0020](./0020-v1-must-demotions.md) | Stream sharing and Google Calendar "leave the v1 MUST set, deferred" | Both are roadmap items. Sharing is `P6`; calendars are `P3`, whose calendar design ADR-0049 supersedes the deferral. The analysis of why each was not buildable at the time stands. |
| [0027](./0027-v1-self-host-first.md) | Managed cloud, billing, quotas, presence, Android and sharing are "post-v1"; seven specs become `proposed` | Each is either ranked on the roadmap or out of scope. Out of scope is a product decision, recorded in [`../00-product/`](../00-product/), not a version. The seven specs stay `proposed`, under the meaning in §4. Clauses 6 and 7 (the Merkle fold order, and the relay evaluating no role or grant) are not release framing, and they stand unchanged. |
| [0028](./0028-ios-is-a-v1-client.md) | iOS is "a v1 client at SHOULD level", with MUST parity reserved for "when an iOS release is cut" | iOS parity is phase `P5`. The parity matrix's MUST and SHOULD columns become per-device-class parity targets ([`../07-clients/parity-matrix.md`](../07-clients/parity-matrix.md)). The reserved MUST-parity ADR slot becomes the parity target for the phone and tablet device class. |

The regression rules in ADR-0020 and ADR-0028 survive in this form, and they no longer
depend on a release. **A capability graded *met* for a device class MUST NOT
regress silently.** The pull request that takes a met row back to unmet MUST
update that audit row in the same pull request and file a ranked issue to
restore it. It needs no ADR
([`../07-clients/parity-matrix.md`](../07-clients/parity-matrix.md) §Hard
rules).

### 4. `proposed` means "design of record, not yet built, ranked"

A document with `status: proposed` is **a design of record that has not been
built yet and is ranked on the roadmap ([`../roadmap.md`](../roadmap.md))**.
The two rules that already apply to `proposed` stay:

- A number quoted from a `proposed` doc is not a contract.
- Citing one from an `accepted` doc as binding is a defect.

What changes is where the status points. It used to mean "not in v1". It now
means "not built yet, and here is where it sits in the queue". A `proposed`
doc SHOULD name the roadmap item or issue that builds it.
[ADR-0043](./0043-commit-tree.md) is the first ADR written directly under this
meaning.

### 5. Protocol identifiers containing `v1` are constants, not product versions

Some wire and crypto identifiers contain `v1` or `v2`. Each one names a version
of **that identifier's own format**, and none of them will ever be renamed to
match a product version. Renaming any of them is a wire break, or a
`CRYPTO_SUITE_V` change, or both:

- **HTTP API paths.** `/api/v1/...`, and the committed contract
  `schemas/generated/openapi.v1.json`
  ([ADR-0021](./0021-kynos-openapi-server.md)).
- **Log schema.** `schemas/log-record.v1.json`, the `log-record.v1` schema id
  ([`../10-cross-cutting/logging.md`](../10-cross-cutting/logging.md)).
- **Domain-separation strings.** Every `sunrise.<purpose>.vN` string used as a
  signature domain, KDF context, AAD prefix or hash personalization, for
  example `sunrise.op_envelope.v1` (`SIG_DOMAIN` in `crates/sunrise-crypto/src/op_envelope.rs`),
  `sunrise.stream_root.init.v1`, and `sunrise.blob_chunk_nonce.v2`. The full
  set is whatever `git grep 'sunrise\.[a-z0-9_.]*\.v[0-9]'` finds in `crates/`,
  and [`../03-crypto/primitives.md`](../03-crypto/primitives.md) is the
  registry.
- **Signature header names.** `header_sig_v2`
  ([ADR-0022](./0022-device-signature-canonical-json.md)).
- **The pairing QR field `magic_v1`**
  (`crates/sunrise-pairing/src/qr.rs#QrPayload`).
- **Magic-prefix structure versions.** For example, recovery blob format `1`
  ([`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md)
  §3).
- **Fixture names that describe schema relationships.** For example,
  `tests/fixtures/forward-compat/v1-reads-v2.cbor`, where `v1` and `v2` are
  document-schema generations.

A document that mentions one of these identifiers keeps it verbatim. The
repo-wide check for leftover product-version framing allowlists exactly this
set.

## Alternatives considered

| Option | Why rejected |
|---|---|
| **Ship a 1.0 once the backlog is clear, and freeze the product version's meaning at that point** | A 1.0 would carry its compatibility promise in a number a reader cannot check against bytes. The invariant has to hold between any two builds, so it has to be carried by what the bytes carry. |
| **Keep the v1/v1.x labels as priority buckets** | Two buckets cannot order around fifty items. The labels also kept implying a release gate that does not exist. A ranked board says what the labels meant, and says it more precisely. |
| **Rewrite ADR-0020, 0027 and 0028 in place** | ADRs are history. Rewriting them would erase why sharing, calendars and iOS were scoped the way they were, and that reasoning is still correct about the tree it describes. |
| **Drop `proposed` and mark unbuilt specs `accepted` with banners** | ADR-0027 already found that banners on `accepted` docs cannot be told apart from contracts. The status has to carry the signal. |
| **v0.x forever, surfaces carry compatibility, board carries order (chosen)** | Each question has exactly one place where it is answered. |

## Consequences

- **The repo-wide scrub is licensed.** Product-version framing is removed from
  docs, the README and code comments. Wire, crypto and schema identifiers
  (§5) are kept.
- **Every compatibility-breaking change now needs a mechanism, not an
  excuse.** The four op-family and op-shape changes so far (`DOC_SCHEMA_V`
  3, 4, 5 and 6, listed in
  [`../02-domain/schema-versioning.md`](../02-domain/schema-versioning.md) and
  [`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md))
  relied on the pre-1.0 licence. The next one relies on parking, lossless unknowns and
  feature gating ([ADR-0045](./0045-schema-identity-and-feature-gating.md)),
  and on per-field merge ([ADR-0044](./0044-per-field-ops.md)). Until those
  land, a new op family MUST NOT ship. The ranking puts P1 ahead of the domain
  work that would add new op families (P2), for this reason.
- **The `v1.x` GitHub label is retired.** Deferral notes on issues are
  replaced by the issue's Phase and Rank on the board.
- **Parity is per device class, not per release.** A desktop capability met on
  macOS is a parity target for every desktop client. A phone capability met on
  iOS is a parity target for every phone client.
  [`../07-clients/parity-matrix.md`](../07-clients/parity-matrix.md) carries the
  targets.

## What would force revisiting this

1. **A distribution channel that requires a 1.x version.** For example, a store
   that treats 0.x as beta and restricts it. The answer would be a
   channel-specific marketing version, not a change to what carries
   compatibility.
2. **A compatibility question that none of the surfaces in §2 can answer.**
   That calls for a new surface with its own ADR, not a product version.
