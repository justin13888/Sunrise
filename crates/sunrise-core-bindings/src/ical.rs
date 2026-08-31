//! The iCalendar (RFC 5545) seam.
//!
//! [`SunriseCore::import_ical`](crate::SunriseCore::import_ical) and
//! [`SunriseCore::export_ical`](crate::SunriseCore::export_ical) are what a
//! "File → Import .ics…" / "Export…" menu item calls. The work is
//! `sunrise_integrations::ical_vault`'s; this module is the vocabulary that
//! crosses.
//!
//! # Text, not a path
//!
//! Import takes the document's **text** and export returns it, for the reason
//! [`SunriseCore::attach_file`](crate::SunriseCore::attach_file) takes bytes:
//! on macOS a file the user picked arrives with a security scope only the app
//! holds, so the Rust side cannot open it, and the save panel's destination is
//! likewise the app's to write. The app reads and writes; the core parses and
//! renders.
//!
//! # The notices are not telemetry
//!
//! An `.ics` from a real calendar client routinely contains things a Block
//! cannot hold — a description, a recurrence rule, a `VTODO`. Those are
//! reported, one [`IcalNotice`] each, and a client that drops them on the
//! floor is silently losing the user's data on their behalf.
//! `docs/09-integrations/icalendar.md` §Edge cases requires them shown.

use sunrise_id::EntityRef;
use sunrise_integrations::ical::{ICalNotice, NoticeCode};
use sunrise_integrations::ical_vault::{ExportWindow, ImportReport, ImportedBlock};

/// See [`sunrise_integrations::ical::NoticeCode`].
///
/// Declared remotely rather than mirrored so that a new variant upstream fails
/// this crate's build instead of becoming an unrepresentable value at runtime.
#[uniffi::remote(Enum)]
pub enum NoticeCode {
    UnsupportedComponent,
    UnmappedProperty,
    UnknownTimezone,
    BadValue,
    Skipped,
}

/// See [`sunrise_integrations::ical_vault::ExportWindow`].
#[uniffi::remote(Enum)]
pub enum ExportWindow {
    Day,
    Week,
}

/// One thing an import could not carry, and why.
#[derive(Debug, Clone, uniffi::Record)]
pub struct IcalNotice {
    /// What kind of loss this is; branch on it to decide how loudly to say so.
    pub code: NoticeCode,
    /// The `UID` of the event it happened in, when it happened inside one.
    pub uid: Option<String>,
    /// Human-readable detail, already phrased for display.
    pub detail: String,
}

impl From<&ICalNotice> for IcalNotice {
    fn from(n: &ICalNotice) -> Self {
        let ICalNotice { code, uid, detail } = n;
        Self {
            code: *code,
            uid: uid.clone(),
            detail: detail.clone(),
        }
    }
}

/// One `VEVENT` that reached the vault.
#[derive(Debug, Clone, uniffi::Record)]
pub struct IcalImportedBlock {
    /// The Block it was written to. Re-importing the same file returns this
    /// same id, which is what makes the import idempotent.
    pub block: EntityRef,
    /// The `UID` it was keyed by.
    pub uid: String,
    /// The title written.
    pub title: String,
    /// `true` when this event minted the Block; `false` when it landed on one
    /// an earlier import had already made.
    pub created: bool,
}

impl From<&ImportedBlock> for IcalImportedBlock {
    fn from(b: &ImportedBlock) -> Self {
        let ImportedBlock {
            block,
            uid,
            title,
            created,
        } = b;
        Self {
            block: *block,
            uid: uid.clone(),
            title: title.clone(),
            created: *created,
        }
    }
}

/// What an import did — the whole result of one menu-item invocation.
#[derive(Debug, Clone, uniffi::Record)]
pub struct IcalImportReport {
    /// The Blocks written, in file order.
    pub blocks: Vec<IcalImportedBlock>,
    /// How many of them are new. Counted here rather than left to the client,
    /// so "3 events imported, 12 already up to date" says the same thing in
    /// every client.
    pub created: u32,
    /// How many landed on a Block a previous import already made.
    pub updated: u32,
    /// Events that could not become a Block at all. Each has a notice.
    pub failed: u32,
    /// Everything the import could not carry.
    pub notices: Vec<IcalNotice>,
}

impl From<&ImportReport> for IcalImportReport {
    fn from(r: &ImportReport) -> Self {
        let created = r.created();
        let updated = r.updated();
        let ImportReport {
            blocks,
            failed,
            notices,
        } = r;
        Self {
            blocks: blocks.iter().map(IcalImportedBlock::from).collect(),
            created,
            updated,
            failed: *failed,
            notices: notices.iter().map(IcalNotice::from).collect(),
        }
    }
}
