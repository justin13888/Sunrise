//! Driving an `.ics` file into, and out of, a live vault.
//!
//! The one layer here that is not pure: it takes a [`Core`] and submits
//! commands. Everything it decides — which Block an event is, what the range
//! is, what could not be carried — is decided by [`crate::ical`] and
//! [`crate::ical_map`], which need no vault at all.
//!
//! # Idempotence
//!
//! Import writes through [`Command::ImportBlock`], which addresses the Block
//! by `(source, uid)` instead of minting an id. Importing the same file twice
//! therefore writes the same Block twice rather than two Blocks, and two
//! devices importing it independently converge on one. See
//! [`sunrise_domain::import`] for the derivation and why it lives in the id.
//!
//! # Failure policy
//!
//! One unusable `VEVENT` does not fail the import. A calendar exported from a
//! real client routinely contains something this build cannot model, and
//! refusing the other 200 events over it would make the feature useless. Each
//! refusal is counted and reported instead.

use crate::ical::{self, ICalNotice, NoticeCode};
use crate::ical_map::{block_to_event, event_to_block};
use crate::IntegrationError;
use sunrise_core::{Command, Core, Query, QueryResult};
use sunrise_id::EntityRef;

/// The `import_source_id` for a one-shot `.ics` file import.
///
/// A constant, not the file's path: a user who re-downloads the same calendar
/// to `~/Downloads/basic (1).ics` is re-importing the same events, and keying
/// on the path would give them a second copy of every one. A caller that
/// really does want two independent copies names its own source.
pub const ICS_SOURCE: &str = "ics";

/// One `VEVENT` that reached the vault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedBlock {
    /// The Block it was written to.
    pub block: EntityRef,
    /// The `UID` it was keyed by.
    pub uid: String,
    /// The title written, which is what a caller echoes back to the user.
    pub title: String,
    /// `true` when this event minted the Block, `false` when it landed on one
    /// an earlier import of the same `(source, uid)` had already made.
    pub created: bool,
}

/// What an import did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// The Blocks written, in file order.
    pub blocks: Vec<ImportedBlock>,
    /// Events this build could not turn into a Block. Each has a notice.
    pub failed: u32,
    /// Everything the import could not carry, from every layer.
    pub notices: Vec<ICalNotice>,
}

/// Which slice of the calendar an export covers.
///
/// Day and week only, because `Query::DayBlocks` and `Query::WeekBlocks` are
/// the two windows the core reads Blocks over. `docs/09-integrations/
/// icalendar.md` also describes "export this Stream as .ics"; that needs a
/// stream-scoped Block query the core does not have, and inventing one here by
/// paging day windows would be a worse answer than not offering it yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportWindow {
    /// The civil day containing the given instant.
    Day,
    /// The Monday-first civil week containing the given instant.
    Week,
}

/// Import every `VEVENT` in `text` as a Block in `stream_id`.
///
/// # Errors
///
/// [`IntegrationError::Decode`] when the input is not an iCalendar document at
/// all, and [`IntegrationError::Core`] when the vault refuses a write. A
/// single unmappable event is reported in the result, not raised.
pub async fn import(
    core: &Core,
    text: &str,
    stream_id: EntityRef,
    source: &str,
) -> Result<ImportReport, IntegrationError> {
    let calendar = ical::parse(text)?;
    let mut report = ImportReport {
        notices: calendar.notices,
        ..ImportReport::default()
    };

    for ev in &calendar.events {
        let mapped = match event_to_block(ev, stream_id) {
            Ok(m) => m,
            Err(notice) => {
                report.failed += 1;
                report.notices.push(notice);
                continue;
            }
        };
        report.notices.extend(mapped.notices);

        // Asked before the write, because after it the answer is always
        // "present". Only the reporting depends on it: the write itself is the
        // same command either way.
        let id = sunrise_domain::imported_block_id(source, &mapped.uid);
        let created = !block_exists(core, id).await;
        let title = mapped.draft.title.clone().unwrap_or_default();

        let res = core
            .submit(Command::ImportBlock {
                source: source.to_string(),
                uid: mapped.uid.clone(),
                draft: mapped.draft,
            })
            .await
            .map_err(|e| IntegrationError::Core(e.to_string()))?;
        report.blocks.push(ImportedBlock {
            block: res.entity,
            uid: mapped.uid,
            title,
            created,
        });
    }
    Ok(report)
}

/// Render the Blocks in one window as an iCalendar document.
///
/// # Errors
///
/// [`IntegrationError::Core`] when the vault cannot be read.
pub async fn export(
    core: &Core,
    window: ExportWindow,
    at_ms: u64,
) -> Result<String, IntegrationError> {
    let q = match window {
        ExportWindow::Day => Query::DayBlocks { day_ms: at_ms },
        ExportWindow::Week => Query::WeekBlocks { week_ms: at_ms },
    };
    let QueryResult::Blocks(rows) = core
        .query(q)
        .await
        .map_err(|e| IntegrationError::Core(e.to_string()))?
    else {
        return Err(IntegrationError::Provider(
            "the calendar query returned something other than blocks".into(),
        ));
    };
    let events: Vec<_> = rows
        .iter()
        .filter(|r| !r.block.deleted)
        .map(|r| block_to_event(&r.block, r.title.as_deref()))
        .collect();
    Ok(ical::write(&events))
}

/// Whether a Block with this id is already materialized here.
///
/// A read that fails for any reason answers "no": the only thing riding on it
/// is which counter goes up, and the write that follows reports a real vault
/// failure on its own.
async fn block_exists(core: &Core, id: EntityRef) -> bool {
    matches!(
        core.query(Query::EntityById(id)).await,
        Ok(QueryResult::Blocks(rows)) if !rows.is_empty()
    )
}

impl ImportReport {
    /// Events that minted a Block this vault did not have.
    #[must_use]
    pub fn created(&self) -> u32 {
        u32::try_from(self.blocks.iter().filter(|b| b.created).count()).unwrap_or(u32::MAX)
    }

    /// Events that landed on a Block an earlier import already made — the
    /// re-import case, and the one the idempotence rule exists for.
    #[must_use]
    pub fn updated(&self) -> u32 {
        u32::try_from(self.blocks.iter().filter(|b| !b.created).count()).unwrap_or(u32::MAX)
    }

    /// The Blocks written, in file order. Comparable across two runs of the
    /// same file, which is exactly what idempotence means here.
    #[must_use]
    pub fn block_ids(&self) -> Vec<EntityRef> {
        self.blocks.iter().map(|b| b.block).collect()
    }

    /// One line for a human, in the shape the other CLI summaries use.
    #[must_use]
    pub fn summary_line(&self) -> String {
        format!(
            "{} created, {} updated, {} skipped, {} notices",
            self.created(),
            self.updated(),
            self.failed,
            self.notices.len()
        )
    }

    /// The notices that describe a whole item being dropped, which is the
    /// subset a caller must show even if it hides the rest.
    pub fn dropped(&self) -> impl Iterator<Item = &ICalNotice> {
        self.notices.iter().filter(|n| {
            matches!(
                n.code,
                NoticeCode::Skipped | NoticeCode::UnsupportedComponent
            )
        })
    }
}
