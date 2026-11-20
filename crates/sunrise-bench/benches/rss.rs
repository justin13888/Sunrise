//! `rss` bench: the process's resident memory with a 10k-task vault open and
//! answering reads (`docs/10-cross-cutting/performance-budgets.md` §Memory
//! ceiling).
//!
//! The vault is seeded with 10k tasks through the command path, closed, and
//! opened as an app opens it, through `Core::open`. Each iteration then runs
//! one `Query::Today`, so the reading is taken with the vault in use rather
//! than idle, and a sample's value is the process's resident set after its
//! iterations. That is the whole bench process, the async runtime and
//! Criterion's own bookkeeping included, so it overstates the core slightly
//! and never understates it.
//!
//! **The value is bytes, not time.** Criterion records whatever its
//! [`Measurement`] returns, so this bench measures with [`ResidentBytes`] and
//! `baseline.rs` reads the `sample.json` it writes like any other, converting
//! to MiB. Criterion divides a sample's value by its iteration count to get a
//! per-iteration figure, which is right for a duration and wrong for a level,
//! so each sample reports the resident set times its iteration count, and the
//! division gives back the resident set.
//!
//! The resident set is read from `/proc/self/status` (`VmRSS`) on Linux and
//! from `ps -o rss=` elsewhere; both report KiB. A platform where neither
//! answers fails the bench rather than recording a zero.

#![allow(clippy::doc_markdown)]

use std::time::Duration;

use criterion::measurement::{Measurement, ValueFormatter};
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use sunrise_bench::{bench_unlock, core_config, seeded_vault_dir, FIXED_NOW_MS};
use sunrise_core::{Core, Query};

/// The process's resident set, in bytes.
///
/// # Panics
/// Panics where the platform reports none: a zero would read as a perfect
/// result, not as a missing one.
fn resident_bytes() -> u64 {
    resident_kib().expect("read this process's resident set") * 1024
}

#[cfg(target_os = "linux")]
fn resident_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
}

#[cfg(not(target_os = "linux"))]
fn resident_kib() -> Option<u64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()?.trim().parse().ok()
}

/// A Criterion measurement whose value is a resident-set reading in bytes,
/// taken through `iter_custom`. `start` and `end` exist for the trait; a
/// bench that times a routine with `iter` under this measurement would record
/// a reading per batch, not per iteration, so this one never does.
struct ResidentBytes;

impl Measurement for ResidentBytes {
    type Intermediate = ();
    type Value = u64;

    fn start(&self) -> Self::Intermediate {}

    fn end(&self, (): Self::Intermediate) -> Self::Value {
        resident_bytes()
    }

    fn add(&self, v1: &Self::Value, v2: &Self::Value) -> Self::Value {
        v1.saturating_add(*v2)
    }

    fn zero(&self) -> Self::Value {
        0
    }

    // Criterion's values are `f64`; a resident set is far below 2^53 bytes.
    #[allow(clippy::cast_precision_loss)]
    fn to_f64(&self, value: &Self::Value) -> f64 {
        *value as f64
    }

    fn formatter(&self) -> &dyn ValueFormatter {
        &MibFormatter
    }
}

/// Prints [`ResidentBytes`] values in MiB.
struct MibFormatter;

impl ValueFormatter for MibFormatter {
    fn scale_values(&self, _typical_value: f64, values: &mut [f64]) -> &'static str {
        for v in values {
            *v /= 1_048_576.0;
        }
        "MiB"
    }

    fn scale_throughputs(
        &self,
        _typical_value: f64,
        _throughput: &Throughput,
        values: &mut [f64],
    ) -> &'static str {
        self.scale_values(0.0, values)
    }

    fn scale_for_machines(&self, _values: &mut [f64]) -> &'static str {
        "B"
    }
}

/// Tasks in the seeded vault; the size `baseline.rs` records.
const TASKS: usize = 10_000;

fn bench_rss(c: &mut Criterion<ResidentBytes>) {
    let dir = seeded_vault_dir(TASKS, 100);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let core = rt
        .block_on(Core::open(core_config(dir.path()), bench_unlock()))
        .expect("open vault");

    let mut group = c.benchmark_group("rss");
    group
        .sample_size(10)
        // Ten samples of up to ten Today queries over 10k tasks need about
        // seven seconds; five warned that it could not finish.
        .measurement_time(Duration::from_secs(8));
    group.bench_function(format!("steady_{TASKS}"), |b| {
        b.iter_custom(|iters| {
            for _ in 0..iters {
                let today = rt
                    .block_on(core.query(Query::Today {
                        now_ms: FIXED_NOW_MS,
                        contexts: Vec::new(),
                    }))
                    .expect("query today");
                drop(today);
            }
            resident_bytes().saturating_mul(iters)
        });
    });
    group.finish();

    rt.block_on(core.close()).expect("close");
}

criterion_group! {
    name = benches;
    config = Criterion::default().with_measurement(ResidentBytes);
    targets = bench_rss
}
criterion_main!(benches);
