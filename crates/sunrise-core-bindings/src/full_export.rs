//! The full decrypted export's seam (issue #350,
//! `docs/04-storage/export-format.md`).
//!
//! [`SunriseCore::export_all`](crate::SunriseCore::export_all) is what a
//! "File → Export Everything…" item calls. The writer is
//! `sunrise_client_core::export`'s, the same one `sunrise export all` runs, so
//! the app and the CLI write the same bytes; this module is the summary that
//! crosses back.
//!
//! # A path, unlike iCal
//!
//! An export is a directory of many files, possibly gigabytes of attachments,
//! so it cannot cross as one buffer the way an `.ics` does. The app passes the
//! folder its panel returned, holding that folder's security scope
//! (`startAccessingSecurityScopedResource`) for the length of the call: the
//! scope is the process's, so the writes made here are inside it.

use sunrise_core::export::{Manifest, MissingReason};

/// What an export wrote.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FullExportSummary {
    /// Files written, `manifest.json` included.
    pub files: u32,
    /// Attachments whose bytes are in the export.
    pub attachments_present: u32,
    /// Attachments listed as missing, for any reason.
    pub attachments_missing: u32,
    /// Of those, the ones a fetch was asked for and did not bring: the count
    /// worth a "try again online" line.
    pub attachments_fetch_failed: u32,
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl From<&Manifest> for FullExportSummary {
    fn from(m: &Manifest) -> Self {
        let with = |r: Option<MissingReason>| {
            count(
                m.attachments
                    .iter()
                    .filter(|a| a.missing_reason == r)
                    .count(),
            )
        };
        let present = with(None);
        Self {
            files: count(m.files.len() + 1),
            attachments_present: present,
            attachments_missing: count(m.attachments.len()).saturating_sub(present),
            attachments_fetch_failed: with(Some(MissingReason::FetchFailed)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_core::export::{AttachmentState, ManifestAttachment, ManifestFile};

    fn att(id: &str, missing_reason: Option<MissingReason>) -> ManifestAttachment {
        ManifestAttachment {
            id: id.into(),
            parent: "tsk_x".into(),
            filename: "a.txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: 1,
            state: if missing_reason.is_some() {
                AttachmentState::Missing
            } else {
                AttachmentState::Present
            },
            path: None,
            missing_reason,
        }
    }

    #[test]
    fn the_summary_counts_the_manifest_itself_and_each_reason() {
        let mut m = Manifest::new(0, "UTC");
        m.files.push(ManifestFile::of("entities/task.jsonl", b""));
        m.attachments = vec![
            att("att_a", None),
            att("att_b", Some(MissingReason::FetchFailed)),
            att("att_c", Some(MissingReason::NotFetched)),
        ];
        assert_eq!(
            FullExportSummary::from(&m),
            FullExportSummary {
                files: 2,
                attachments_present: 1,
                attachments_missing: 2,
                attachments_fetch_failed: 1,
            }
        );
    }
}
