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
| Rust toolchain | `rustc` (not a crate) | 1.91.1 | [ADR-0026](../11-adr/0026-msrv-bump.md) | Not a lockfile entry: `channel` in `rust-toolchain.toml` and `rust-version` in the root `Cargo.toml`, kept **byte-identical**, plus `ARG RUST_VERSION` in the `Dockerfile`. A reproducibility floor rather than a dependency workaround, raised from 1.88.0 to clear three things at once: `cargo-platform` 0.3.3 (≥ 1.91), `libsqlite3-sys` 0.38.1's `cfg_select!` (≥ 1.91, ADR-0012's blocker) and `std::fs::File::try_lock` (≥ 1.89, which retired `fs4`). The first step of CI's `rust` job asserts all four against each other: the toolchain file, `rust-version`, the Dockerfile's `ARG RUST_VERSION`, and the `rustc` a checkout resolves to. Raising it again requires an ADR. |
| Signatures (identity, device keys) | `ed25519-dalek` | 2.2.0 | [ADR-0004](../11-adr/0004-crypto-primitives.md) | Frozen crypto suite; consumed only by `sunrise-crypto`. |
| Key agreement (X25519 / DHKEM) | `x25519-dalek` | 2.0.1 | [ADR-0004](../11-adr/0004-crypto-primitives.md) | Frozen; `sunrise-crypto` only. |
| AEAD (XChaCha20-Poly1305) | `chacha20poly1305` | 0.10.1 | [ADR-0004](../11-adr/0004-crypto-primitives.md) | Frozen; `sunrise-crypto` only. |
| Public-key encryption (key envelopes, share grants) | `hpke` | *declared 0.13; not in lock* | [ADR-0004](../11-adr/0004-crypto-primitives.md), [primitives.md](../03-crypto/primitives.md) | Declared in `[workspace.dependencies]` but unconsumed — pending the sharing key-envelope implementation. Not yet resolved into `Cargo.lock` because no crate references it. See Reconciliations §b. |
| Handshake (Noise) | `snow` | 0.9.6 | [ADR-0004](../11-adr/0004-crypto-primitives.md), [pairing-and-onboarding.md](../03-crypto/pairing-and-onboarding.md) | `sunrise-pairing` only. |
| Hashing / KDF context | `blake3` | 1.8.5 | [ADR-0004](../11-adr/0004-crypto-primitives.md) | Content hashing + SQLCipher key derivation. |
| Password hashing | `argon2` | 0.5.3 | [ADR-0004](../11-adr/0004-crypto-primitives.md) | `sunrise-crypto` only, and on **one** path: stretching the recovery code in `recovery.rs`. Not used at unlock — there is no passphrase unlock; the vault root is 32 random bytes from a keystore. See [`../03-crypto/identity-and-device-keys.md`](../03-crypto/identity-and-device-keys.md). |
| Local database | `rusqlite` | 0.31.0 | [local-database.md](../04-storage/local-database.md) | `bundled-sqlcipher` feature — statically links SQLCipher v4 (encrypted SQLite); no system SQLite dependency. |
| CBOR serialization | `ciborium` | 0.2.2 | [wire-protocol.md](../05-sync/wire-protocol.md), [data-encryption-format.md](../03-crypto/data-encryption-format.md) | Canonical op / envelope encoding. |
| HTTP + WebSocket server | `axum` | 0.7.9 | [ADR-0005](../11-adr/0005-sync-transport.md) | Relay REST + `/sync` WS; `sunrise-server`. |
| Async runtime | `tokio` | 1.52.3 | — (runtime substrate) | Multi-thread runtime for server + sync driver. |
| WebSocket client transport | `tokio-tungstenite` | 0.24.0 | [ADR-0005](../11-adr/0005-sync-transport.md) | Sync transport; `rustls-tls-webpki-roots`. |
| TLS | `rustls` | 0.23.40 | [ADR-0005](../11-adr/0005-sync-transport.md) | `ring` backend, no OpenSSL. |
| Frame compression | `zstd` | 0.13.3 | [wire-protocol.md](../05-sync/wire-protocol.md) | Wire-frame payload compression. |
| Server config parsing | `toml` | 1.1.4 | [self-hosting.md](../06-server/self-hosting.md) | `sunrise-server` only, `default-features = false` + `parse`/`serde`. Added **zero** crates to the lock: it was already resolved as a `uniffi_macros` dependency of `sunrise-core-bindings`. See Reconciliations §g. |
| Structured logging | `tracing` | 0.1.44 | [ADR-0010](../11-adr/0010-logging-strategy.md) (amended), [logging.md](../10-cross-cutting/logging.md) | The logging API for the whole workspace. Was already in the lock transitively via `tower-http`/`axum`; now a direct dependency of `sunrise-log`, `-server`, `-storage`, `-core`, `-cli`. (`-tui` was listed here until [ADR-0019](../11-adr/0019-swiftui-macos-client.md) deleted that crate.) |
| Log subscriber | `tracing-subscriber` | 0.3.23 | [ADR-0010](../11-adr/0010-logging-strategy.md) (amended) | `default-features = false` + `std`/`fmt`/`env-filter`/`json`/`registry`. `ansi` deliberately off — no colour codes in NDJSON, and it drops `nu-ansi-term`. Adds `sharded-slab`, `thread_local`, `matchers`, `tracing-serde` to the lock, all MIT/Apache-2.0. |
| Log redaction | `sunrise-log` (workspace) | — | [ADR-0010](../11-adr/0010-logging-strategy.md) (amended), [logging.md](../10-cross-cutting/logging.md) §6 | Not a logger. `Plain<T>` (no `Display`/`Serialize`/`Value`), the `RedactionLayer` field-name veto, the `ev` catalogue check, and subscriber assembly. |
| Property-based testing | `proptest` | 1.11.0 | [testing.md](../10-cross-cutting/testing.md) | Convergence / redaction / round-trip proptests. |
| Foreign bindings (Swift, later Kotlin) | `uniffi` | 0.32.0 | [ADR-0019](../11-adr/0019-swiftui-macos-client.md), [ADR-0026](../11-adr/0026-msrv-bump.md) | `sunrise-core-bindings` only, `default-features = false` + `tokio`. 0.32's only default feature is `cargo-metadata`, which serves the UDL/build-script path; these bindings are generated in `--library` mode from the compiled dylib, so it is dead weight here and turning it off keeps `cargo_metadata` + `cargo-platform` out of the seam crate. The generator lives in `tools/uniffi-bindgen`, **outside** the workspace, with its own lockfile — for **feature unification**, not MSRV: as a member it would ask `uniffi` for `cli`, and resolver 2 would unify that onto the seam crate's build, adding 20 third-party packages (`askama`, `goblin`, `uniffi_bindgen`, `uniffi_udl`, …) to every workspace build. It formerly also pinned `cargo-platform` to 0.3.2 against the 1.88 toolchain; ADR-0026 lifted that and the pin is gone. `mise run apple-xcframework` builds it `--locked`. |
| Snapshot testing | `insta` | *declared 1.40; not in lock* | [testing.md](../10-cross-cutting/testing.md) | Declared `1.40` with the `yaml` feature. It resolved to `1.48.0` while it had a consumer; with that consumer gone it is unconsumed again and **no longer appears in `Cargo.lock`**, like `hpke`. Its only consumer was `sunrise-tui`'s golden-frame render snapshots, deleted with the TUI ([ADR-0019](../11-adr/0019-swiftui-macos-client.md)); the entry stays because snapshot testing is still the right tool for the next renderer that needs it. See Reconciliations §e. |
| Benchmarking | `criterion` | 0.5.1 | [testing.md](../10-cross-cutting/testing.md) | `sunrise-bench` only. `default-features = false` + `cargo_bench_support`; drives the submit / query_today@10k / fts@10k / ws-handshake benches that feed `bench/baseline.json`. |
| Deterministic seeded RNG | `rand_chacha` | 0.3.1 | [testing.md](../10-cross-cutting/testing.md) | ChaCha20 CSPRNG seeded for reproducibility. Direct dependency of `sunrise-crypto`, `sunrise-onboarding`, `sunrise-crypto-test-vectors`, `sunrise-e2e` (seeded chaos transport), and `sunrise-bench` (fixture generation). |
| Datetime | `jiff` | 0.2.32 | [ADR-0011](../11-adr/0011-datetime-jiff.md) | Sole datetime library **in Sunrise's own code**. `jiff::Timestamp` for absolute instants; civil/`Zoned` types available for wall-clock and tz-aware semantics. The chrono→jiff migration landed and the unused `time` dependency was removed. `chrono` 0.4.45 is back in `Cargo.lock` — transitively, via `oauth2`; see that row and Reconciliations §f. Nothing in the workspace calls it. See Reconciliations §d. |
| OIDC client (PKCE, code + refresh grants) | `oauth2` | 5.0.0 | [auth.md](../06-server/auth.md), issue #7 | `sunrise-auth` only, `default-features = false`. Disabling defaults is load-bearing: the defaults pull `reqwest`, a second HTTP stack beside the `hyper` + `hyper-rustls` one `sunrise-server` already uses for JWKS. With them off, `oauth2` speaks `http::Request`/`Response` and the client is supplied — which is also what makes the whole flow testable against an in-memory issuer. **Costs `chrono` and its timezone-detection stack (10 crates) transitively**, used only by `oauth2`'s RFC 7662 introspection module, which Sunrise never calls; accepted rather than hand-writing PKCE and refresh. MIT OR Apache-2.0; clean under `cargo deny check`. |
| URL parsing (redirect + query handling) | `url` | 2.5.8 | — (supporting `oauth2`) | Already present transitively; made direct in `sunrise-auth` so the loopback redirect's query parsing does not depend on a transitive. |
| `http` request/response types | `http` | 1.4.0 | — (supporting `oauth2`/`hyper`) | Already present via `hyper`; made direct in `sunrise-auth` because it is the currency `oauth2` hands the HTTP client. |
| JWT / JWKS verification | `jsonwebtoken` | 9.3.1 | [auth.md](../06-server/auth.md) | `sunrise-server` only, `default-features = false` (`use_pem` off). Pinned to 9.x deliberately: it links `ring`, which `rustls` already pulls in, so the JWT/JWKS surface costs one crate. 10.x/11.x replaced `ring` with either `aws-lc-rs` (C toolchain, cmake + bindgen) or `rust_crypto`, which drags in `rsa` 0.9 and the unpatched RUSTSEC-2023-0071 Marvin advisory — that alone would red `cargo deny check`. Dropping `use_pem` sheds `pem` + `simple_asn1`; JWKS keys arrive as base64url `n`/`e`, never PEM. |
| HTTPS for issuer fetches | `hyper-rustls` | 0.27.9 | [auth.md](../06-server/auth.md) | `sunrise-server` (JWKS + discovery) and `sunrise-auth` (discovery + token endpoint). `ring` + `webpki-tokio` reuse the rustls stack and Mozilla root bundle already in the tree; the default `aws-lc-rs` would add a C toolchain for no gain. |
| RRULE parsing | *none (hand-written)* | — | [recurrence-engine.md](../08-features/recurrence-engine.md), [routines-and-recurrence.md](../02-domain/routines-and-recurrence.md) | Hand-written parser at `crates/sunrise-domain/src/rrule.rs`. See Reconciliations §c. |

Supporting utility crates (`serde`, `thiserror`, `anyhow`, `hyper`, `tower`,
`tower-http`, `subtle`, `zeroize`, `rand`, `parking_lot`, etc.) follow their
declared caret ranges in `Cargo.toml` and are not individually pinned here;
they carry no independent design decision. The auth crates above are the
exception in the other direction: `jsonwebtoken` and `hyper-rustls` are
utility-shaped but their *feature* choices are security decisions, so they are
listed.

## Bun workspace pins

Majors only; exact ranges live in the per-package `package.json` files.

| Purpose | Package | Major | Notes |
|---|---|---|---|
| UI framework | `react` / `react-dom` | 18 | `apps/web` only. `apps/desktop` was listed here until the Tauri shell was cut; the directory no longer exists. |
| Web bundler / dev server | `vite` | 5 | `apps/web`. |
| Language | `typescript` | 5.8 | Workspace-wide (`~5.8.3`). |
| Lint / format | `@biomejs/biome` | 2 | Root dev dependency. |
| Unit testing | `vitest` | 4 | Root + coverage-v8. |

## Reconciliations

Seven places where doc prose had drifted from the real manifest / lockfile.
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
  `serde`, and **`tzdb-bundle-always`** — not `tzdb-bundle-platform`, which an
  earlier revision of this section named. The distinction is the point of the
  choice: `-always` embeds a fixed tzdb in the binary so every device resolves
  IANA zones identically, which is what makes routine expansion deterministic
  across platforms. `default-features = false` also disables `tzdb-zoneinfo`,
  so without an explicit bundle no named zone would resolve at all.
- `chrono` was **removed** — no `chrono::` usage remains anywhere in the
  workspace. It reappeared in `Cargo.lock` later as a transitive dependency of
  `oauth2`; that is a lock-file fact, not a datetime decision, and §f explains
  it.
- `time` (declared `0.3`, never consumed) was **removed** with it.

Storage stays epoch-ms integers and op CBOR stays RFC 3339 strings, so neither
the on-disk nor the wire format changed. A permanent wire-compat test
(`crates/sunrise-domain/tests/serde_compat.rs`) decodes the pre-migration
chrono-era CBOR fixtures under the jiff types; `jiff::Timestamp` and
`chrono::DateTime<Utc>` serialize a UTC instant to the identical RFC 3339
string, so the canonical bytes are byte-identical.

### e. `insta` — consumed, then unconsumed again

`insta` was *declared but unconsumed*, then became a dev-dependency of
`sunrise-tui` for golden-frame render snapshots, and is now unconsumed again:
[ADR-0019](../11-adr/0019-swiftui-macos-client.md) deleted that crate. Declared
as `1.40` in `[workspace.dependencies]`, it resolved to **`1.48.0`** while it
had a consumer. The declaration stays — snapshot testing is the right tool for
whatever renders next, and re-adding it later would be a decision to re-argue
for no reason.

### f. `chrono` is back in the lock file, and nothing calls it

[ADR-0011](../11-adr/0011-datetime-jiff.md) chose `jiff` and the migration
removed `chrono` from `Cargo.lock` entirely. Adding `oauth2` for the OIDC login
flow (issue #7) puts it back — `oauth2` depends on `chrono` unconditionally,
with the `clock` feature, which also drags in `iana-time-zone`,
`android_system_properties`, `core-foundation-sys`, and four `windows-*`
crates. Eleven new entries in the lock file, ten of them serving `chrono`.

Two things are worth writing down rather than discovering later:

1. **`oauth2` uses `chrono` in exactly one module**: `introspection.rs`, the
   RFC 7662 token-introspection endpoint, which Sunrise does not call. The cost
   is real and the benefit is zero — but the dependency is not optional
   upstream, so it cannot be feature-gated away.
2. **ADR-0011 is not weakened.** It governs the code in this repository, and no
   Sunrise crate gains a `chrono` import; `jiff` remains the sole datetime
   library anything here calls. What changed is the transitive closure, not the
   decision.

It was accepted rather than hand-writing PKCE and the token grants. Those are
the two places OAuth implementations reliably go wrong, and a vetted,
widely-used crate getting them right is worth more than ten unused lock
entries. `cargo deny check` passes with the advisory ignore list still empty.

### g. `toml` — a new dependency that costs nothing

`sunrise-server` reads `sunrise.toml`. The workspace has a standing preference
for hand-rolling small grammars — `sunrise-client-core::config::parse_pairs`
exists precisely because pulling a TOML crate in "to read a flat list of string
pairs would be the largest dependency in the tree for the smallest grammar in
it", and that reasoning was right for that case.

It does not apply here, for two reasons.

1. **The grammar is not small.** The server config is nested tables carrying
   strings, booleans, integers, and string arrays. `parse_pairs` handles quoted
   string values in a flat namespace and cannot express `allow_signup = false`
   or `allowed_origins = [...]` without coercion. Extending it would mean
   hand-writing a TOML subset parser for the file that decides whether the
   server authenticates anyone — the wrong place to save a dependency.
2. **It adds nothing to the tree.** `toml` 1.1.4 was already in `Cargo.lock`,
   pulled by `uniffi_macros` for `sunrise-core-bindings`, along with
   `toml_datetime`, `toml_parser`, and `toml_writer`. Naming it in
   `sunrise-server` added exactly one line to the lock file: an edge, not a
   crate. Nothing new is compiled and nothing new is audited.

Pinned `default-features = false` with `parse` + `serde`, which excludes the
serializer — the server reads config and never writes it.
