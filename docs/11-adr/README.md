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
| 0005 | [WebSocket as default sync transport](./0005-sync-transport.md) | accepted |
| 0006 | [TUI built with Ratatui](./0006-tui-framework.md) | superseded by 0019 |
| 0007 | [Native-per-platform UI vs shared UI framework](./0007-mobile-strategy.md) | accepted (0019 defers every platform but macOS; the shared-core half is unchanged) |
| 0008 | [Local FTS over server-side search](./0008-search-strategy.md) | accepted |
| 0009 | [Explicit protocol versioning spec](./0009-protocol-versioning-spec.md) | accepted (amended by 0015: the envelope container is versioned separately from the doc schema) |
| 0010 | [Layered structured logging](./0010-logging-strategy.md) | accepted |
| 0011 | [Datetime library: jiff](./0011-datetime-jiff.md) | accepted |
| 0012 | [Web WASM core deferred; localStorage stub for v1](./0012-web-wasm-deferred.md) | accepted |
| 0013 | [Focus session op representation](./0013-focus-session-op-representation.md) | accepted (amended by 0014: OR-Set → append-only row) |
| 0014 | [Entity-level LWW in SQLite is the v1 merge model](./0014-entity-level-lww-merge.md) | accepted (amended by 0016: comparison key is now `(hlc, device_id, seq)`) |
| 0015 | [Envelope container format is versioned separately from the doc schema](./0015-envelope-doc-schema-split.md) | accepted |
| 0016 | [Hybrid logical clocks order writes](./0016-hlc-timestamps.md) | accepted |
| 0017 | [Scheduled times are a tagged `SunriseTime`](./0017-sunrise-time-representation.md) | accepted |
| 0018 | [Local schema collapses to one pre-1.0 baseline](./0018-storage-baseline-reset.md) | accepted |
| 0019 | [SwiftUI macOS client over a UniFFI seam](./0019-swiftui-macos-client.md) | accepted (supersedes 0006; replaces the Tauri desktop spec) |

## When to write a new ADR

- Picking among substantively different architectural options where future contributors might reasonably wonder why.
- Reversing or superseding a prior decision.
- Adopting an external standard or replacing one.

## When *not* to write one

- Implementation details that don't constrain future decisions.
- Style / naming choices.
- Decisions already obvious from the spec.
