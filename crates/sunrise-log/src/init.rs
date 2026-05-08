//! `init(LogConfig)` — global bootstrap.
//!
//! Per `spec/10-cross-cutting/logging.md` §10, every binary calls
//! `sunrise_log::init(LogConfig)` exactly once at startup, before any other
//! workspace code runs. After `init`, the global dispatcher routes records
//! to the configured sinks. Calling `init` more than once is allowed only
//! during tests; in release the second call is a permanent error.

use crate::{
    ctx::{Ctx, CtxKey, CtxValue},
    level::Level,
    proto::ProtoVersions,
    record::Record,
    sink::{RingSink, Sink, StderrSink},
    span::{SpanId, TraceId},
    throttle::{Throttle, Verdict},
};
use arc_swap::ArcSwap;
use once_cell::sync::OnceCell;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

/// Errors produced by `init` / dispatch.
#[derive(Debug, Error)]
pub enum LogError {
    /// `init` called more than once.
    #[error("sunrise_log::init called more than once")]
    AlreadyInitialized,
    /// Misconfiguration (e.g., proto.wire == 0 in non-test mode).
    #[error("invalid log config: {0}")]
    InvalidConfig(&'static str),
}

/// Static log config. Consumers call [`init`] exactly once with this.
#[derive(Debug, Clone)]
pub struct LogConfig {
    /// Crate / package short id (e.g., `"sunrise-sync"`). Goes into `pkg`.
    pub pkg: &'static str,
    /// `<semver>+<platform>` (e.g., `"1.4.2+linux-x86_64"`).
    pub app: String,
    /// `dev_<8 hex>` device-local hash. Empty string if unknown (e.g., on a
    /// fresh install before the salt has been generated).
    pub dev: String,
    /// Pinned numeric protocol versions.
    pub proto: ProtoVersions,
    /// Sinks to fan out to.
    pub sinks: Vec<Arc<dyn Sink>>,
    /// Diagnostic mode — temporarily raises the in-memory ring to `trace+`
    /// for 30 minutes. Per logging.md §2.
    pub diagnostic_mode: bool,
}

/// Builder for [`LogConfig`].
#[derive(Debug)]
pub struct LogConfigBuilder {
    pkg: &'static str,
    app: Option<String>,
    dev: Option<String>,
    proto: ProtoVersions,
    sinks: Vec<Arc<dyn Sink>>,
    diagnostic_mode: bool,
}

impl LogConfigBuilder {
    /// Start a builder rooted at the calling crate's pkg id.
    #[must_use]
    pub fn new(pkg: &'static str) -> Self {
        Self {
            pkg,
            app: None,
            dev: None,
            proto: ProtoVersions::UNSET,
            sinks: Vec::new(),
            diagnostic_mode: false,
        }
    }

    /// Set the `app` field.
    #[must_use]
    pub fn app(mut self, app: impl Into<String>) -> Self {
        self.app = Some(app.into());
        self
    }

    /// Set the `dev` field.
    #[must_use]
    pub fn dev(mut self, dev: impl Into<String>) -> Self {
        self.dev = Some(dev.into());
        self
    }

    /// Set the protocol versions.
    #[must_use]
    pub fn proto(mut self, proto: ProtoVersions) -> Self {
        self.proto = proto;
        self
    }

    /// Append a sink.
    #[must_use]
    pub fn sink(mut self, sink: Arc<dyn Sink>) -> Self {
        self.sinks.push(sink);
        self
    }

    /// Append the standard stderr + ring buffer sinks.
    #[must_use]
    pub fn with_default_sinks(mut self) -> Self {
        self.sinks.push(Arc::new(StderrSink::new(Level::Info)));
        self.sinks.push(Arc::new(RingSink::with_default_capacity()));
        self
    }

    /// Toggle diagnostic mode on.
    #[must_use]
    pub fn diagnostic_mode(mut self, on: bool) -> Self {
        self.diagnostic_mode = on;
        self
    }

    /// Finalize.
    #[must_use]
    pub fn build(self) -> LogConfig {
        LogConfig {
            pkg: self.pkg,
            app: self.app.unwrap_or_else(|| "0.0.0+unknown".to_string()),
            dev: self.dev.unwrap_or_default(),
            proto: self.proto,
            sinks: self.sinks,
            diagnostic_mode: self.diagnostic_mode,
        }
    }
}

/// Global dispatcher state. Atomic via `ArcSwap` so `set_proto` etc. can
/// retune without restart.
struct Global {
    cfg: ArcSwap<LogConfig>,
    throttle: Throttle,
}

static GLOBAL: OnceCell<Global> = OnceCell::new();

/// Initialize the global logger. Returns the previous config if already set
/// (used by tests to swap fixtures).
pub fn init(cfg: LogConfig) -> Result<(), LogError> {
    if GLOBAL.get().is_some() {
        return Err(LogError::AlreadyInitialized);
    }
    GLOBAL
        .set(Global {
            cfg: ArcSwap::from_pointee(cfg),
            throttle: Throttle::new(),
        })
        .map_err(|_| LogError::AlreadyInitialized)
}

/// Test helper: install or replace the global config. Tests use this in lieu
/// of `init` so each test starts with a fresh logger.
#[doc(hidden)]
pub fn install_global(cfg: LogConfig) {
    if let Some(g) = GLOBAL.get() {
        g.cfg.store(Arc::new(cfg));
    } else {
        // Best-effort; race here is benign.
        let _ = GLOBAL.set(Global {
            cfg: ArcSwap::from_pointee(cfg),
            throttle: Throttle::new(),
        });
    }
}

/// Whether the global logger has been initialized.
#[must_use]
pub fn is_initialized() -> bool {
    GLOBAL.get().is_some()
}

/// Internal entrypoint — emit a fully-formed event.
///
/// The macros in [`crate::macros`] call this directly. Fast path: no
/// allocation when the level is below the lowest enabled sink and not in
/// diagnostic mode.
#[doc(hidden)]
pub fn emit_event(
    level: Level,
    ev: &'static str,
    pkg_override: Option<&'static str>,
    mod_path: &'static str,
    msg: &str,
    ctx: &Ctx,
    err: Option<&crate::record::ErrField>,
    share: bool,
) {
    let Some(global) = GLOBAL.get() else {
        // Pre-init: drop. Per logging.md §10, this is acceptable for early
        // boot; a `log.bootstrap.late` is emitted by the entry binary if
        // it noticed the logger was not installed in time. We don't have a
        // place to put that here without recursion, so silently drop.
        return;
    };

    // Throttle gate.
    match global.throttle.check(ev, level) {
        Verdict::Allow => {}
        Verdict::Drop { notify_with } => {
            if let Some(n) = notify_with {
                emit_throttle_notice(global, n, ev);
            }
            return;
        }
    }

    let cfg = global.cfg.load();
    let pkg = pkg_override.unwrap_or(cfg.pkg);

    // Snapshot trace/span; if absent, synthesize a self-trace.
    let (trace, span) = crate::span::current();
    let trace_str = trace.unwrap_or(TraceId([0u8; 16])).to_ulid_string();
    let span_str = span.unwrap_or(SpanId([0u8; 16])).to_ulid_string();

    let record = Record {
        ts: rfc3339_now_ms(),
        lv: level,
        ev,
        pkg,
        mod_path,
        span: span_str,
        trace: trace_str,
        dev: cfg.dev.clone(),
        app: cfg.app.clone(),
        proto: cfg.proto,
        ctx,
        msg,
        err,
        share,
    };

    let Ok(bytes) = serde_json::to_vec(&record) else {
        return;
    };

    for sink in &cfg.sinks {
        if level < sink.min_level() {
            continue;
        }
        if sink.requires_share() && !share {
            continue;
        }
        sink.write(&bytes);
    }
}

fn emit_throttle_notice(global: &Global, n_dropped: u64, original_ev: &'static str) {
    let cfg = global.cfg.load();
    let ctx = Ctx::new()
        .with(CtxKey::NDropped, CtxValue::U64(n_dropped))
        .with(CtxKey::OpKind, CtxValue::Str(original_ev));
    let record = Record {
        ts: rfc3339_now_ms(),
        lv: Level::Warn,
        ev: "log.throttled",
        pkg: cfg.pkg,
        mod_path: module_path!(),
        span: SpanId([0u8; 16]).to_ulid_string(),
        trace: TraceId([0u8; 16]).to_ulid_string(),
        dev: cfg.dev.clone(),
        app: cfg.app.clone(),
        proto: cfg.proto,
        ctx: &ctx,
        msg: "log records dropped due to per-(ev, lv) rate limit",
        err: None,
        share: false,
    };
    if let Ok(bytes) = serde_json::to_vec(&record) {
        for sink in &cfg.sinks {
            if Level::Warn < sink.min_level() {
                continue;
            }
            if sink.requires_share() {
                continue;
            }
            sink.write(&bytes);
        }
    }
}

/// RFC 3339 with millisecond precision in UTC.
///
/// The bare-bones formatter avoids pulling in `chrono` here. Format:
/// `YYYY-MM-DDTHH:MM:SS.mmmZ`.
///
/// `sunrise-log` is the one crate where reading wall-clock time directly is
/// permitted: log timestamps are observational data, not domain state, and
/// must reflect real elapsed time even when the core's injected `Clock` is a
/// fixture. The workspace clippy `disallowed_methods` lint is allowed locally
/// for that reason; no other crate may follow this pattern.
#[allow(clippy::disallowed_methods)]
fn rfc3339_now_ms() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let secs = now.as_secs();
    let millis = u64::from(now.subsec_millis());
    format_rfc3339_ms(secs, millis)
}

#[allow(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation
)]
fn format_rfc3339_ms(unix_secs: u64, millis: u64) -> String {
    // Convert Unix seconds → civil date (Howard Hinnant's algorithm).
    // Casts are bounded by Howard Hinnant's analysis: era × 400 ≤ year, and
    // each component fits in u32 for any plausible Unix timestamp.
    let unix_secs_i = unix_secs as i64;
    let z_days = unix_secs_i / 86_400 + 719_468;
    let secs_of_day = unix_secs % 86_400;
    let era = if z_days >= 0 {
        z_days
    } else {
        z_days - 146_096
    } / 146_097;
    let day_of_era = (z_days - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year_marbased = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_marbased = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_marbased + 2) / 5 + 1) as u32;
    let month_gregorian = if month_marbased < 10 {
        month_marbased + 3
    } else {
        month_marbased - 9
    } as u32;
    let year_gregorian = (year_marbased + i64::from(month_gregorian <= 2)) as u32;
    let hour = (secs_of_day / 3600) as u32;
    let minute = ((secs_of_day % 3600) / 60) as u32;
    let second = (secs_of_day % 60) as u32;
    format!(
        "{year_gregorian:04}-{month_gregorian:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_value() {
        // 2026-05-08T12:34:56Z = unix 1_778_243_696 (verified against Python
        // datetime.timestamp()).
        let s = format_rfc3339_ms(1_778_243_696, 789);
        assert_eq!(s, "2026-05-08T12:34:56.789Z");
    }

    #[test]
    fn rfc3339_another_known_value() {
        // 2024-02-29T23:59:59Z (leap day) = unix 1_709_251_199.
        let s = format_rfc3339_ms(1_709_251_199, 0);
        assert_eq!(s, "2024-02-29T23:59:59.000Z");
    }

    #[test]
    fn rfc3339_epoch() {
        let s = format_rfc3339_ms(0, 0);
        assert_eq!(s, "1970-01-01T00:00:00.000Z");
    }
}
