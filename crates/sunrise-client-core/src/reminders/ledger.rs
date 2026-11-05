//! What this device has already shown, so a re-plan never shows it twice
//! (`docs/08-features/notifications.md` §Security events, §Sync stalled,
//! §Time zone changed, §Morning brief and triage nudge).
//!
//! The scheduled kinds need no memory: a re-plan names the same identifiers,
//! and the reconcile keeps what is held. The on-event kinds and the triage
//! nudge do. A security alert is posted once per event and device; a stall
//! is raised once and cleared silently; the zone notice compares with the
//! zone this device last observed; the nudge fires at most once a day and
//! only when the queue changed since it last fired.
//!
//! # The caller keeps it
//!
//! A ledger is device-local and never synced, and it holds no content: ids,
//! a zone name, a date and a fingerprint of the triage queue. It is a value
//! here, read from and written back to wherever the caller keeps device
//! state, as [`crate::history::persist`] is — the Apple client beside its
//! settings, the CLI beside the vault. [`NotificationLedger::from_json`] reads
//! what [`NotificationLedger::to_json`] wrote; anything else, a missing file
//! included, reads as a fresh ledger.
//!
//! # A fresh ledger adopts what is there
//!
//! The first plan with a fresh ledger posts no security alert: it records the
//! events the vault already holds as shown. A device that adopts this build
//! months after pairing would otherwise announce every device that joined
//! since; one that just paired holds no events yet
//! ([`sunrise_core::Core::security_events`] starts at its own certificate).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// The version of the written form. Another version reads as fresh.
pub const LEDGER_VERSION: u32 = 1;

/// One planned or fired triage nudge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NudgeRecord {
    /// The planner day it is for.
    pub date: jiff::civil::Date,
    /// [`super::triage_fingerprint`] of the queue it named.
    pub fingerprint: String,
    /// When it fires, epoch ms.
    pub fire_at_ms: i64,
}

/// What this device has shown. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationLedger {
    /// [`LEDGER_VERSION`].
    #[serde(default)]
    pub version: u32,
    /// Whether a plan has run with it: false adopts the events held.
    #[serde(default)]
    pub initialized: bool,
    /// The identifiers of the on-event alerts posted.
    #[serde(default)]
    pub posted: BTreeSet<String>,
    /// Whether the current stall has been raised.
    #[serde(default)]
    pub stall_raised: bool,
    /// The zone this device last observed, an IANA name.
    #[serde(default)]
    pub last_zone: Option<String>,
    /// The nudge handed to the OS and not yet due.
    #[serde(default)]
    pub nudge_planned: Option<NudgeRecord>,
    /// The last nudge that came due.
    #[serde(default)]
    pub nudged: Option<NudgeRecord>,
}

impl Default for NotificationLedger {
    fn default() -> Self {
        Self {
            version: LEDGER_VERSION,
            initialized: false,
            posted: BTreeSet::new(),
            stall_raised: false,
            last_zone: None,
            nudge_planned: None,
            nudged: None,
        }
    }
}

impl NotificationLedger {
    /// A fresh ledger, as a device that never planned holds.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The ledger `json` holds, or a fresh one for anything else.
    #[must_use]
    pub fn from_json(json: Option<&str>) -> Self {
        json.and_then(|j| serde_json::from_str::<Self>(j).ok())
            .filter(|l| l.version == LEDGER_VERSION)
            .unwrap_or_default()
    }

    /// The written form.
    #[must_use]
    pub fn to_json(&self) -> String {
        // A struct of strings, sets and plain integers always serializes.
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Move a planned nudge that has come due to [`Self::nudged`]: the OS
    /// fired it, or would have.
    pub fn settle_nudge(&mut self, now_ms: i64) {
        if self
            .nudge_planned
            .as_ref()
            .is_some_and(|p| p.fire_at_ms <= now_ms)
        {
            self.nudged = self.nudge_planned.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_round_trips_and_anything_else_reads_fresh() {
        let mut l = NotificationLedger::new();
        l.initialized = true;
        l.posted.insert("sunrise.alert.device_paired.x".into());
        l.last_zone = Some("Europe/Paris".into());
        assert_eq!(NotificationLedger::from_json(Some(&l.to_json())), l);
        for junk in [None, Some(""), Some("{"), Some(r#"{"version":9}"#)] {
            assert_eq!(
                NotificationLedger::from_json(junk),
                NotificationLedger::new()
            );
        }
    }

    #[test]
    fn a_planned_nudge_settles_once_it_is_due() {
        let rec = NudgeRecord {
            date: jiff::civil::date(2026, 3, 2),
            fingerprint: "f".into(),
            fire_at_ms: 100,
        };
        let mut l = NotificationLedger {
            nudge_planned: Some(rec.clone()),
            ..NotificationLedger::new()
        };
        l.settle_nudge(99);
        assert_eq!(
            (l.nudge_planned.is_some(), l.nudged.is_none()),
            (true, true)
        );
        l.settle_nudge(100);
        assert_eq!(l.nudged, Some(rec));
        assert!(l.nudge_planned.is_none());
    }
}
