# Chaos test harness

The chaos suite runs an in-process toxic proxy between two `sunrise-tui`
clients and a `sunrise-server` to exercise the system under network
adversity:

- packet drop
- byte corruption
- arbitrary delay
- full partition

Per `spec/10-cross-cutting/testing.md` §release-gates, the suite is
expected to converge to byte-identical state on both clients within
the bounded retry / backoff budget for every scenario.

v1 ships the harness layout; the actual scenarios are populated as the
sync layer sees real traffic. The harness skeleton lives at
[`harness.rs`](./harness.rs).

To run:

```bash
cargo test --test chaos -- --nocapture
```

Add new scenarios by extending `scenarios::all()`.
