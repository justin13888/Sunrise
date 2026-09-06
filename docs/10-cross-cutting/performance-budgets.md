---
status: accepted
---

# Performance Budgets

Numbers we promise. CI fails when a regression breaks them.

## Cold start (app launch → first render of Today)

| Platform | p50 | p95 | Hard cap |
|---|---|---|---|
| Desktop (baseline A) | 200 ms | 500 ms | 1 s |
| iOS (baseline B) | 300 ms | 700 ms | 1.5 s |
| Android (baseline C) | 400 ms | 900 ms | 2 s |
| Web (4G, cold cache, baseline A) | 1.5 s | 3 s | 5 s |
| Web (warm cache, baseline A) | 250 ms | 500 ms | 1 s |
| CLI (baseline A) | 100 ms | 250 ms | 500 ms |

### Hardware baselines (CI-pinned)

| Code | Spec | Used in CI as |
|---|---|---|
| **Baseline A** (desktop) | Apple M1 Pro, 16 GB RAM, NVMe SSD | macOS GitHub Actions runner; `bench-desktop-a` |
| **Baseline B** (iOS) | iPhone 13 (A15, 4 GB RAM) | physical device farm; `bench-ios-b` |
| **Baseline C** (Android) | Pixel 7 (Tensor G2, 8 GB RAM, UFS 3.1) | physical device farm; `bench-android-c` |

Linux/Windows desktop builds run a calibrated comparison test on a Ryzen 5 5600 (16 GB DDR4-3200, NVMe SSD) and a Dell XPS 13 9310 (i7-1185G7, 16 GB, NVMe SSD); benches must hit Baseline A within ±15 % on both. Targets above are normative on the named baseline; comparable consumer hardware is expected to track within the same envelope.

## Quick capture (trigger → input ready)

All platforms: ≤100ms p95.

## Capture commit (Enter pressed → confirmation visible)

≤50ms p95 on every platform.

## Op apply rate

- Desktop (Baseline A): ≥50k ops/sec.
- iOS (Baseline B): ≥10k ops/sec.
- Android (Baseline C): ≥8k ops/sec.
- Web (V8 + WASM, Baseline A): ≥5k ops/sec.
- CLI (Baseline A): ≥40k ops/sec.

## Initial sync (10k tasks)

- Desktop: ≤5 s.
- Mobile: ≤30 s.
- Web (cold OPFS): ≤60 s.

## Search

- p95 ≤100 ms over 10k tasks on all platforms.

## Memory ceiling (steady-state)

- Desktop: ≤300 MB.
- iOS: ≤200 MB.
- Android: ≤200 MB.
- Web tab: ≤150 MB.
- CLI: ≤80 MB (a one-shot process; the number is a ceiling on the core, not on a long-running UI).

## Background impact

- iOS: average background CPU per day ≤30s.
- Android: average background CPU per day ≤40s; respects Doze.

### iOS background-CPU measurement

- **Tool**: MetricKit's `MXCPUMetric.cumulativeCPUTime`, aggregated over the `MXMetricPayload.timeStampEnd − timeStampBegin` window.
- **Window**: backgrounded period only (foreground time excluded).
- **Inclusions**: Sunrise's own `BGAppRefreshTask` CPU time.
- **Exclusions**: silent-push CPU is system-attributed and not counted.
- **Test fleet**: 10 devices enrolled in the TestFlight beta with MetricKit reporting enabled. The fleet's aggregate **p95 must be ≤ 30 s/day**; a release that exceeds this for two consecutive weekly windows blocks promotion to General Availability.

## Network

- Idle steady-state: ≤2 KB/min keepalive.
- Per-op delivery: envelope size + framing; typical 200 B – 1.5 KB.

## Battery

- A device that opens Sunrise 10 times/day and receives 100 ops/day should consume ≤1% of battery on a modern phone.

## Tooling

- `cargo bench` for core operations (Criterion).
- Lighthouse for web cold-start budget.
- Custom harnesses for app-launch numbers (Instruments on macOS, hyperfine for the CLI).

## Regression policy

- **As specified:** any benchmark exceeding budget by >5% is a P1 and blocks
  merge via a required check, and a "Hard cap" violation on the user's path
  blocks release.
- **As built:** the `bench-regression` job in `ci.yml` runs on `schedule` and
  `workflow_dispatch` only — never on a pull request — carries
  `continue-on-error: true`, and compares against `bench/baseline.json` with a
  **60%** tolerance rather than 5%. It reports; it blocks nothing, and no
  automated check enforces the hard caps in the table above.
- The job's own comment records why: measured on the shared runner, the same
  binary against its own recorded baseline swings +270% (`ws_handshake`) and
  −39% (`submit_create_task`) from scheduling noise and a short measurement
  window alone. A gate that red-lights on noise is ignored within a week, and
  then it protects nothing. The 5% figure assumes the dedicated hardware named
  under "Calibration cadence" below, which the project does not have yet.
- Until it does, a budget regression is something a human notices in the
  nightly report, not something that stops a merge or a release. Tracked in
  [#33](https://github.com/justin13888/Sunrise/issues/33).

## Calibration cadence

- Benchmarks are calibrated on bare-metal CI runners owned by the project (rented dedicated hosts, not shared cloud VMs). Shared-VM runners introduce too much variance to defend a 5 % gate.
- Recalibration runs **monthly**, and additionally whenever the CI runner image changes.
- If a calibration result diverges by more than 5 % from the previous calibration, page the on-call and hold baseline updates until the divergence is investigated.
