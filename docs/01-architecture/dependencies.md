---
status: living
---

# Dependency Pins

`living`, because this file tracks resolved versions that move with `Cargo.lock` and the workspace manifest; it is the normative pin registry, not a frozen design doc.

This is the source of truth for *which* third-party crates the Rust workspace
depends on and *why*. Ranges are declared in the root `Cargo.toml`
`[workspace.dependencies]`; the **Version** column below is the value actually
resolved in `Cargo.lock`. When the lockfile moves, this table moves with it —
but any change governed by an ADR or an `accepted` spec still requires the
superseding decision named in the **Governing decision** column.

## Rust workspace pins

| Purpose | Crate | Version (Cargo.lock) | Governing decision | Notes |
|---|---|---|---|---|
| Signatures (identity, device keys) | `ed25519-dalek` | 2.2.0 | [ADR-0004](../11-adr/0004-crypto-primitives.md) | Frozen crypto suite; consumed only by `sunrise-crypto`. |
| Key agreement (X25519 / DHKEM) | `x25519-dalek` | 2.0.1 | [ADR-0004](../11-adr/0004-crypto-primitives.md) | Frozen; `sunrise-crypto` only. |
| AEAD (XChaCha20-Poly1305) | `chacha20poly1305` | 0.10.1 | [ADR-0004](../11-adr/0004-crypto-primitives.md) | Frozen; `sunrise-crypto` only. |
| Public-key encryption (key envelopes, share grants) | `hpke` | *declared 0.13; not in lock* | [ADR-0004](../11-adr/0004-crypto-primitives.md), [primitives.md](../03-crypto/primitives.md) | Declared in `[workspace.dependencies]` but unconsumed — pending the sharing key-envelope implementation. Not yet resolved into `Cargo.lock` because no crate references it. See Reconciliations §b. |
| Handshake (Noise) | `snow` | 0.9.6 | [ADR-0004](../11-adr/0004-crypto-primitives.md), [pairing-and-onboarding.md](../03-crypto/pairing-and-onboarding.md) | `sunrise-pairing` only. |
| Hashing / KDF context | `blake3` | 1.8.5 | [ADR-0004](../11-adr/0004-crypto-primitives.md) | Content hashing + SQLCipher key derivation. |
| Password hashing | `argon2` | 0.5.3 | [ADR-0004](../11-adr/0004-crypto-primitives.md) | Argon2id at unlock; `sunrise-crypto` only. |
| Local database | `rusqlite` | 0.31.0 | [local-database.md](../04-storage/local-database.md) | `bundled-sqlcipher` feature — statically links SQLCipher v4 (encrypted SQLite); no system SQLite dependency. |
| CBOR serialization | `ciborium` | 0.2.2 | [wire-protocol.md](../05-sync/wire-protocol.md), [data-encryption-format.md](../03-crypto/data-encryption-format.md) | Canonical op / envelope encoding. |
| HTTP + WebSocket server | `axum` | 0.7.9 | [ADR-0005](../11-adr/0005-sync-transport.md) | Relay REST + `/sync` WS; `sunrise-server`. |
| Async runtime | `tokio` | 1.52.3 | — (runtime substrate) | Multi-thread runtime for server + sync driver. |
| WebSocket client transport | `tokio-tungstenite` | 0.24.0 | [ADR-0005](../11-adr/0005-sync-transport.md) | Sync transport; `rustls-tls-webpki-roots`. |
| TLS | `rustls` | 0.23.40 | [ADR-0005](../11-adr/0005-sync-transport.md) | `ring` backend, no OpenSSL. |
| TUI rendering | `ratatui` | 0.28.1 | [ADR-0006](../11-adr/0006-tui-framework.md) | `sunrise-tui`. |
| Terminal backend | `crossterm` | 0.28.1 | [ADR-0006](../11-adr/0006-tui-framework.md) | `sunrise-tui`. |
| TUI image preview | `ratatui-image` | 2.0.1 | [ADR-0006](../11-adr/0006-tui-framework.md) | `sunrise-tui` only, behind the default-on `images` feature (`:preview <path>` in Focus). Pinned to the `2.0` line: it targets `ratatui ^0.28.1`; `3.x`+ requires a newer ratatui. Features `crossterm` + `rustix`, halfblocks fallback. Its transitive `icy_sixel` `0.1.3` ships **no `license` field** in its manifest (upstream README states MIT), so `cargo deny check licenses` emits an unlicensed-crate warning for it — accepted as MIT pending an upstream fix. |
| Image decoding (TUI preview) | `image` | 0.25.10 | [ADR-0006](../11-adr/0006-tui-framework.md) | `sunrise-tui` only, `images` feature. `default-features = false` + `png` + `jpeg` only, to keep the decoder surface small. |
| Frame compression | `zstd` | 0.13.3 | [wire-protocol.md](../05-sync/wire-protocol.md) | Wire-frame payload compression. |
| Structured logging | *none (hand-written)* | — | [ADR-0010](../11-adr/0010-logging-strategy.md), [logging.md](../10-cross-cutting/logging.md) | `sunrise-log` is hand-rolled (NDJSON, `Plain<T>` redaction); `tracing` 0.1 is declared but enters the lock only transitively via axum's `tracing` feature. |
| Property-based testing | `proptest` | 1.11.0 | [testing.md](../10-cross-cutting/testing.md) | Convergence / redaction / round-trip proptests. |
| Snapshot testing | `insta` | 1.48.0 | [testing.md](../10-cross-cutting/testing.md) | Now consumed by `sunrise-tui`'s golden-frame render snapshots (declared `1.40`, resolved to `1.48.0` in `Cargo.lock`); `yaml` feature. See Reconciliations §e. |
| Benchmarking | `criterion` | 0.5.1 | [testing.md](../10-cross-cutting/testing.md) | `sunrise-bench` only. `default-features = false` + `cargo_bench_support`; drives the submit / query_today@10k / fts@10k / ws-handshake benches that feed `bench/baseline.json`. |
| Deterministic seeded RNG | `rand_chacha` | 0.3.1 | [testing.md](../10-cross-cutting/testing.md) | ChaCha20 CSPRNG seeded for reproducibility. Direct dependency of `sunrise-crypto`, `sunrise-onboarding`, `sunrise-crypto-test-vectors`, `sunrise-e2e` (seeded chaos transport), and `sunrise-bench` (fixture generation). |
| Datetime | `jiff` | 0.2.32 | [ADR-0011](../11-adr/0011-datetime-jiff.md) | Sole datetime library. `jiff::Timestamp` for absolute instants; civil/`Zoned` types available for wall-clock and tz-aware semantics. The chrono→jiff migration landed; `chrono` and the unused `time` dependency were removed. See Reconciliations §d. |
| RRULE parsing | *none (hand-written)* | — | [recurrence-engine.md](../08-features/recurrence-engine.md), [routines-and-recurrence.md](../02-domain/routines-and-recurrence.md) | Hand-written parser at `crates/sunrise-domain/src/rrule.rs`. See Reconciliations §c. |

Supporting utility crates (`serde`, `thiserror`, `anyhow`, `hyper`, `tower`,
`tower-http`, `subtle`, `zeroize`, `rand`, `parking_lot`, etc.) follow their
declared caret ranges in `Cargo.toml` and are not individually pinned here;
they carry no independent design decision.

## Bun workspace pins

Majors only; exact ranges live in the per-package `package.json` files.

| Purpose | Package | Major | Notes |
|---|---|---|---|
| UI framework | `react` / `react-dom` | 18 | `apps/web`, `apps/desktop`. |
| Web bundler / dev server | `vite` | 5 | `apps/web`. |
| Language | `typescript` | 5.8 | Workspace-wide (`~5.8.3`). |
| Lint / format | `@biomejs/biome` | 2 | Root dev dependency. |
| Unit testing | `vitest` | 4 | Root + coverage-v8. |

## Reconciliations

Five places where doc prose had drifted from the real manifest / lockfile.
Each is now reconciled to reality.

### a. `loro` — removed; there is no CRDT library

An earlier revision of this table pinned `loro 1.12.0` as the "CRDT document
store". [ADR-0014](../11-adr/0014-entity-level-lww-merge.md) supersedes ADR-0003:
the Loro layer (`crates/sunrise-crdt`) was never depended on by anything, merge
runs on entity-level LWW in SQLite, and both the crate and the `loro` entry in
`[workspace.dependencies]` are deleted. **No CRDT library is in the dependency
graph.** Removing it also dropped the `im` / `bitmaps` / `sized-chunks` /
`atomic-polyfill` RUSTSEC suppressions from `deny.toml`.

### b. `hpke` — declared, not yet consumed

`docs/03-crypto/primitives.md` previously wrote `hpke 0.11.x`. The workspace
actually declares **`hpke = { version = "0.13", ... }`**. No crate consumes it
yet — `grep` over `crates/` finds no `use hpke` and no member `Cargo.toml`
referencing it — so it does **not** appear in `Cargo.lock`. State: *declared,
pending the sharing key-envelope implementation.* `primitives.md` has been
aligned to `0.13`.

### c. `rrule` — hand-written parser, not a crate

`docs/08-features/recurrence-engine.md` previously named a third-party crate
(`rrule`) pinned via `Cargo.toml` as the RRULE library. Reality: recurrence
parsing is a
**hand-written parser at `crates/sunrise-domain/src/rrule.rs`**. This is
deliberate:

- the v1 supported subset is narrow (`FREQ`, `INTERVAL`, `BYDAY`, `BYMONTHDAY`,
  `BYMONTH`, `BYSETPOS`, `COUNT`, `UNTIL`, `WKST`), so a full RFC 5545 engine is
  unnecessary;
- it adds **zero unvetted transitive dependencies** to a security-frozen
  workspace (`unsafe_code = "forbid"`, deny-listed types);
- it lets us emit an **exact error taxonomy** aligned with the domain spec
  rather than mapping a third-party crate's errors.

The table row above reflects this (Purpose: RRULE parsing → *none
(hand-written)*).

### d. `chrono` / `time` → `jiff`

Per [ADR-0011](../11-adr/0011-datetime-jiff.md), **`jiff` `0.2.32`** is the sole
datetime library. The migration **landed**:

- `jiff` is declared in `[workspace.dependencies]` (`0.2`, resolving to
  `0.2.32` in `Cargo.lock`) with `default-features = false` plus `std`,
  `serde`, and `tzdb-bundle-platform`.
- `chrono` was **removed** — no `chrono::` usage remains and it is absent
  from `Cargo.lock`.
- `time` (declared `0.3`, never consumed) was **removed** with it.

Storage stays epoch-ms integers and op CBOR stays RFC 3339 strings, so neither
the on-disk nor the wire format changed. A permanent wire-compat test
(`crates/sunrise-domain/tests/serde_compat.rs`) decodes the pre-migration
chrono-era CBOR fixtures under the jiff types; `jiff::Timestamp` and
`chrono::DateTime<Utc>` serialize a UTC instant to the identical RFC 3339
string, so the canonical bytes are byte-identical.

### e. `insta` — now consumed, so now in the lockfile

`insta` was previously *declared but unconsumed* (absent from `Cargo.lock`).
It is now a dev-dependency of `sunrise-tui`, which uses it for golden-frame
render snapshots of the TUI views. Declared as `1.40` in
`[workspace.dependencies]`, it resolves to **`1.48.0`** in `Cargo.lock`. The
table row above reflects the resolved version.
