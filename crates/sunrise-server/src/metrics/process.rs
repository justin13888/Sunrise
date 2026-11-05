//! The standard Prometheus process collector: `process_*`.
//!
//! The names and meanings are the ones every Prometheus client library
//! exports (<https://prometheus.io/docs/instrumenting/writing_clientlibs/#process-metrics>),
//! so a stock dashboard reads them unchanged. They are sampled at scrape time
//! from `/proc/self`, which is the authoritative state `metrics.md` §Rules asks
//! a gauge to come from, and nothing on a request path touches them.
//!
//! # Linux only
//!
//! `/proc/self` is Linux's. Reading the same facts on macOS takes
//! `proc_pidinfo` and `getrusage`, which are C calls, and the workspace forbids
//! `unsafe`. The relay ships as a Linux container, so every deployment has the
//! series; a development build on another platform has none, rather than
//! zeros a dashboard would read as an idle process. The parsers below are pure
//! and tested on every platform; only the file reads are Linux's.

use super::Metrics;

/// The kernel's clock tick for the times in `/proc/self/stat`.
///
/// `USER_HZ` is part of the kernel's userspace ABI and is 100 on every
/// architecture Linux ships, independent of the kernel's own `HZ`. Reading
/// `sysconf(_SC_CLK_TCK)` would need `libc`; the Prometheus Go collector
/// (`procfs`) hard-codes the same constant for the same reason.
const USER_HZ: f64 = 100.0;

/// One reading of the process.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcessSample {
    /// User plus system CPU time, in seconds.
    pub cpu_seconds: f64,
    /// Resident set size, in bytes.
    pub resident_bytes: u64,
    /// Virtual memory size, in bytes.
    pub virtual_bytes: u64,
    /// Threads in the process.
    pub threads: u64,
    /// Open file descriptors.
    pub open_fds: u64,
    /// The soft limit on open file descriptors; `None` when unlimited.
    pub max_fds: Option<u64>,
    /// Unix time the process started, in seconds; `None` when the boot time
    /// could not be read.
    pub start_time_seconds: Option<f64>,
}

/// The fields of `/proc/self/stat` the collector reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    /// `utime + stime`, in clock ticks.
    pub cpu_ticks: u64,
    /// `num_threads`.
    pub threads: u64,
    /// `starttime`: clock ticks after boot at which the process started.
    pub start_ticks: u64,
    /// `vsize`, in bytes.
    pub virtual_bytes: u64,
}

/// Parse `/proc/self/stat`.
///
/// The second field is the command name in parentheses, and it may itself
/// hold spaces and parentheses, so the fields are counted from the *last*
/// `)`. What follows it is field 3, `state`.
#[must_use]
pub fn parse_stat(stat: &str) -> Option<Stat> {
    let (_, rest) = stat.rsplit_once(')')?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // Field N of proc(5) is `fields[N - 3]`.
    let field = |n: usize| fields.get(n - 3)?.parse::<u64>().ok();
    Some(Stat {
        cpu_ticks: field(14)?.checked_add(field(15)?)?,
        threads: field(20)?,
        start_ticks: field(22)?,
        virtual_bytes: field(23)?,
    })
}

/// `VmRSS` from `/proc/self/status`, in bytes.
///
/// Read here rather than as `rss` from `stat`, which is in pages: the page
/// size is another `sysconf`.
#[must_use]
pub fn parse_resident_bytes(status: &str) -> Option<u64> {
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let mut parts = line["VmRSS:".len()..].split_whitespace();
    let kib: u64 = parts.next()?.parse().ok()?;
    (parts.next() == Some("kB")).then_some(kib.checked_mul(1024)?)
}

/// The soft `Max open files` limit from `/proc/self/limits`; `None` when it
/// is `unlimited` or absent.
#[must_use]
pub fn parse_max_fds(limits: &str) -> Option<u64> {
    let line = limits.lines().find(|l| l.starts_with("Max open files"))?;
    line["Max open files".len()..]
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// `btime` from `/proc/stat`: Unix time the system booted, in seconds.
#[must_use]
pub fn parse_boot_time(proc_stat: &str) -> Option<u64> {
    proc_stat
        .lines()
        .find_map(|l| l.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()
}

/// Assemble a sample from the files' contents and the descriptor count.
#[must_use]
pub fn from_parts(
    stat: &str,
    status: &str,
    limits: &str,
    proc_stat: &str,
    open_fds: u64,
) -> Option<ProcessSample> {
    let s = parse_stat(stat)?;
    // Tick counts stay far below 2^53 for any process that has ever run.
    #[allow(clippy::cast_precision_loss)]
    let ticks = |t: u64| t as f64 / USER_HZ;
    Some(ProcessSample {
        cpu_seconds: ticks(s.cpu_ticks),
        resident_bytes: parse_resident_bytes(status)?,
        virtual_bytes: s.virtual_bytes,
        threads: s.threads,
        open_fds,
        max_fds: parse_max_fds(limits),
        #[allow(clippy::cast_precision_loss)]
        start_time_seconds: parse_boot_time(proc_stat)
            .map(|boot| boot as f64 + ticks(s.start_ticks)),
    })
}

/// Read this process's sample, on Linux. `None` elsewhere, or when a file
/// could not be read.
#[must_use]
pub fn sample() -> Option<ProcessSample> {
    #[cfg(target_os = "linux")]
    {
        let read = |p: &str| std::fs::read_to_string(p).ok();
        let open_fds = std::fs::read_dir("/proc/self/fd").ok()?.count() as u64;
        from_parts(
            &read("/proc/self/stat")?,
            &read("/proc/self/status")?,
            &read("/proc/self/limits").unwrap_or_default(),
            &read("/proc/stat").unwrap_or_default(),
            open_fds,
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Write `sample` into `metrics` under the standard names.
#[allow(clippy::cast_precision_loss)]
pub fn record(metrics: &Metrics, sample: &ProcessSample) {
    metrics.set_counter("process_cpu_seconds_total", &[], sample.cpu_seconds);
    metrics.set_gauge(
        "process_resident_memory_bytes",
        &[],
        sample.resident_bytes as f64,
    );
    metrics.set_gauge(
        "process_virtual_memory_bytes",
        &[],
        sample.virtual_bytes as f64,
    );
    metrics.set_gauge("process_threads", &[], sample.threads as f64);
    metrics.set_gauge("process_open_fds", &[], sample.open_fds as f64);
    if let Some(max) = sample.max_fds {
        metrics.set_gauge("process_max_fds", &[], max as f64);
    }
    if let Some(start) = sample.start_time_seconds {
        metrics.set_gauge("process_start_time_seconds", &[], start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command name holding `) (` and spaces, which a split on the first
    /// `)` would misread every later field of.
    const STAT: &str = "4242 (sunrise ) (srv) S 1 4242 4242 0 -1 4194560 1200 0 3 0 \
        250 75 0 0 20 0 9 0 12350 104857600 2048 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0";

    const STATUS: &str =
        "Name:\tsunrise-server\nVmPeak:\t  110000 kB\nVmRSS:\t    8192 kB\nThreads:\t9\n";

    const LIMITS: &str =
        "Limit                     Soft Limit           Hard Limit           Units     \n\
        Max cpu time              unlimited            unlimited            seconds   \n\
        Max open files            1024                 1048576              files     \n";

    #[test]
    fn stat_fields_are_counted_from_the_last_parenthesis() {
        assert_eq!(
            parse_stat(STAT),
            Some(Stat {
                cpu_ticks: 325,
                threads: 9,
                start_ticks: 12350,
                virtual_bytes: 104_857_600,
            })
        );
        assert_eq!(parse_stat("4242 (truncated) S 1"), None);
        assert_eq!(parse_stat("no parenthesis at all"), None);
    }

    #[test]
    fn the_small_files_parse_and_refuse_what_they_cannot_read() {
        assert_eq!(parse_resident_bytes(STATUS), Some(8192 * 1024));
        assert_eq!(parse_resident_bytes("VmRSS:\t12 pages\n"), None);
        assert_eq!(parse_resident_bytes("Name:\tx\n"), None);
        assert_eq!(parse_max_fds(LIMITS), Some(1024));
        assert_eq!(
            parse_max_fds(
                "Max open files            unlimited            unlimited            files\n"
            ),
            None
        );
        assert_eq!(
            parse_boot_time("cpu  1 2 3\nbtime 1700000000\n"),
            Some(1_700_000_000)
        );
        assert_eq!(parse_boot_time("cpu  1 2 3\n"), None);
    }

    #[test]
    fn a_sample_converts_ticks_to_seconds_and_renders_the_standard_names() {
        let sample = from_parts(STAT, STATUS, LIMITS, "btime 1700000000\n", 12).unwrap();
        assert!((sample.cpu_seconds - 3.25).abs() < 1e-9);
        assert_eq!(sample.start_time_seconds, Some(1_700_000_123.5));

        let m = Metrics::new();
        record(&m, &sample);
        let s = m.render();
        for line in [
            "# TYPE process_cpu_seconds_total counter\nprocess_cpu_seconds_total 3.25\n",
            "process_resident_memory_bytes 8388608\n",
            "process_virtual_memory_bytes 104857600\n",
            "process_threads 9\n",
            "process_open_fds 12\n",
            "process_max_fds 1024\n",
            "process_start_time_seconds 1700000123.5\n",
        ] {
            assert!(s.contains(line), "missing {line:?} in:\n{s}");
        }
    }

    /// No boot time and no limit leave those two series out rather than
    /// reporting a zero that reads as a fact.
    #[test]
    fn an_unreadable_limit_or_boot_time_is_absent_not_zero() {
        let sample = from_parts(STAT, STATUS, "", "", 3).unwrap();
        let m = Metrics::new();
        record(&m, &sample);
        let s = m.render();
        assert!(!s.contains("process_max_fds"), "{s}");
        assert!(!s.contains("process_start_time_seconds"), "{s}");
        assert!(s.contains("process_open_fds 3\n"), "{s}");
    }

    /// On Linux the live read works against this very process.
    #[test]
    #[cfg(target_os = "linux")]
    fn this_process_can_be_sampled() {
        let s = sample().expect("/proc/self is readable");
        assert!(
            s.threads >= 1 && s.resident_bytes > 0 && s.open_fds > 0,
            "{s:?}"
        );
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn other_platforms_have_no_sample() {
        assert_eq!(sample(), None);
    }
}
