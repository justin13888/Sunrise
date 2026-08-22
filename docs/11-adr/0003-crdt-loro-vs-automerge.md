# 0003 — CRDT: Loro over Automerge

**Status:** superseded by [0014](./0014-entity-level-lww-merge.md)

> This decision was **never realized**. `crates/sunrise-crdt` was written
> against `loro` but nothing in the workspace ever depended on it; merge has
> always run on entity-level LWW in SQLite. The crate and the `loro` dependency
> are deleted. See [ADR-0014](./0014-entity-level-lww-merge.md) for the merge
> model that actually ships, what it gives up, and what would bring a CRDT
> library back. The comparison below is retained as the record of the original
> evaluation.

## Context

We need a CRDT library that supports map, list, text, counter, and movable-list types. It must run as native Rust and as WASM, with strong perf on mobile.

## Decision

Use **Loro** (Rust, with WASM bindings).

## Alternatives considered

| Option | Pros | Cons |
|---|---|---|
| **Loro** | Rust-native, supports all types we need, compact encoding, active development | Younger; smaller community; less battle-tested than Automerge |
| Automerge (rust port `automerge-rs`) | More mature; well-documented | Heavier op encoding; perf weaker on mobile (in our benchmarks); some types we want are missing or limited |
| Yjs | Excellent for collaborative text editing | TS/JS-first; Rust port lags; less natural for the broader entity model |
| Custom CRDTs | Tailored | Re-inventing the wheel; correctness risk in concurrent edge cases |

## Consequences

- We get all the CRDT shapes we need from a single library.
- Smaller wire size and faster apply on phones.
- We accept Loro's youth: pin a version, run our convergence property tests on every upgrade, contribute upstream where bugs surface.
- We are insulated by our envelope wrapper: if a future migration to a different CRDT becomes necessary, only the inner-op codec needs to change; the envelope, sync protocol, and storage layers are independent.
- Test investment: extensive convergence and round-trip tests (see [`../10-cross-cutting/testing.md`](../10-cross-cutting/testing.md)).
