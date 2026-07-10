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
| 0003 | [CRDT: Loro over Automerge](./0003-crdt-loro-vs-automerge.md) | accepted |
| 0004 | [Crypto primitives selection](./0004-crypto-primitives.md) | accepted |
| 0005 | [WebSocket as default sync transport](./0005-sync-transport.md) | accepted |
| 0006 | [TUI built with Ratatui](./0006-tui-framework.md) | accepted |
| 0007 | [Native-per-platform UI vs shared UI framework](./0007-mobile-strategy.md) | accepted |
| 0008 | [Local FTS over server-side search](./0008-search-strategy.md) | accepted |
| 0009 | [Explicit protocol versioning spec](./0009-protocol-versioning-spec.md) | accepted |
| 0010 | [Layered structured logging](./0010-logging-strategy.md) | accepted |

## When to write a new ADR

- Picking among substantively different architectural options where future contributors might reasonably wonder why.
- Reversing or superseding a prior decision.
- Adopting an external standard or replacing one.

## When *not* to write one

- Implementation details that don't constrain future decisions.
- Style / naming choices.
- Decisions already obvious from the spec.
