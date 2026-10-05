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

## Sync propagation (commit on one device → applied on another)

The end-to-end number, and the one the user feels. **p99 < 500 ms** for a peer that is online and
subscribed, over a relay with ≤ 50 ms RTT to each device. That is the typical case: the same
city, or the same network. The clock starts when the authoring device's local commit returns. It
stops when the receiving device's engine has applied the op and published it on its change feed,
because the UI repaints from that feed.

| Leg | Budget (p99) | Measured by |
|---|---|---|
| Author: commit → batch on the wire | 50 ms | not measured on its own; inside the harness total |
| Relay: batch accepted → handed to the last subscriber's stream | 100 ms | `sunrise_sync_fanout_latency_seconds` ([`metrics.md`](../06-server/metrics.md)) |
| Network, both hops | 2 × RTT (≤ 100 ms at the stated RTT) | harness-injected as one RTT on the op's path (half up, half down), held on the author's send; the second RTT is budgeted, not injected |
| Receiver: frame read → applied and published | 50 ms | not measured on its own; inside the harness total |
| Headroom | 200 ms | — |

**How it is measured.** `crates/sunrise-e2e/src/latency.rs` boots the real relay and two paired
cores on loopback. Each device reaches the relay through a fault-injecting link that holds every
send for the stated RTT, ±20 % from a seeded RNG: on the SSE transport a send is a `POST` whose reply
is its ack, and an op's trip up to the relay and down to the peer adds to one round trip. Device A
commits 10 000 tasks, ten a second. Each sample runs from A's `submit` returning to B publishing that
task's `Created`, both timestamps read from one monotonic clock, and the p99 is nearest-rank over
every op. An op that never arrives is reported, not dropped from the sample.
`crates/sunrise-e2e/tests/sync_latency.rs` runs that at 0, 20 and 80 ms RTT in the nightly
`Sync latency` CI job, and fails if, at 0 or 20 ms, any op is missing or the p99 reaches 500 ms. The
80 ms leg falls outside the budget's conditions and is reported only.

Measured on an Apple M3 Pro, release build, at this revision:

| RTT | Ops arrived | p50 | p95 | p99 | max |
|---|---|---|---|---|---|
| 0 ms | 10 000 of 10 000 | 12.3 ms | 23.9 ms | 31.0 ms | 60.2 ms |
| 20 ms | 10 000 of 10 000 | 36.3 ms | 62.0 ms | 74.3 ms | 111.3 ms |
| 80 ms | 1 910 of 10 000 | — | — | — | — |

At 80 ms the author's session collapsed partway through ([#475](https://github.com/justin13888/Sunrise/issues/475)),
so that row has no distribution to report.

Ten commits a second is not an arbitrary ceiling. At 40 a second and 20 ms, or 10 a second and
80 ms, the author's sync driver stops reading its acks and tears its session down
([#475](https://github.com/justin13888/Sunrise/issues/475)). Until that is fixed, a faster harness
would measure that collapse and not propagation.

The wider-network rows in [`../05-sync/overview.md`](../05-sync/overview.md) §Latency targets (LTE, a
phone woken by push) are budgets for those conditions, not relaxations of this one.

## Planner preview

`plan_preview` ([`../08-features/planner.md`](../08-features/planner.md)) runs whenever a drag
crosses a snap boundary, at most once per frame, so it has a frame budget: **≤ 16 ms p95, ≤ 33 ms p99** on Baseline A for a typical week
(200 open tasks, 60 blocks, 40 external events, 20 dependencies). On Baseline B the ceiling is
2 ×. A preview over budget is a bug in the solver's incrementality, not a reason to throttle the drag.

## Search

- p95 ≤100 ms over 10k tasks on all platforms, and ≤ 250 ms over 100k tasks on Baseline A and B,
  across every indexed entity kind ([`../08-features/search.md`](../08-features/search.md)).
- The index's memory, resident while searching, counts against the ceilings below. A 100k-task
  vault MUST NOT push iOS past its 200 MB ceiling.

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
- **As built, the Criterion suite:** the `bench-regression` job in `ci.yml`
  runs on `schedule` and `workflow_dispatch` only — never on a pull request —
  on `ubuntu-latest` and on the pinned `macos-15` image, one leg per platform
  key in `bench/baseline.json`. It carries `continue-on-error: true` and
  compares with a **60%** tolerance rather than 5%. It reports; it blocks
  nothing, and no automated check enforces the hard caps in the table above.
- The job's own comment records why: measured on the shared runner, the same
  binary against its own recorded baseline swings +270% (`ws_handshake`, since
  renamed `sync_session`) and −39% (`submit_create_task`) from scheduling noise
  and a short measurement window alone. A gate that red-lights on noise is
  ignored within a week, and then it protects nothing. The 5% figure assumes
  the dedicated hardware named under "Calibration cadence" below, which the
  project does not have yet.
- **What the comparison reads.** `crates/sunrise-bench/src/bin/baseline.rs`
  keeps one row per directory Criterion writes, and a unit test holds that
  table to every `bench_function` and `benchmark_group` name in
  `crates/sunrise-bench/benches/*.rs`, in both directions. A row whose bench did
  not run, or a directory no row names, is printed and fails `--check`; it is
  never skipped in silence, which is how `sync_session` went uncompared after
  it replaced `ws_handshake`.
- **As built, the sync propagation budget:** the `Sync latency` job runs the
  harness under [Sync propagation](#sync-propagation-commit-on-one-device--applied-on-another)
  nightly and on demand, and it does gate: it fails when the p99 at any RTT the
  budget covers reaches 500 ms. An absolute budget with the measured tail far
  below it is not at the mercy of runner noise the way a 5% relative gate is.
  It is still not a pull-request check — it takes minutes — so a regression
  there turns the nightly run red rather than stopping a merge.
- Otherwise a budget regression is something a human notices in the nightly
  report, not something that stops a merge or a release. Tracked in
  [#33](https://github.com/justin13888/Sunrise/issues/33).

## Calibration cadence

- Benchmarks are calibrated on bare-metal CI runners owned by the project (rented dedicated hosts, not shared cloud VMs). Shared-VM runners introduce too much variance to defend a 5 % gate.
- Recalibration runs **monthly**, and additionally whenever the CI runner image changes.
- If a calibration result diverges by more than 5 % from the previous calibration, page the on-call and hold baseline updates until the divergence is investigated.
