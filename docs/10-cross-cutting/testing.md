---
status: accepted
---

# Testing Strategy

Test pyramid plus a few specialized layers for what makes Sunrise distinctive.

## Layers

### 1. Unit (broad, fast)

- Pure-function tests in `sunrise-core` for parsers, RRULE expansion, LWW merge, crypto envelopes.
- Run on `cargo test`; <30 seconds in CI.
- Coverage target: 80% lines on the core; 90% on crypto and on the merge path (`sunrise-core::engine::lww_wins` and the materialization it guards). The 90% figure previously named "CRDT modules", which do not exist — `crates/sunrise-crdt` was deleted by [ADR-0014](../11-adr/0014-entity-level-lww-merge.md), so that half of the gate applied to nothing. No Rust coverage tool is wired (CI runs `bun run test:coverage` for the TS side only), so neither target is measured or enforced today.

### 2. Property tests (thinner, deep)

- Merge convergence: random op streams across simulated devices.
- Capture parser fuzz.
- RRULE expansion across DST boundaries.
- Op envelope round-trip (encode/decode/encrypt/decrypt/sign/verify).

#### Convergence property-test determinism

- **Library**: `proptest` (Rust). Fuzz targets that need wire-bytes coverage are `cargo-fuzz` binaries in `fuzz/`: `op_envelope`, `wire_frame`, `rrule`, `ical`, `oauth_state`, `recovery_blob`.
- **Seed**: read from `SUNRISE_FUZZ_SEED` (hex) when set; otherwise default to the first 8 bytes of the workspace `HEAD` commit hash. Every CI run logs the resolved seed in the suite header so a failing run is reproducible by re-export.
- **Volume**: 1 000 random op sequences per CI run; release branches run 100 000 nightly.
- **Assertion**: for every permutation of the same op set across N simulated devices, the final state is byte-identical (canonical CBOR comparison).

### 3. Integration (medium)

- End-to-end inside one process: spin up an in-memory server + N simulated clients; orchestrate scenarios:
  - Multi-device offline / online dance.
  - Sharing accept / edit / revoke.
  - Migration from old schema fixture.
  - Recovery code restore.

### 4. Per-platform UI tests (narrow)

- Critical-path smoke tests on each platform:
  - Cold launch → Today renders.
  - Capture → save → see in Today.
  - Mark done → state persists across relaunch.
- Platform-specific tooling: XCTest on macOS; Espresso and Playwright when Android and Web are scheduled. The CLI needs none — `crates/sunrise-cli/tests/cli.rs` runs the real binary against a real vault, which covers the same three critical paths without a UI harness at all.

### 5. Network / chaos tests

- Toxic-proxy between client and server: drop packets, corrupt bytes, delay, partition.
- Verify clients converge once the partition heals.
- Verify integrity warnings fire on tampered envelopes.

### 6. Performance tests

- Per-platform benchmark suite (op apply rate, capture latency, search latency).
- Run on representative hardware in CI (a Mac mini, an Android reference device, a Pixel emulator, a Linux runner).
- Regression alerts.

#### Perf-bench CI integration

- Per-platform baselines live in `bench/baseline.json`, committed to the repo.
- Every PR runs the benchmark suite and compares against the baseline.
- Any benchmark regressing >5% blocks merge via the required check `bench / regression`; the CI job comments the offending benchmarks on the PR.
- Nightly runs on `main` open an auto-bot PR that updates `bench/baseline.json` to the new measurements, so the baseline tracks intentional drift without humans curating it.

### 7. Privacy / safety tests

- Static checks: any code path under `telemetry/` or `logging/` that calls `.expose()` on a `Plain<T>` fails the build.
- Server-side: no log statement contains task IDs from a fixture vault (regex check on test logs).

## Test data

- Synthetic vault generator that produces known-shape data (N tasks, N streams, recurring routines).
- Fixture vaults for migration tests.

## CI matrix

- Per PR: Rust core unit + property + integration; web + desktop UI smoke.
- Nightly: full mobile UI on real-device simulators; chaos suite; performance benchmarks.
- Per release: manual a11y; manual cross-platform pairing flow.

## Coverage and what we don't measure

- We don't measure UI coverage by lines (false-precision).
- We measure UI by:
  - Critical-path scenarios passing.
  - Performance budgets met.
  - A11y audit clean.

## Mutation testing

- Run `cargo-mutants` on release-candidate branches only (the suite is too expensive for per-PR runs).
- Targets: `sunrise-core`, `sunrise-crypto`, `sunrise-sync`, `sunrise-domain`.
- **≥ 90 % caught mutations** is required for release sign-off; below this threshold blocks the release.

## Release gates

- All CI green.
- No P1 a11y regression.
- No new unfamiliar crash (per crash reporting baseline).
- Performance regression ≤5% on every benchmark.
- Sync convergence test: green across the version pair (current vs N-1).

## Security testing

Security testing is a first-class layer alongside unit and property tests. It has three pillars:

### Continuous fuzz targets

`cargo-fuzz` binaries live in `fuzz/` and run on every CI build (short budget) and nightly (long budget). The v1 target set:

| Target | Scope |
|---|---|
| `op_envelope` | `sunrise-crypto` envelope decode + signature verify path. |
| `wire_frame` | `sunrise-sync` framing + magic-prefix parser. |
| `rrule` | RRULE parser and DST-aware expansion. |
| `ical` | inbound iCalendar feed parser (`sunrise-integrations`). |
| `oauth_state` | OAuth/PKCE state-machine transitions in `sunrise-server::auth`. |
| `recovery_blob` | recovery-blob decode + KDF input validation. |

Any new crash discovered by a fuzz target opens a P1 bug and the corresponding minimized input is added to the seed corpus.

### Quarterly external pen test

A scoped external penetration test runs once per quarter. The standing scope covers: pairing/onboarding (Noise-XX), sync wire protocol (auth, replay, downgrade), crypto suite (envelope tampering, recovery-blob misuse), server auth surface (`sunrise-server::auth`), and integration OAuth flows. Findings are tracked in the same issue tracker as internal bugs; high/critical findings block the next minor release.

### Security-review gate

Changes to any of these modules require a security-focused review (a reviewer from the security-reviewers group) in addition to the normal code review:

- `sunrise-crypto`
- `sunrise-sync`
- `sunrise-server::auth`
- `sunrise-storage::migrations`
- pairing/onboarding code paths in `sunrise-core`

CI enforces the gate via a `CODEOWNERS` rule on these directories.
