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
| 0012 | [Web WASM core deferred; localStorage stub for v1](./0012-web-wasm-deferred.md) | superseded by 0055 (amended by 0026: the MSRV revisit trigger fired; the `rusqlite` 0.40 swap has since landed) |
| 0013 | [Focus session op representation](./0013-focus-session-op-representation.md) | accepted (amended by 0014: OR-Set → append-only row) |
| 0014 | [Entity-level LWW in SQLite is the v1 merge model](./0014-entity-level-lww-merge.md) | superseded by 0044 (per-field ops replace entity-level LWW; was amended by 0016) |
| 0015 | [Envelope container format is versioned separately from the doc schema](./0015-envelope-doc-schema-split.md) | accepted (amended by 0045: container floor tolerance) |
| 0016 | [Hybrid logical clocks order writes](./0016-hlc-timestamps.md) | accepted |
| 0017 | [Scheduled times are a tagged `SunriseTime`](./0017-sunrise-time-representation.md) | accepted |
| 0018 | [Local schema collapses to one pre-1.0 baseline](./0018-storage-baseline-reset.md) | accepted |
| 0019 | [SwiftUI macOS client over a UniFFI seam](./0019-swiftui-macos-client.md) | accepted (supersedes 0006; replaces the Tauri desktop spec; revisit trigger 1 fired — see 0028; amended by 0026: the bindgen quarantine survives, but on feature unification rather than MSRV) |
| 0020 | [Three capabilities leave the v1 MUST set](./0020-v1-must-demotions.md) | accepted (amends the client parity matrix); amended by 0042: the demoted rows are ranked MUSTs again |
| 0021 | [`kynos` + an authoritative OpenAPI 3.2 document replace hand-written axum routing](./0021-kynos-openapi-server.md) | accepted (amended by 0026: its "the workspace pins 1.88.0" is now historical; `spargen`'s floor still clears the pin) |
| 0022 | [`header_sig_v2` signs canonical JSON (RFC 8785)](./0022-device-signature-canonical-json.md) | accepted (forced by 0021; amended 2026-10: the received value is what is verified, and the sync bodies accept unknown fields) |
| 0023 | [Sync moves to SSE downstream and typed POST upstream](./0023-sse-sync-transport.md) | accepted (supersedes 0005) |
| 0024 | [Stream keys are random and wrapped, not derived from the vault root](./0024-key-hierarchy.md) | accepted (amended by 0046: a private key domain for stream-less data); its "`require_device_sig` at its default `false`" is restated by 0063: on wherever an issuer is configured |
| 0025 | [Integration credentials are a synced entity, not a Stream field](./0025-integration-account-entity.md) | accepted (depends on 0024); amended by 0049: tokens stay on the device that minted them |
| 0026 | [The rustc pin moves to 1.91.1](./0026-msrv-bump.md) | accepted (amends 0012, 0019 and 0021) |
| 0027 | [v1 is self-host-first: managed cloud, billing, quotas, presence, Android and sharing are post-v1](./0027-v1-self-host-first.md) | accepted; amended by 0042: "post-v1" becomes a roadmap rank |
| 0028 | [iOS is a v1 client with its own parity column, at SHOULD level](./0028-ios-is-a-v1-client.md) | accepted (amends the client parity matrix; closes 0019's revisit trigger 1; 0039 takes the channel half of its Decision 6 slot, the MUST-parity half is still reserved); amended by 0042: iOS carries MUSTs in the phone/tablet class |
| 0029 | [Design tokens are compiled from TOML, committed, and drift-checked](./0029-design-token-pipeline.md) | accepted (amends the shared UI system's token section; amended by 0030: the seven light-theme colours below AA are raised and the gate is exhaustive) |
| 0030 | [Every palette colour clears a stated contrast threshold, and the loader enforces it](./0030-palette-contrast-gate.md) | accepted (amends 0029 and the shared UI system's colour section) |
| 0031 | [macOS ships as a Developer ID-signed, notarized `.dmg`, not through the Mac App Store](./0031-macos-distribution.md) | accepted (settles the "under evaluation" in the clients overview and `desktop.md`; answers 0019's unstated distribution question; revisit trigger 4 fired — see 0038) |
| 0032 | [Revocation cannot bound certificate issuance; disclose it rather than half-enforce it](./0032-revocation-cannot-bound-cert-issuance.md) | accepted (amends 0024's revocation guarantee; records why the narrow fixes for #105 are unsound); its "`require_device_sig` at its default `false`" is restated by 0063 |
| 0033 | [The relay dedups whole batches, and a re-partitioned re-send is accepted](./0033-relay-batch-dedup-is-whole-batch.md) | accepted (amends the wire protocol's ack-semantics section) |
| 0034 | [Revocation bounds a device's reads, not its writes, and no replica refuses an op](./0034-revocation-bounds-reads-not-writes.md) | accepted (depends on 0024; amends key rotation §Revocation and the threat model's A3; amended by 0041: corollary 3's reservation is taken up for two control ops and the entity-write decision is unchanged; revisit trigger 1 fired — see 0041); its "`require_device_sig` defaults to false" is restated by 0063, and corollary 3's "defence in depth" now holds on a relay with the binding on |
| 0035 | [The bearer-validity disclosure on `AUTH_DEVICE_SIG_INVALID` is accepted](./0035-bearer-validity-oracle-accepted.md) | accepted (amends server auth §Device binding and the threat model's A2) |
| 0036 | [The HLC is restored from the op log at open, not left to reset](./0036-hlc-restored-at-open.md) | accepted (amends 0016: its "HLC state does not survive a restart" concession is withdrawn; depends on 0014) |
| 0037 | [The account identity is a chain, and membership is derived from its head](./0037-identity-transition.md) | accepted (supersedes 0032's decision and keeps its analysis; corrects key rotation §Identity rotation; depends on 0024, 0034); its "`require_device_sig` at its default `false`" is restated by 0063 |
| 0038 | [The macOS app updates itself through Sparkle, and the appcast's EdDSA key is subordinate to the Developer ID certificate in lifecycle, not in authority](./0038-macos-update-feed.md) | accepted (answers 0031's revisit trigger 4; amends `desktop.md` §Update channel and `releasing.md`) |
| 0039 | [A tag uploads an iOS build to TestFlight; App Store submission is a separate, manual act](./0039-ios-distribution.md) | accepted (takes the channel half of 0028's Decision 6 slot and leaves the MUST-parity half reserved; amends the clients overview §Distribution) |
| 0040 | [A predecessor's successor places are held by rank, not by arrival, and are re-judged when the predecessor is established](./0040-sibling-admission-is-a-rank.md) | accepted (amends 0037 §Consequences and key rotation §Verification; depends on 0034, 0037) |
| 0041 | [Peer-side revocation enforcement covers the control ops whose effect can be re-derived, and the register becomes a fold](./0041-peer-side-revocation-is-a-fold.md) | accepted (amends 0034 corollary 3 and key rotation §Revocation and the threat model's A3; closes #82; `STORAGE_V` 28, then 29 for §Decision 4, a key a read-bounded device delivered is never written under, #280); amended by 0056: an un-revoke is not what settles item 4's residuals; amended by 0058: a revocation the account identity signs settles item 4's two-device residual; revisit trigger 1 fired (`require_device_sig` defaults on with an issuer) — see 0063: the decision stands, and peer-side refusal is defence in depth only on a relay with the binding on |
| 0042 | [Sunrise stays at v0.x; compatibility is carried by versioned surfaces, not a product version](./0042-v0-forever.md) | accepted (amends 0020, 0027 and 0028: their release framing becomes roadmap rank; redefines doc status `proposed`); amended by 0064: §1's product version becomes two train versions, core and app |
| 0043 | [Ops form per-device hash chains with causal heads, and replicas compare a per-stream digest](./0043-commit-tree.md) | proposed |
| 0044 | [Entity ops write fields, not whole entities, and each field merges by its own CRDT type](./0044-per-field-ops.md) | accepted (supersedes 0014; amended by 0065: causal parents, conflict classes, kept losers, emission on) |
| 0045 | [A schema version has a fingerprint, nothing verified is dropped, and a vault declares the features it requires](./0045-schema-identity-and-feature-gating.md) | accepted (amends 0009 and 0015) |
| 0046 | [A task's stream is optional, stream-less content lives in a private key domain, and Inbox is a view](./0046-optional-stream.md) | accepted (amends 0024) |
| 0047 | [A task has a plan time and two deadlines, lateness is derived at read time, and triage is one bulk command](./0047-deadlines-and-lateness.md) | accepted |
| 0048 | [The planner is a pure, deterministic Rust solver that previews every drag as a `PlanDiff` and commits exactly what it previewed](./0048-interactive-planner.md) | accepted |
| 0049 | [Calendar integrations are read-only, each device holds its own OAuth token in its keychain, and the vault syncs configuration and fetched events](./0049-calendar-integrations-per-device-oauth.md) | accepted (amends 0025) |
| 0050 | [Preferences are a vault-synced typed entity with a device overlay, and the day schedule is one of them](./0050-preferences-and-day-schedule.md) | accepted |
| 0051 | [Places are a synced, end-to-end encrypted entity, and presence is evaluated on the device and never stored](./0051-places.md) | accepted |
| 0052 | [Search indexes every entity kind twice (words and trigrams), parses an operator grammar in Rust, and stays inside the vault](./0052-search-v2.md) | accepted (amends 0008) |
| 0053 | [The source device makes a JPEG or PNG thumbnail as its own blob, clients render only what their platform decodes natively, and a core-owned LRU bounds the local copy](./0053-attachment-thumbnails-and-native-rendering.md) | accepted |
| 0054 | [User-visible strings are compiled from one TOML catalog into committed bindings, in a MessageFormat subset every client renders the same way](./0054-string-catalog-pipeline.md) | accepted (amends `i18n.md` §String catalog and §Plural-rule test coverage; follows 0029) |
| 0055 | [The web client runs `sunrise-core` compiled to wasm, over plaintext SQLite in OPFS, one tab at a time](./0055-web-wasm-core.md) | accepted (supersedes 0012) |
| 0056 | [A revocation is withdrawn only by the device that made it, and the withdrawal waits for unknown op kinds to be parked](./0056-a-revocation-is-withdrawn-only-by-its-author.md) | accepted (amends 0041 §"What a user sees" item 4; depends on 0045 §4 and §7; built by #383) |
| 0057 | [Cross-version merges are tested by driving a pinned baseline build out of process](./0057-cross-version-merge-harness.md) | accepted (tests 0042 §2 and 0045; baselines start at the 0042 floor; built by #326) |
| 0058 | [The account identity is the authority for membership, and its revocations are never gated and never discounted](./0058-the-account-identity-is-the-membership-authority.md) | accepted (amends 0041 §"What a user sees" item 4; answers 0056's revisit item 4; depends on 0045 §4 and §7; answers #394) |
| 0059 | [The client op log is folded below an acknowledged floor, and a stream is bootstrapped from a signed snapshot of its merge state](./0059-client-op-log-compaction.md) | accepted (answers 0043's resolved question 8; depends on 0043, 0044 and 0045 §4; built by #330) |
| 0060 | [The relay database is SQLCipher-encrypted under an operator's key file, and backed up online through SQLite's backup API](./0060-relay-database-encryption-at-rest.md) | accepted (supersedes the push-token scheme in `push-notifications.md`; built by #360) |
| 0061 | [An external review audits the key hierarchy, the identity chain and the revocation fold; constant-time comparison is a CI gate; and a pull request that touches the crypto is fuzzed](./0061-crypto-audit-scope.md) | accepted (amends `testing.md` §CI shape; scopes a review of 0022, 0024, 0037 and 0041; built by #365) |
| 0062 | [The relay scales out behind four storage traits, fans out over Postgres `LISTEN/NOTIFY` with the durable log as the only cursor, and keeps sync sessions in a shared table](./0062-relay-scale-out-storage-seam.md) | accepted (amends the managed storage layout in `relay-and-blob-storage.md`; built by #364) |
| 0063 | [Device signatures default on with an issuer, and peer-side revocation enforcement stays: the only line where the relay bound is off, defence in depth where it is on](./0063-peer-side-refusal-stands-where-the-relay-bound-is-in-force.md) | accepted (amends 0041 §Context, §Alternatives (a) and (c) and §Consequences "Why not the relay"; answers 0041's revisit trigger 1; restates the `require_device_sig` default in 0024, 0032, 0034 and 0037; depends on #363) |
| 0064 | [Sunrise releases on two version trains, core and app, and nothing publishes to a registry](./0064-two-version-trains.md) | accepted (amends 0042 §1: the product version becomes the core train version and the app train version; neither carries compatibility meaning) |
| 0065 | [Entities are copy-on-write version DAGs; rows are a fold, and every conflict is kept and named](./0065-copy-on-write-entity-versions.md) | accepted (amends 0044; clarified 2026-10 by its own D1–D18, of which D10 keeps the version rows through compaction for now, D13 marks a group of full-state ops and D16 writes defers and streak keys as `Patch`es between devices that all fold them) |
| 0066 | [Routines follow the traveller by default, and no stored time is ever re-derived from a zone](./0066-times-survive-travel.md) | accepted (amends `time.md` §1's anchor default) |
| 0067 | [Windows secret files get a protected, one-entry DACL, set by the workspace's one `unsafe` module](./0067-windows-owner-only-files.md) | accepted (the one exception to the workspace's `unsafe_code = "forbid"`; built by #486) |
| 0068 | [A stream's snapshot record travels as a blob the relay pins per stream, and a cursor gap is answered by applying it](./0068-a-streams-snapshot-travels-as-a-pinned-blob.md) | accepted (answers 0059 §7; depends on 0059 §4 and §5; built by #462) |
| 0069 | [The account's Stream keys travel as one identity-sealed bundle the relay pins, so a recovery does not depend on op retention](./0069-the-accounts-keys-travel-as-an-identity-sealed-bundle.md) | accepted (amends 0024 decision 4 and `recovery.md` §Recovery flow step 8; depends on 0068 and 0041 §4; built by #580) |

## When to write a new ADR

- Picking among substantively different architectural options where future contributors might reasonably wonder why.
- Reversing or superseding a prior decision.
- Adopting an external standard or replacing one.

## When *not* to write one

- Implementation details that don't constrain future decisions.
- Style / naming choices.
- Decisions already obvious from the spec.
