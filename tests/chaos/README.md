# Chaos test harness

The chaos suite exercises sync convergence under network adversity by running
an in-process **toxic** transport between two `sunrise-core` replicas and a
`sunrise-server` relay, injecting:

- packet drop
- byte corruption
- arbitrary delay
- full partition

Per `docs/10-cross-cutting/testing.md` §network/chaos, the suite is expected to
converge to byte-identical state on both replicas once a partition heals, within
the bounded retry / backoff budget for every scenario. Tampered frames must
surface integrity failures downstream (a corrupted header fails framing; a
corrupted payload fails AEAD verification).

## Where the harness lives

The reusable harness lives in the `sunrise-e2e` crate, not in this directory:

- `crates/sunrise-e2e/src/chaos/toxic.rs` — `Toxic<T: Transport>`, the
  fault-injecting wrapper, plus `ToxicConfig` and the shared `FaultHandle`
  runtime switches (`partition`, drop/corrupt probability setters).
- `crates/sunrise-e2e/src/chaos/loopback.rs` — `loopback_pair()`, a connected
  pair of async, FIFO in-process transports the wrapper sits on top of.

## Reproducibility

All faults are drawn from a seeded `rand_chacha` ChaCha20 RNG. The seed is read
from `SUNRISE_FUZZ_SEED` (hex, or plain decimal; unset falls back to a fixed
default), or passed explicitly via `Toxic::with_seed`. A failing run is
reproducible by re-exporting its seed.

## Running

Transport-level unit tests today:

```bash
cargo test -p sunrise-e2e --test chaos_transport
```

The convergence scenarios that drive real cores across a toxic link land in a
later slice as:

```bash
cargo test -p sunrise-e2e --test chaos
```
