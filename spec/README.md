# Sunrise — Specification

This directory is the design source of truth for Sunrise: a local-first, end-to-end encrypted productivity system for people running many parallel streams of work (multiple jobs, household, relationships, travel, side projects, etc.).

The implementation in `apps/` and `packages/` is the *previous* prototype and is not load-bearing for these specs. Treat the specs as the target.

## How to read these specs

Read top-down. Each numbered section builds on the prior ones:

| # | Section | What it answers |
|---|---|---|
| 00 | [`00-product/`](./00-product) | Who is Sunrise for? What does it do, and not do? |
| 01 | [`01-architecture/`](./01-architecture) | What are the layers and trust boundaries? |
| 02 | [`02-domain/`](./02-domain) | What entities and relationships do we model? |
| 03 | [`03-crypto/`](./03-crypto) | How is data E2E-encrypted? Keys, recovery, sharing. |
| 04 | [`04-storage/`](./04-storage) | How is data persisted on each client? |
| 05 | [`05-sync/`](./05-sync) | How do clients converge? CRDT, protocol, conflicts. |
| 06 | [`06-server/`](./06-server) | What does the server do (and *not* do)? |
| 07 | [`07-clients/`](./07-clients) | Per-platform: desktop, iOS, Android, web, TUI. |
| 08 | [`08-features/`](./08-features) | User-facing feature specs (capture, planning, focus…) |
| 09 | [`09-integrations/`](./09-integrations) | External: Google Calendar, CalDAV, iCal, webhooks. |
| 10 | [`10-cross-cutting/`](./10-cross-cutting) | A11y, i18n, telemetry, testing, perf. |
| 11 | [`11-adr/`](./11-adr) | Architecture Decision Records — *why* not *what*. |

## Conventions

- **Status legend** at the top of every spec:
  `status: accepted | superseded | deprecated`. Every spec in this directory is **accepted (frozen)** for v1; changes require a superseding ADR.
- **MUST / SHOULD / MAY** follow [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119).
- **Cross-references** use relative paths: `[envelope format](../03-crypto/data-encryption-format.md)`.
- **Diagrams** are ASCII first; Mermaid only if absolutely required for clarity.
- **Wire formats** are specified in CDDL or pseudo-Rust struct form, not prose.
- **Platform-specific** concerns live in `07-clients/<platform>.md`. The shared core spec must be platform-agnostic.

## Glossary

See [`00-product/glossary.md`](./00-product/glossary.md) for shared vocabulary (Stream, Context, Routine, Block, etc.).

## Status

All v1 specs are **accepted**. The cryptography section ([`03-crypto/`](./03-crypto/)) is the cryptographic design of record: algorithms, parameters, wire formats, key lifecycles, and protocol fixtures are byte-exact. Any future change requires a superseding ADR in [`11-adr/`](./11-adr/) and a wire-format version bump.
