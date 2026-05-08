---
status: draft
---

# Performance Budgets

Numbers we promise. CI fails when a regression breaks them.

## Cold start (app launch → first render of Today)

| Platform | p50 | p95 | Hard cap |
|---|---|---|---|
| Desktop (Apple Silicon, mid-tier 2022 Win/Linux) | 200 ms | 500 ms | 1 s |
| iOS (iPhone 14+) | 300 ms | 700 ms | 1.5 s |
| Android (Pixel 7) | 400 ms | 900 ms | 2 s |
| Web (4G, cold cache) | 1.5 s | 3 s | 5 s |
| Web (warm cache) | 250 ms | 500 ms | 1 s |
| TUI | 100 ms | 250 ms | 500 ms |

## Quick capture (trigger → input ready)

All platforms: ≤100ms p95.

## Capture commit (Enter pressed → confirmation visible)

≤50ms p95 on every platform.

## Op apply rate

- Desktop: ≥50k ops/sec.
- iOS (iPhone 14+): ≥10k ops/sec.
- Android (Pixel 7): ≥8k ops/sec.
- Web (V8 + WASM): ≥5k ops/sec.
- TUI: ≥40k ops/sec.

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
