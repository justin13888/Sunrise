---
status: draft
---

# Testing Strategy

Test pyramid plus a few specialized layers for what makes Sunrise distinctive.

## Layers

### 1. Unit (broad, fast)

- Pure-function tests in `sunrise-core` for parsers, RRULE expansion, CRDT mutations, crypto envelopes.
- Run on `cargo test`; <30 seconds in CI.
- Coverage target: 80% lines on the core; 90% on crypto and CRDT modules.

### 2. Property tests (thinner, deep)

- CRDT convergence: random op streams across simulated devices.
- Capture parser fuzz.
- RRULE expansion across DST boundaries.
- Op envelope round-trip (encode/decode/encrypt/decrypt/sign/verify).

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
- Platform-specific tooling: XCTest, Espresso, Playwright (web), Tauri's WebDriver, asserting on Ratatui's snapshot for TUI.

### 5. Network / chaos tests

- Toxic-proxy between client and server: drop packets, corrupt bytes, delay, partition.
- Verify clients converge once the partition heals.
- Verify integrity warnings fire on tampered envelopes.

### 6. Performance tests

- Per-platform benchmark suite (op apply rate, capture latency, search latency).
- Run on representative hardware in CI (a Mac mini, an Android reference device, a Pixel emulator, a Linux runner).
- Regression alerts.

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

- For critical-path code (CRDT merge, crypto), run mutation tests (`cargo-mutants`) on a quarterly cadence; aim for >90% caught mutations.

## Release gates

- All CI green.
- No P1 a11y regression.
- No new unfamiliar crash (per crash reporting baseline).
- Performance regression ≤5% on every benchmark.
- Sync convergence test: green across the version pair (current vs N-1).
