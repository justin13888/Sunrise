# Sunrise — Design

This directory is the design source of truth for Sunrise: a local-first, end-to-end encrypted productivity system for people running many parallel streams of work (multiple jobs, household, relationships, travel, side projects, etc.).

The live v1 implementation is `crates/` (shared Rust core) plus `apps/` (clients and server). The earlier prototype now lives in `legacy/` and is not load-bearing for this design.

This tree fully covers the design from first principles; every design decision is asserted with a brief justification; exact dependency pins live in `01-architecture/dependencies.md` (and the ADRs).

## How to read

Read top-down, 00 → 11, then `implementation/`. Each numbered section builds on the prior ones. The section list may grow as the design expands.

| # | Section | What it covers |
|---|---|---|
| 00 | [`00-product/`](./00-product) | Who Sunrise is for; what it does and does not do. |
| 01 | [`01-architecture/`](./01-architecture) | Layers, trust boundaries, deployment; `dependencies.md` pins exact versions. |
| 02 | [`02-domain/`](./02-domain) | Entities and relationships we model. |
| 03 | [`03-crypto/`](./03-crypto) | E2E encryption: keys, recovery, sharing, wire formats. |
| 04 | [`04-storage/`](./04-storage) | How data is persisted on each client. |
| 05 | [`05-sync/`](./05-sync) | How clients converge: CRDT, transport, protocol, conflicts. |
| 06 | [`06-server/`](./06-server) | What the server does (and does not). |
| 07 | [`07-clients/`](./07-clients) | Per-platform: desktop, iOS, Android, web, TUI. |
| 08 | [`08-features/`](./08-features) | User-facing feature specs (capture, planning, focus…). |
| 09 | [`09-integrations/`](./09-integrations) | External: Google Calendar, iCalendar import/export. |
| 10 | [`10-cross-cutting/`](./10-cross-cutting) | A11y, i18n, telemetry, logging, versioning, testing, perf. |
| 11 | [`11-adr/`](./11-adr) | Architecture Decision Records — *why*, not *what*. |
| — | [`implementation/`](./implementation) | Living tracker of the build against this design. |

[`03-crypto/`](./03-crypto) is the cryptographic design of record: algorithms, parameters, wire formats, key lifecycles, and protocol fixtures are byte-exact. Any change to an `accepted` doc requires a superseding ADR in [`11-adr/`](./11-adr), and wire-format changes additionally require a version bump per [`10-cross-cutting/protocol-versioning.md`](./10-cross-cutting/protocol-versioning.md).

Two cross-cutting substrates every section depends on:

- [`10-cross-cutting/logging.md`](./10-cross-cutting/logging.md) — layered structured-logging contract ([ADR-0010](./11-adr/0010-logging-strategy.md)).
- [`10-cross-cutting/protocol-versioning.md`](./10-cross-cutting/protocol-versioning.md) — version negotiation, capability bits, deprecation policy ([ADR-0009](./11-adr/0009-protocol-versioning-spec.md)).

## Conventions

- **Status legend** — YAML frontmatter on every doc: `status: accepted` (frozen design) or `status: living` (tracks implementation). ADRs use a `**Status:** accepted` line instead.
- **MUST / SHOULD / MAY** follow [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119).
- **Cross-references** use relative paths: `[envelope format](./03-crypto/data-encryption-format.md)`.
- **Diagrams** are ASCII first.
- **Wire formats** are specified in CDDL or pseudo-Rust struct form, not prose.
- **Platform-specific** concerns live in `07-clients/<platform>.md`; the shared core design stays platform-agnostic.

See [`00-product/glossary.md`](./00-product/glossary.md) for shared vocabulary (Stream, Context, Routine, Block, etc.).
