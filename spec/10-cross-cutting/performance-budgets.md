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
| TUI (baseline A) | 100 ms | 250 ms | 500 ms |

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
- TUI (Baseline A): ≥40k ops/sec.

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
- TUI: ≤80 MB.

## Background impact

- iOS: average background CPU per day ≤30s.
- Android: average background CPU per day ≤40s; respects Doze.

## Network

- Idle steady-state: ≤2 KB/min keepalive.
- Per-op delivery: envelope size + framing; typical 200 B – 1.5 KB.

## Battery

- A device that opens Sunrise 10 times/day and receives 100 ops/day should consume ≤1% of battery on a modern phone.

## Tooling

- `cargo bench` for core operations (Criterion).
- Lighthouse for web cold-start budget.
- Custom harnesses for app-launch numbers (perfetto on Android, MetricKit on iOS, hyperfine for TUI).

## Regression policy

- Any benchmark exceeding budget by >5% is a P1; merge gate.
- "Hard cap" violations on the user's path block release.
