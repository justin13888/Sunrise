//! What a core logged, captured so a property can read it.
//!
//! The no-break half of the property is partly about logs: the sync driver
//! does not return an inbound op it classed as corruption to anyone, it logs
//! `sync.loss_evidence` with `cause = "corrupt_op"` and carries on. The
//! baseline driver captures that in its own process; this is the same capture
//! for the `HEAD` cores in the test process.

use std::sync::{Mutex, OnceLock};

use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

/// One captured log event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedEvent {
    /// `ERROR`, `WARN`, `DEBUG` and so on.
    pub level: String,
    /// The catalogued event name (`ev = "..."`), when the event carries one.
    pub ev: Option<String>,
    /// The `cause` field, when the event carries one.
    pub cause: Option<String>,
    /// The human-readable message.
    pub message: String,
}

impl LoggedEvent {
    /// Whether this is the sync driver classing an inbound op as corruption.
    #[must_use]
    pub fn is_corrupt_op(&self) -> bool {
        self.ev.as_deref() == Some("sync.loss_evidence")
            && self.cause.as_deref() == Some("corrupt_op")
    }

    /// Whether this is a `HEAD` core publishing a `StreamDigest` (ADR-0043
    /// §5): an op kind a baseline cut before #320's parking fix cannot read.
    #[must_use]
    pub fn is_digest_published(&self) -> bool {
        self.ev.as_deref() == Some("core.chain.digest_published")
    }

    /// Read one event out of the baseline driver's JSON form, which uses the
    /// same field names.
    pub(crate) fn from_json(v: &serde_json::Value) -> Self {
        let text = |k: &str| {
            v.get(k)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        Self {
            level: text("level").unwrap_or_default(),
            ev: text("ev"),
            cause: text("cause"),
            message: text("message").unwrap_or_default(),
        }
    }
}

static HEAD_EVENTS: Mutex<Vec<LoggedEvent>> = Mutex::new(Vec::new());
static INSTALLED: OnceLock<bool> = OnceLock::new();

#[derive(Default)]
struct Fields {
    ev: Option<String>,
    cause: Option<String>,
    message: String,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "ev" => self.ev = Some(value.to_owned()),
            "cause" => self.cause = Some(value.to_owned()),
            "message" => value.clone_into(&mut self.message),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let text = format!("{value:?}");
        match field.name() {
            "ev" => self.ev = Some(text),
            "cause" => self.cause = Some(text),
            "message" => self.message = text,
            _ => {}
        }
    }
}

struct Capture;

impl<S: tracing::Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let level = *event.metadata().level();
        // Two debug events are kept: the sync driver's loss evidence, and a
        // published digest, which is what explains a baseline's corruption
        // log when the baseline predates parking.
        let kept = matches!(
            fields.ev.as_deref(),
            Some("sync.loss_evidence" | "core.chain.digest_published")
        );
        if level > tracing::Level::WARN && !kept {
            return;
        }
        if let Ok(mut events) = HEAD_EVENTS.lock() {
            events.push(LoggedEvent {
                level: level.to_string(),
                ev: fields.ev,
                cause: fields.cause,
                message: fields.message,
            });
        }
    }
}

/// Install the capture as this process's global subscriber, once.
///
/// Returns whether the capture is the installed subscriber. `false` means
/// something else in the process installed one first, and the `HEAD`-side
/// corruption check would read nothing; a caller asserts on it rather than
/// pass vacuously.
pub fn install_event_capture() -> bool {
    *INSTALLED.get_or_init(|| {
        let filter = tracing_subscriber::filter::filter_fn(|meta| {
            meta.target().starts_with("sunrise") || *meta.level() <= tracing::Level::WARN
        });
        let subscriber = tracing_subscriber::registry().with(Capture.with_filter(filter));
        tracing::subscriber::set_global_default(subscriber).is_ok()
    })
}

/// Drain every event the `HEAD` cores in this process logged since the last
/// call.
pub(crate) fn take_head_events() -> Vec<LoggedEvent> {
    HEAD_EVENTS
        .lock()
        .map(|mut e| std::mem::take(&mut *e))
        .unwrap_or_default()
}
