//! Writing a full decrypted export to a directory (issue #350).
//!
//! The format is `docs/04-storage/export-format.md`, and the core decides
//! what goes in it (`sunrise_core::export`). This is the writer every client
//! calls — the CLI's `sunrise export all <dir>` and the `UniFFI` seam's
//! `export_all` — so the two produce the same bytes from the same vault:
//!
//! 1. refuse a destination that exists and is not an empty directory;
//! 2. with [`ExportOptions::fetch`], ask for every attachment whose bytes are
//!    not on this device and can be fetched, one at a time;
//! 3. read every live record in one consistent read;
//! 4. write `entities/<kind>.jsonl`, `notes/<id>.md` and `calendar.ics`;
//! 5. read and write each attachment that is here, one at a time, and list
//!    the rest as `missing` with the reason;
//! 6. write `manifest.json` last, so an export without one is visibly
//!    unfinished.
//!
//! The path is the caller's (`docs/01-architecture/shared-core.md` §The
//! client layer), and the instant and zone are the core's injected clock.

use std::path::{Path, PathBuf};

use sunrise_core::export::{
    entity_files, ExportFile, Manifest, ManifestAttachment, ManifestFile, MissingReason,
    CALENDAR_PATH, MANIFEST_PATH,
};
use sunrise_core::{AttachError, Core, CoreError};

/// What an export is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportOptions {
    /// Fetch attachment bytes that are on the relay but not on this device
    /// before writing. Off, they are listed as `missing` with
    /// [`MissingReason::NotFetched`].
    pub fetch: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self { fetch: true }
    }
}

/// Why an export stopped. Nothing is cleaned up: what was written stays, and
/// without its `manifest.json` it reads as unfinished.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// The destination exists and is not an empty directory.
    #[error("{0} already exists and is not an empty directory")]
    NotEmpty(PathBuf),
    /// A file or directory could not be written.
    #[error("{path}: {source}")]
    Io {
        /// What was being written.
        path: PathBuf,
        /// The OS error.
        source: std::io::Error,
    },
    /// The vault could not be read.
    #[error(transparent)]
    Core(#[from] CoreError),
    /// An attachment's bytes could not be read back, although they are here.
    #[error(transparent)]
    Attachment(#[from] AttachError),
    /// A record did not encode.
    #[error(transparent)]
    Format(#[from] sunrise_core::export::ExportError),
}

/// Write the export of `core` into `dir` and return its manifest.
///
/// # Errors
///
/// [`ExportError::NotEmpty`] before anything is written when `dir` is a file
/// or a non-empty directory; otherwise the first read or write that failed.
pub async fn export_all(
    core: &Core,
    dir: &Path,
    opts: ExportOptions,
) -> Result<Manifest, ExportError> {
    prepare(dir)?;
    let mut failed_fetch = std::collections::BTreeSet::new();
    if opts.fetch {
        for att in core.export_records()?.attachments {
            if !core.attachment_is_local(&att)? && att.is_fetchable() {
                // One failure is one missing file, not a failed export: the
                // rest of the vault is still worth having.
                if core.fetch_attachment(att.id).await.is_err() {
                    failed_fetch.insert(att.id);
                }
            }
        }
    }

    let records = core.export_records()?;
    let zone = core.reader_zone();
    let mut manifest = Manifest::new(core.now_ms(), zone.iana_name().unwrap_or("UTC"));

    let mut files = entity_files(&records)?;
    files.push(ExportFile {
        path: CALENDAR_PATH.to_owned(),
        bytes: calendar(&records.blocks).into_bytes(),
    });
    for f in &files {
        write(dir, &f.path, &f.bytes)?;
        manifest.files.push(ManifestFile::of(&f.path, &f.bytes));
    }

    for att in &records.attachments {
        let missing = if core.attachment_is_local(att)? {
            None
        } else if failed_fetch.contains(&att.id) {
            Some(MissingReason::FetchFailed)
        } else if !att.is_fetchable() {
            Some(MissingReason::NeverUploaded)
        } else {
            Some(MissingReason::NotFetched)
        };
        let entry = ManifestAttachment::new(att, missing);
        if let Some(path) = &entry.path {
            let bytes = core.attachment_bytes(att.id).await?;
            write(dir, path, &bytes)?;
            manifest.files.push(ManifestFile::of(path, &bytes));
        }
        manifest.attachments.push(entry);
    }

    write(dir, MANIFEST_PATH, &manifest.to_bytes())?;
    Ok(manifest)
}

/// Every Block as one iCalendar document, through the importer's own
/// mapping, so `ical import calendar.ics` lands on the Blocks it came from.
fn calendar(blocks: &[sunrise_core::queries::BlockRow]) -> String {
    let events: Vec<_> = blocks
        .iter()
        .map(|r| sunrise_integrations::ical_map::block_to_event(&r.block, r.title.as_deref()))
        .collect();
    sunrise_integrations::ical::write(&events)
}

/// Make `dir`, or accept it if it is already an empty directory.
fn prepare(dir: &Path) -> Result<(), ExportError> {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(ExportError::NotEmpty(dir.to_path_buf()));
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir).map_err(|source| ExportError::Io {
                path: dir.to_path_buf(),
                source,
            })
        }
        Err(_) if dir.exists() => Err(ExportError::NotEmpty(dir.to_path_buf())),
        Err(source) => Err(ExportError::Io {
            path: dir.to_path_buf(),
            source,
        }),
    }
}

/// Write `bytes` at the `/`-separated `rel` under `dir`, making its parents.
fn write(dir: &Path, rel: &str, bytes: &[u8]) -> Result<(), ExportError> {
    let path = rel.split('/').fold(dir.to_path_buf(), |p, c| p.join(c));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ExportError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(&path, bytes).map_err(|source| ExportError::Io { path, source })
}
