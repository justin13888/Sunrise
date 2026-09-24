# Architecture Decision Records

Records of substantive architectural decisions made for Sunrise. Each ADR explains *why* — including the alternatives we rejected — so a future contributor (or future-you) can revisit a decision with the context that led to it.

## Format

Each ADR has:

- **Status:** `proposed | accepted | superseded by NNNN | deprecated`.
- **Context:** what forced a decision now.
- **Decision:** what we picked.
- **Alternatives considered:** with their tradeoffs.
- **Consequences:** what becomes possible or constrained as a result.

ADRs are numbered sequentially (`0001`, `0002`, …) and never renumbered.

## Index

| # | Title | Status |
|---|---|---|
| 0001 | [Bun workspace for app code](./0001-bun-workspace.md) | accepted |
| 0002 | [Shared core in Rust](./0002-shared-core-rust.md) | accepted |
| 0003 | [CRDT: Loro over Automerge](./0003-crdt-loro-vs-automerge.md) | superseded by 0014 |
| 0004 | [Crypto primitives selection](./0004-crypto-primitives.md) | accepted |
| 0005 | [WebSocket as default sync transport](./0005-sync-transport.md) | superseded by 0023 |
| 0006 | [TUI built with Ratatui](./0006-tui-framework.md) | superseded by 0019 |
| 0007 | [Native-per-platform UI vs shared UI framework](./0007-mobile-strategy.md) | accepted (0019 defers every platform but macOS; the shared-core half is unchanged) |
| 0008 | [Local FTS over server-side search](./0008-search-strategy.md) | accepted (amended by 0052) |
| 0009 | [Explicit protocol versioning spec](./0009-protocol-versioning-spec.md) | accepted (amended by 0015: the envelope container is versioned separately from the doc schema); amended by 0045: fingerprints, parking, `vault_requires` |
| 0010 | [Layered structured logging](./0010-logging-strategy.md) | accepted (amended 2026-08: `tracing` carries the transport) |
| 0011 | [Datetime library: jiff](./0011-datetime-jiff.md) | accepted |
| 0012 | [Web WASM core deferred; localStorage stub for v1](./0012-web-wasm-deferred.md) | accepted (amended by 0026: the MSRV revisit trigger has fired; the deferral now rests on the `rusqlite` 0.40 swap alone) |
| 0013 | [Focus session op representation](./0013-focus-session-op-representation.md) | accepted (amended by 0014: OR-Set → append-only row) |
| 0014 | [Entity-level LWW in SQLite is the v1 merge model](./0014-entity-level-lww-merge.md) | superseded by 0044 (per-field ops replace entity-level LWW; was amended by 0016) |
| 0015 | [Envelope container format is versioned separately from the doc schema](./0015-envelope-doc-schema-split.md) | accepted (amended by 0045: container floor tolerance) |
| 0016 | [Hybrid logical clocks order writes](./0016-hlc-timestamps.md) | accepted |
| 0017 | [Scheduled times are a tagged `SunriseTime`](./0017-sunrise-time-representation.md) | accepted |
| 0018 | [Local schema collapses to one pre-1.0 baseline](./0018-storage-baseline-reset.md) | accepted |
| 0019 | [SwiftUI macOS client over a UniFFI seam](./0019-swiftui-macos-client.md) | accepted (supersedes 0006; replaces the Tauri desktop spec; revisit trigger 1 fired — see 0028; amended by 0026: the bindgen quarantine survives, but on feature unification rather than MSRV) |
| 0020 | [Three capabilities leave the v1 MUST set](./0020-v1-must-demotions.md) | accepted (amends the client parity matrix); amended by 0042: the demoted rows are ranked MUSTs again |
| 0021 | [`kynos` + an authoritative OpenAPI 3.2 document replace hand-written axum routing](./0021-kynos-openapi-server.md) | accepted (amended by 0026: its "the workspace pins 1.88.0" is now historical; `spargen`'s floor still clears the pin) |
| 0022 | [`header_sig_v2` signs canonical JSON (RFC 8785)](./0022-device-signature-canonical-json.md) | accepted (forced by 0021) |
| 0023 | [Sync moves to SSE downstream and typed POST upstream](./0023-sse-sync-transport.md) | accepted (supersedes 0005) |
| 0024 | [Stream keys are random and wrapped, not derived from the vault root](./0024-key-hierarchy.md) | accepted (amended by 0046: a private key domain for stream-less data) |
| 0025 | [Integration credentials are a synced entity, not a Stream field](./0025-integration-account-entity.md) | accepted (depends on 0024); amended by 0049: tokens stay on the device that minted them |
| 0026 | [The rustc pin moves to 1.91.1](./0026-msrv-bump.md) | accepted (amends 0012, 0019 and 0021) |
| 0027 | [v1 is self-host-first: managed cloud, billing, quotas, presence, Android and sharing are post-v1](./0027-v1-self-host-first.md) | accepted; amended by 0042: "post-v1" becomes a roadmap rank |
| 0028 | [iOS is a v1 client with its own parity column, at SHOULD level](./0028-ios-is-a-v1-client.md) | accepted (amends the client parity matrix; closes 0019's revisit trigger 1; 0039 takes the channel half of its Decision 6 slot, the MUST-parity half is still reserved); amended by 0042: iOS carries MUSTs in the phone/tablet class |
| 0029 | [Design tokens are compiled from TOML, committed, and drift-checked](./0029-design-token-pipeline.md) | accepted (amends the shared UI system's token section; amended by 0030: the seven light-theme colours below AA are raised and the gate is exhaustive) |
| 0030 | [Every palette colour clears a stated contrast threshold, and the loader enforces it](./0030-palette-contrast-gate.md) | accepted (amends 0029 and the shared UI system's colour section) |
| 0031 | [macOS ships as a Developer ID-signed, notarized `.dmg`, not through the Mac App Store](./0031-macos-distribution.md) | accepted (settles the "under evaluation" in the clients overview and `desktop.md`; answers 0019's unstated distribution question; revisit trigger 4 fired — see 0038) |
| 0032 | [Revocation cannot bound certificate issuance; disclose it rather than half-enforce it](./0032-revocation-cannot-bound-cert-issuance.md) | accepted (amends 0024's revocation guarantee; records why the narrow fixes for #105 are unsound) |
| 0033 | [The relay dedups whole batches, and a re-partitioned re-send is accepted](./0033-relay-batch-dedup-is-whole-batch.md) | accepted (amends the wire protocol's ack-semantics section) |
| 0034 | [Revocation bounds a device's reads, not its writes, and no replica refuses an op](./0034-revocation-bounds-reads-not-writes.md) | accepted (depends on 0024; amends key rotation §Revocation and the threat model's A3; amended by 0041: corollary 3's reservation is taken up for two control ops and the entity-write decision is unchanged; revisit trigger 1 fired — see 0041) |
| 0035 | [The bearer-validity disclosure on `AUTH_DEVICE_SIG_INVALID` is accepted](./0035-bearer-validity-oracle-accepted.md) | accepted (amends server auth §Device binding and the threat model's A2) |
| 0036 | [The HLC is restored from the op log at open, not left to reset](./0036-hlc-restored-at-open.md) | accepted (amends 0016: its "HLC state does not survive a restart" concession is withdrawn; depends on 0014) |
| 0037 | [The account identity is a chain, and membership is derived from its head](./0037-identity-transition.md) | accepted (supersedes 0032's decision and keeps its analysis; corrects key rotation §Identity rotation; depends on 0024, 0034) |
| 0038 | [The macOS app updates itself through Sparkle, and the appcast's EdDSA key is subordinate to the Developer ID certificate in lifecycle, not in authority](./0038-macos-update-feed.md) | accepted (answers 0031's revisit trigger 4; amends `desktop.md` §Update channel and `releasing.md`) |
| 0039 | [A tag uploads an iOS build to TestFlight; App Store submission is a separate, manual act](./0039-ios-distribution.md) | accepted (takes the channel half of 0028's Decision 6 slot and leaves the MUST-parity half reserved; amends the clients overview §Distribution) |
| 0040 | [A predecessor's successor places are held by rank, not by arrival, and are re-judged when the predecessor is established](./0040-sibling-admission-is-a-rank.md) | accepted (amends 0037 §Consequences and key rotation §Verification; depends on 0034, 0037) |
| 0041 | [Peer-side revocation enforcement covers the control ops whose effect can be re-derived, and the register becomes a fold](./0041-peer-side-revocation-is-a-fold.md) | accepted (amends 0034 corollary 3 and key rotation §Revocation and the threat model's A3; closes #82; `STORAGE_V` 28) |
| 0042 | [Sunrise stays at v0.x; compatibility is carried by versioned surfaces, not a product version](./0042-v0-forever.md) | accepted (amends 0020, 0027 and 0028: their release framing becomes roadmap rank; redefines doc status `proposed`) |
| 0043 | [Ops form per-device hash chains with causal heads, and replicas compare a per-stream digest](./0043-commit-tree.md) | proposed |
| 0044 | [Entity ops write fields, not whole entities, and each field merges by its own CRDT type](./0044-per-field-ops.md) | accepted (supersedes 0014) |
| 0045 | [A schema version has a fingerprint, nothing verified is dropped, and a vault declares the features it requires](./0045-schema-identity-and-feature-gating.md) | accepted (amends 0009 and 0015) |
| 0046 | [A task's stream is optional, stream-less content lives in a private key domain, and Inbox is a view](./0046-optional-stream.md) | accepted (amends 0024) |
| 0047 | [A task has a plan time and two deadlines, lateness is derived at read time, and triage is one bulk command](./0047-deadlines-and-lateness.md) | accepted |
| 0048 | [The planner is a pure, deterministic Rust solver that previews every drag as a `PlanDiff` and commits exactly what it previewed](./0048-interactive-planner.md) | accepted |
| 0049 | [Calendar integrations are read-only, each device holds its own OAuth token in its keychain, and the vault syncs configuration and fetched events](./0049-calendar-integrations-per-device-oauth.md) | accepted (amends 0025) |
| 0050 | [Preferences are a vault-synced typed entity with a device overlay, and the day schedule is one of them](./0050-preferences-and-day-schedule.md) | accepted |
| 0051 | [Places are a synced, end-to-end encrypted entity, and presence is evaluated on the device and never stored](./0051-places.md) | accepted |
| 0052 | [Search indexes every entity kind twice (words and trigrams), parses an operator grammar in Rust, and stays inside the vault](./0052-search-v2.md) | accepted (amends 0008) |
| 0053 | [The source device makes a JPEG or PNG thumbnail as its own blob, clients render only what their platform decodes natively, and a core-owned LRU bounds the local copy](./0053-attachment-thumbnails-and-native-rendering.md) | accepted |

## When to write a new ADR

- Picking among substantively different architectural options where future contributors might reasonably wonder why.
- Reversing or superseding a prior decision.
- Adopting an external standard or replacing one.

## When *not* to write one

- Implementation details that don't constrain future decisions.
- Style / naming choices.
- Decisions already obvious from the spec.
