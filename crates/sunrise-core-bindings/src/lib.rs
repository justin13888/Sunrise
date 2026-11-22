//! The UniFFI seam over `sunrise-core`.
//!
//! One opaque handle ([`SunriseCore`]), one command type, one query type, one
//! result type, and a change stream. The generated Swift is what the macOS app
//! talks to; the same scaffolding generates Kotlin when Android arrives
//! (ADR-0019).
//!
//! # Shape
//!
//! * [`SunriseCore::open`] is an **async constructor**, so the tokio runtime
//!   UniFFI supplies is live for everything the handle later does.
//! * [`SunriseCore::submit`] and [`SunriseCore::query`] are async and return
//!   `Result`, which reaches Swift as `async throws`.
//! * The domain vocabulary crosses as [`crate::dto`] records and [`crate::types`]
//!   enums. `sunrise-domain` gains **no uniffi dependency**: the enums are
//!   declared with `#[uniffi::remote]`, and the structured types are mirrored.
//! * [`ChangeListener`] is a foreign-implemented trait — the way a
//!   `broadcast::Receiver` becomes something Swift can hold.
//!
//! # Three things that are load-bearing, not stylistic
//!
//! 1. **There is no process-global runtime and no `block_on`.** UniFFI runs
//!    exported async methods on its own tokio runtime; `block_on` inside that
//!    context deadlocks. The handle captures [`tokio::runtime::Handle`] in the
//!    async constructor and spawns through it.
//! 2. **`async_runtime = "tokio"` does not put *sync* exported methods on the
//!    runtime.** A sync method runs on the calling foreign thread with no
//!    reactor, so a bare `tokio::spawn` in one compiles and then panics —
//!    reaching Swift as a trapped `rustPanic`, i.e. a crash. Every spawn here
//!    goes through the captured handle.
//! 3. **The exported task record is [`dto::TaskItem`], not `Task`.** A Swift
//!    `Task` shadows `_Concurrency.Task` and breaks every `Task { }` in the
//!    app.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    // The split-optional edit records are wide by construction: each clearable
    // field costs one `bool`. See `dto::TaskEdit`.
    clippy::struct_excessive_bools
)]

use std::path::PathBuf;
use std::sync::Arc;

use sunrise_core::{Core, CoreConfig, Unlock};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_id::EntityRef;
use tokio::runtime::Handle;

mod account;
mod attachments;
mod changes;
mod error;
mod login;

pub mod background;
pub mod brief;
pub mod build_info;
pub mod calendar;
pub mod client;
pub mod command;
pub mod conflicts;
pub mod day_schedule;
pub mod dto;
pub mod editing;
pub mod flows;
pub mod full_export;
pub mod ical;
pub mod keyboard;
pub mod lateness;
pub mod links;
pub mod note_editing;
pub mod notes;
pub mod notifications;
pub mod pairing;
pub mod pairing_relay;
pub mod places;
pub mod planner;
pub mod print;
pub mod query;
pub mod recovery;
pub mod relay_url;
pub mod reminders;
pub mod saved_views;
pub mod search;
pub mod secret_copies;
pub mod settings;
pub mod shifts;
pub mod sync_status;
pub mod task_order;
pub mod types;
pub mod upcoming;
pub mod vault_repair;
pub mod vocab;

pub use background::{SyncCancel, SyncOnceOutcome};
pub use build_info::{core_build_info, CoreBuildInfo};
pub use changes::{ChangeEvent, ChangeListener, Subscription};
pub use client::{
    PrimaryView, SavedView, SavedViewFile, SavedViews, UndoRefusal, UndoState, UndoableOutcome,
};
pub use command::CoreCommand;
pub use dto::{CapturePreview, CommandOutcome, TaskItem};
pub use error::BindingError;
pub use ical::{IcalImportReport, IcalImportedBlock, IcalNotice};
pub use login::{LoginCredentials, SunriseLogin};
pub use notes::{
    decode_note_body, encode_note_body, note_body_limits, note_body_markdown, NoteBlock,
    NoteBodyLimits, NoteChecklistItem, NoteDocument, NoteFidelity, NoteHeadingLevel, NoteInline,
    NoteListItem, NoteMark,
};
pub use pairing::{DevicePairing, PairingRole, PairingStep};
pub use pairing_relay::RelayPairing;
pub use query::{CoreQuery, CoreQueryResult};
pub use recovery::{
    check_recovery_code, is_recovery_word, RecoveryListener, RecoveryStep, ReplayListener,
    ReplayProgress,
};
pub use vocab::RelativeDay;

uniffi::setup_scaffolding!();

/// A live vault.
///
/// Opaque on the foreign side: it is an `Arc`-held object with methods, not a
/// record. Everything that touches storage goes through here.
#[derive(uniffi::Object)]
pub struct SunriseCore {
    inner: Arc<Core>,
    /// The tokio runtime UniFFI is driving this object's async methods on,
    /// captured in the async constructor.
    ///
    /// Load-bearing: [`SunriseCore::subscribe_changes`] and
    /// [`SunriseCore::start_sync`] are **sync** exported methods, and a sync
    /// exported method runs on the calling foreign thread with no reactor
    /// installed. A bare `tokio::spawn` there panics at runtime. Spawning
    /// through a captured handle works from any thread.
    rt: Handle,
    /// The undo and redo stacks for this session.
    ///
    /// Session-scoped and deliberately not persisted: an inverse command is
    /// only meaningful against the vault state it was read from, and one
    /// restored from disk a week later would restore a value nothing on screen
    /// has shown since. It is also **not** synced — undo is this device's
    /// record of what *it* did.
    undo: sunrise_client_core::history::UndoHistory,
    /// The kick and dial count every sync transport this vault dials shares,
    /// so [`SunriseCore::sync_once`] can tell a session dialled for it from one
    /// that was up before the app was suspended. See [`background`].
    sync_link: background::SyncLink,
}

#[uniffi::export(async_runtime = "tokio")]
impl SunriseCore {
    /// Open (or create) the vault at `vault_dir`, keyed by a 32-byte root.
    ///
    /// The root comes from the platform keychain or a completed pairing; this
    /// seam does not derive it.
    ///
    /// `paired_bundle` is what
    /// [`DevicePairing::open_pairing_grant`](pairing::DevicePairing::open_pairing_grant)
    /// assembled out of a completed three-message exchange, and is passed
    /// **once**, on the first open of a device that was just added. It carries
    /// the account's public identity, the Stream keys — which since ADR-0024
    /// the vault root no longer implies, so a device handed only a root would
    /// open an empty-looking vault full of ciphertext it could never read —
    /// and this device's own keys with the `DeviceCert` its sponsor signed over
    /// them.
    ///
    /// It defaults to `None`, so every existing Swift call site keeps
    /// compiling and keeps meaning what it meant.
    ///
    /// # Errors
    ///
    /// For a vault that will not open, the `Vault…` variant naming why:
    /// [`BindingError::VaultDamaged`] and the five after it, for which
    /// [`vault_repair::open_remedies`] says what to offer, and
    /// [`BindingError::Core`] for any other reason.
    /// [`BindingError::BadVaultRoot`] and [`BindingError::Pairing`] for what
    /// was handed in.
    // `default(paired_bundle = None)` is what keeps every existing Swift call
    // site compiling and meaning what it meant: the generated signature gains
    // an argument the foreign side may omit, rather than a fourth required one.
    #[uniffi::constructor(default(paired_bundle = None))]
    pub async fn open(
        vault_dir: String,
        vault_root: Vec<u8>,
        app_version: String,
        paired_bundle: Option<Vec<u8>>,
    ) -> Result<Arc<Self>, BindingError> {
        let root = vault_root_bytes(&vault_root)?;
        let paired = match paired_bundle {
            Some(bytes) => Some(Box::new(
                sunrise_pairing::decode_pairing_payload(&bytes)
                    .map_err(|e| BindingError::Pairing(e.to_string()))?,
            )),
            None => None,
        };
        let cfg = CoreConfig::production(PathBuf::from(vault_dir), app_version);
        let core = Core::open(
            cfg,
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes(root),
                paired,
            },
        )
        .await
        .map_err(vault_repair::open_failed)?;
        Ok(Self::wrap(core))
    }

    /// Submit one mutating command.
    ///
    /// # Errors
    ///
    /// [`BindingError::FeatureMissing`] when the vault requires a feature this
    /// build lacks and the command would edit what it covers.
    /// [`BindingError::BadFixedBytes`] for an `AttachFile` whose key or digest
    /// is the wrong width or not hex. [`BindingError::Core`] when the command
    /// does not validate, the vault refuses the write, or a `ResolveConflict`
    /// value is not canonical CBOR.
    pub async fn submit(&self, cmd: CoreCommand) -> Result<CommandOutcome, BindingError> {
        let res = self.inner.submit(cmd.into_core()?).await?;
        Ok(CommandOutcome::from(&res))
    }

    /// Run one read query.
    ///
    /// [`CoreQuery::StreamList`] answers with one row the core's list does not
    /// have: a synthetic Inbox first, under the legacy Inbox id, whose
    /// `open_task_count` counts the Inbox view. The core has no such row because
    /// the Inbox is a view rather than a stream (ADR-0046 §3).
    pub async fn query(&self, q: CoreQuery) -> Result<CoreQueryResult, BindingError> {
        let stream_list = matches!(q, CoreQuery::StreamList);
        let res = self.inner.query(q.into_core()).await?;
        let mut out = CoreQueryResult::from_core(res);
        // The Inbox is a view, not a stream (ADR-0046 §3), so the core's
        // stream list has no row for it. A client that predates that reads
        // its sidebar's Inbox off the first row, so the seam puts a synthetic
        // one back, under the legacy Inbox id, counting the view.
        if let (true, CoreQueryResult::Streams { streams }) = (stream_list, &mut out) {
            let open = match self.inner.query(sunrise_core::Query::Inbox).await? {
                sunrise_core::QueryResult::StreamTasks(t) | sunrise_core::QueryResult::Tasks(t) => {
                    t.len()
                }
                _ => 0,
            };
            streams.insert(
                0,
                crate::dto::StreamListRow::inbox(u64::try_from(open).unwrap_or(u64::MAX)),
            );
        }
        Ok(out)
    }

    /// Submit one command **and record how to reverse it**.
    ///
    /// The rows the inverse is built from are read from the vault *before* the
    /// command lands, because once it has landed the old values are gone.
    ///
    /// `label` is what a menu item says ("complete “Renew passport”"). The
    /// seam does not compose it: only the caller knows which of several rows
    /// on screen the user acted on.
    ///
    /// A command with no inverse is still **submitted** — refusing the write
    /// because it cannot be undone would be a worse client — and the returned
    /// [`client::UndoableOutcome`] carries why nothing went on the stack. Say
    /// that out loud rather than offering an undo that silently does nothing:
    /// detaching an attachment, resolving a conflict, and any other command
    /// that answers [`client::UndoRefusal::Unsupported`] stay done, and that
    /// fact belongs in front of the user *before* they rely on undo. A delete
    /// is not among them: its undo is a restore (ADR-0065 §3).
    ///
    /// # Errors
    ///
    /// As [`SunriseCore::submit`]; nothing goes on the stack then.
    pub async fn submit_undoable(
        &self,
        cmd: CoreCommand,
        label: String,
    ) -> Result<client::UndoableOutcome, BindingError> {
        let (res, not_undoable) = self
            .undo
            .submit(&self.inner, cmd.into_core()?, label)
            .await?;
        Ok(client::UndoableOutcome {
            outcome: CommandOutcome::from(&res),
            not_undoable: not_undoable.map(client::UndoRefusal::from),
        })
    }

    /// Reverse the most recent recorded step. Returns its label, or `None`
    /// when there is nothing to undo.
    ///
    /// This is a **new write**, not a rollback: it converges like any other
    /// op, and a device undoing what another has since changed loses the LWW
    /// tie exactly as a manual edit would.
    ///
    /// # Errors
    ///
    /// [`BindingError::FeatureMissing`] or [`BindingError::Core`], as
    /// [`SunriseCore::submit`] raises them for the reversing write. The step
    /// does not go back on either stack: it may have half applied, and
    /// replaying it is not known to be safe.
    pub async fn undo(&self) -> Result<Option<String>, BindingError> {
        Ok(self.undo.undo(&self.inner).await?)
    }

    /// Replay the most recently undone step. Returns its label, or `None`.
    ///
    /// # Errors
    ///
    /// As [`SunriseCore::undo`], for the replayed write.
    pub async fn redo(&self) -> Result<Option<String>, BindingError> {
        Ok(self.undo.redo(&self.inner).await?)
    }

    /// What can be undone or redone right now, for the menu.
    ///
    /// Sync: it reads two labels off a mutex and touches nothing else.
    #[must_use]
    pub fn undo_state(&self) -> client::UndoState {
        self.undo.labels().into()
    }

    /// Parse one capture line (`#stream @context ^when !priority ~duration`)
    /// and commit it, in the caller's timezone.
    ///
    /// `tz` is an IANA zone name; an unknown one falls back to UTC rather than
    /// refusing the capture, because losing what someone typed is the worse
    /// failure.
    ///
    /// # Errors
    ///
    /// [`BindingError::FeatureMissing`] when the vault requires a feature this
    /// build lacks and creating a task would edit what it covers.
    /// [`BindingError::Core`] when the vault cannot be read to resolve names,
    /// the parsed task does not validate, or the vault refuses the write.
    pub async fn capture(&self, text: String, tz: String) -> Result<CommandOutcome, BindingError> {
        let zone = sunrise_client_core::vocab::zone_or_utc(&tz);
        let parsed = self.inner.capture(&text, &zone).await?;
        let res = self
            .inner
            .submit(sunrise_core::Command::CreateTask(parsed.draft))
            .await?;
        Ok(CommandOutcome::from(&res))
    }

    /// Parse one capture line **without writing anything**, so a capture
    /// field can show what it would create as the user types.
    ///
    /// Resolving `#stream` and `@context` needs the vault, which is why this
    /// is a method and not a [`crate::vocab`] function. The returned
    /// `draft` is the same value [`SunriseCore::capture`] would submit: hand
    /// it straight back as [`command::CoreCommand::CreateTask`] and the
    /// preview cannot disagree with the commit.
    ///
    /// Never fails on bad input — an unparseable `^when` is reported in
    /// `issues` with its text left in the title. Validation stays the
    /// committing command's job, so a half-typed line still previews.
    ///
    /// Not free: resolving names costs two vault reads per call, so a caller
    /// wiring this to a text field should debounce rather than fire on every
    /// keystroke.
    pub async fn preview_capture(
        &self,
        text: String,
        tz: String,
    ) -> Result<CapturePreview, BindingError> {
        let zone = sunrise_client_core::vocab::zone_or_utc(&tz);
        Ok(CapturePreview::from(
            &self.inner.capture(&text, &zone).await?,
        ))
    }

    /// Import an `.ics` document's `VEVENT`s as time blocks.
    ///
    /// **Idempotent.** Each event is written to the Block that
    /// `(source, UID)` derives, so importing the same file twice updates the
    /// same Blocks rather than making a second calendar — and two paired
    /// devices importing it independently converge on one set. A client may
    /// therefore offer "import" without first asking the user whether they
    /// have already done it.
    ///
    /// `stream_id` is where the Blocks land; `None`, or the legacy Inbox id,
    /// imports into no stream.
    /// `source` distinguishes two calendars that happen to share a `UID` and
    /// should stay separate; `None` means the shared one-shot `.ics` source,
    /// which is what a file picker wants.
    ///
    /// The document arrives as text rather than a path: on macOS the file the
    /// user picked carries a security scope only the app holds, so the app
    /// reads it and hands over what it read — the same argument as
    /// [`SunriseCore::attach_file`].
    ///
    /// The returned report's `notices` are **not** telemetry. They name every
    /// piece of the file a Block cannot hold, and a client that discards them
    /// is dropping the user's data without telling them.
    ///
    /// # Errors
    ///
    /// [`BindingError::Calendar`] when the text is not an iCalendar document
    /// at all, or when the vault refuses a write. A single unusable event is
    /// reported in the result instead, because refusing 200 good events over
    /// one bad one would make the feature useless.
    ///
    /// [`BindingError::BadId`] when `stream_id` names something other than a
    /// stream; nothing is imported then.
    pub async fn import_ical(
        &self,
        text: String,
        stream_id: Option<EntityRef>,
        source: Option<String>,
    ) -> Result<ical::IcalImportReport, BindingError> {
        let stream = match stream_id {
            Some(id) if id.kind() != sunrise_id::EntityKind::Stream => {
                return Err(BindingError::BadId {
                    id: id.to_str(),
                    cause: "a calendar imports into a stream".into(),
                })
            }
            // None, or the legacy Inbox id, imports into no stream.
            other => sunrise_domain::home_stream(other),
        };
        let source = source
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(sunrise_integrations::ical_vault::ICS_SOURCE);
        let report =
            sunrise_integrations::ical_vault::import(&self.inner, &text, stream, source).await?;
        Ok(ical::IcalImportReport::from(&report))
    }

    /// Render one window of the calendar as an `.ics` document.
    ///
    /// Returns the text; writing it is the app's, for the same reason import
    /// takes text. `at_ms` is any instant inside the window to export — pass
    /// [`SunriseCore::now_ms`] for "today" or "this week", or the instant the
    /// calendar view is scrolled to.
    ///
    /// Every `UID` written is the Block's own id, so a document exported here
    /// and imported back lands on the Blocks it came from instead of doubling
    /// them.
    ///
    /// # Errors
    ///
    /// [`BindingError::Calendar`] when the vault cannot be read.
    pub async fn export_ical(
        &self,
        window: sunrise_integrations::ical_vault::ExportWindow,
        at_ms: u64,
    ) -> Result<String, BindingError> {
        Ok(sunrise_integrations::ical_vault::export(&self.inner, window, at_ms).await?)
    }

    /// Write everything, decrypted, into `dir`, in the open format of
    /// `docs/04-storage/export-format.md`: every record as JSON lines, notes
    /// as Markdown, every Block as one `.ics`, the attachment bytes, and a
    /// `manifest.json` of versions and BLAKE3 digests, written last.
    ///
    /// `dir` must not exist yet or be an empty directory. With `fetch`,
    /// attachment bytes that are on the relay and not here are downloaded
    /// first; whatever is still not here is listed in the manifest as
    /// missing, with the reason. The same writer as `sunrise export all`.
    ///
    /// # Errors
    ///
    /// [`BindingError::Core`] for a destination that is not empty, a file
    /// that could not be written, or a vault that could not be read.
    pub async fn export_all(
        &self,
        dir: String,
        fetch: bool,
    ) -> Result<full_export::FullExportSummary, BindingError> {
        let manifest = sunrise_client_core::export::export_all(
            &self.inner,
            std::path::Path::new(&dir),
            sunrise_client_core::export::ExportOptions { fetch },
        )
        .await
        .map_err(|e| BindingError::Core(e.to_string()))?;
        Ok(full_export::FullExportSummary::from(&manifest))
    }

    /// Rebuild the vault's projection from its local op log: every entity
    /// row, the task search index and the merge state are cleared and
    /// folded again from the ops, in one transaction (issue #441). Nothing
    /// is reported on `subscribe_changes`; re-read what is on screen.
    ///
    /// The vault is copied first, to `<vault>.pre-rebuild.bak`, which the
    /// next open deletes; [`vault_repair::vault_backups`] lists it until then.
    ///
    /// # Errors
    ///
    /// [`BindingError::Core`] if the copy cannot be written, an op in the log
    /// cannot be read or the vault cannot be written. The projection is then
    /// left as it was.
    pub fn rebuild_projection(&self) -> Result<dto::RebuildReportItem, BindingError> {
        Ok(self.inner.rebuild_projection()?.into())
    }

    /// Bring search's word index into line with the vault's content language
    /// (`search.content_language`), a batch at a time with the vault free
    /// between batches; search answers from the old index until the new one
    /// is swapped in. Cheap when nothing needs doing, so call it in the
    /// background after open and after the language changes.
    ///
    /// # Errors
    ///
    /// [`BindingError::Core`] when the vault is closed or cannot be written;
    /// the old index stays, and the next call resumes.
    pub async fn reindex_search(&self) -> Result<search::SearchReindexItem, BindingError> {
        Ok(self.inner.reindex_search().await?.into())
    }

    /// Whether this vault can sponsor a pairing at all.
    ///
    /// False on a vault that was itself added by pairing: since #105 it holds
    /// `ID_S_pub` and no signing key, so it cannot issue the `DeviceCert` a
    /// joiner needs. Read this before offering an "add a device" affordance
    /// rather than letting the user walk a six-leg handshake to a grant that
    /// cannot be produced.
    #[must_use]
    pub fn can_sponsor_pairing(&self) -> bool {
        self.inner.can_sponsor_pairing()
    }

    /// Seal message 1 — this account's identity — into a confirmed pairing, on
    /// the device that holds the vault.
    ///
    /// Nothing secret crosses the seam in the clear on this leg or any other.
    /// A `vault_root()` getter would be the obvious shape and the wrong one: it
    /// would put the key that decrypts everything into a Swift `Data`, where it
    /// outlives the call, lands in whatever the app logs, and is one
    /// autocomplete away from a file. So the app only ever gets *ciphertext*,
    /// and the vault root does not appear even as ciphertext until
    /// [`Self::send_pairing_grant`], which runs on a request this device
    /// accepted.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] when the SAS has not been confirmed on this
    /// device, or when this device is the one being added.
    pub fn send_pairing_offer(
        &self,
        pairing: Arc<pairing::DevicePairing>,
    ) -> Result<String, BindingError> {
        Ok(sunrise_client_core::pairing::seal_offer(
            &self.inner,
            pairing.inner(),
        )?)
    }

    /// Issue the joiner's `DeviceCert` and seal message 3, on the sponsor.
    ///
    /// `sealed_request` is what the joiner's `request_device_cert` produced.
    /// The request is opened by the pairing object, handed to the vault — which
    /// is the only thing holding `ID_S_priv` — and the grant it produces goes
    /// straight back out through the same channel. The cert is signed inside
    /// the core and the signing key never reaches this seam, let alone Swift.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] when the SAS has not been confirmed, when this
    /// device is the one being added, when the request does not open or decode,
    /// or when the grant is too large for one Noise transport message.
    /// [`BindingError::Core`] when this vault holds no `ID_S_priv` — see
    /// [`Self::can_sponsor_pairing`] — or when the request answers a different
    /// account's offer.
    pub fn send_pairing_grant(
        &self,
        pairing: Arc<pairing::DevicePairing>,
        sealed_request: String,
    ) -> Result<String, BindingError> {
        // The request is opened by the pairing object, answered by the vault
        // and the grant sealed straight back out, all in the client layer's
        // `seal_grant`, which wipes the grant's encoding once sealed.
        Ok(sunrise_client_core::pairing::seal_grant(
            &self.inner,
            pairing.inner(),
            &sealed_request,
        )?)
    }

    /// Stop the sync driver and release the vault lock. Idempotent.
    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }

    /// The core's clock, in epoch milliseconds. Every `now_ms` a query wants
    /// should come from here so one reading drives the whole screen.
    pub fn now_ms(&self) -> u64 {
        self.inner.now_ms()
    }

    /// Report the zone the OS says this device is in: on the OS's
    /// time-zone-change notification and on every foreground
    /// (`docs/10-cross-cutting/time.md` §7). See
    /// [`sunrise_core::Core::on_time_zone_changed`].
    ///
    /// # Errors
    ///
    /// [`BindingError::Core`] for a zone name the tzdb does not know.
    pub fn on_time_zone_changed(&self, zone: String) -> Result<TimeZoneChange, BindingError> {
        let c = self.inner.on_time_zone_changed(&zone)?;
        Ok(TimeZoneChange {
            previous: c.previous,
            zone: c.zone,
            changed: c.changed,
            affected_tasks: c.affected_tasks,
            notify: c.notify,
            hard_violations: c.hard_violations,
        })
    }

    /// Start the live-sync driver against `url` (a relay `/sync` endpoint:
    /// Server-Sent Events downstream, typed `POST` upstream, per ADR-0023),
    /// presenting `bearer` on every request.
    ///
    /// `bearer` is empty only for a self-host relay: every other deployment
    /// refuses an unauthenticated upgrade with `401`. Renewing is
    /// [`SunriseCore::set_sync_credential`] — the driver picks the new token up
    /// on its next connect, and the caller does not restart sync.
    ///
    /// `relay_device_id` is the ADR-0022 device binding: the ULID the relay
    /// minted at registration, which every request then names in
    /// `X-Sunrise-Device` and signs for. It is **not** [`Self::device_id`] —
    /// that is the vault's own 16-byte id, and handing it over here produces a
    /// signature that verifies against nothing, which the relay reports as a
    /// bad *bearer* so a caller cannot enumerate an account's devices. `None`
    /// starts an unbound driver, which a self-host relay accepts and a relay
    /// with `require_device_sig` refuses; the caller keeps the id, because only
    /// the caller knows where a platform stores one.
    ///
    /// The signer is built once and cloned per connection attempt, for the same
    /// reason the bearer is read per attempt: a reconnect is a new transport
    /// and has to carry the binding too.
    ///
    /// Deliberately sync: it only spawns. The spawn goes through the tokio
    /// runtime handle captured in the async constructor, not `tokio::spawn` —
    /// a sync exported method runs on the calling foreign thread with no
    /// reactor installed, and a bare `tokio::spawn` there panics at runtime.
    ///
    /// # Errors
    ///
    /// None today: the driver's connection failures happen after this has
    /// returned. A second call while a driver runs does not start another or
    /// restart the running one, so its `url` and `relay_device_id` are
    /// ignored; its `bearer` still replaces the stored credential, exactly as
    /// [`SunriseCore::set_sync_credential`] would, and the running driver
    /// presents it on its next connect.
    pub fn start_sync(
        &self,
        url: String,
        bearer: Option<String>,
        relay_device_id: Option<String>,
    ) -> Result<(), BindingError> {
        let _guard = self.rt.enter();
        self.inner.sync_credential().set(bearer);
        // An empty string is not an id. A foreign caller reading a missing
        // value out of a store that answers with `""` would otherwise start a
        // driver that presents an `X-Sunrise-Device` naming no row, and the
        // relay's answer to that is indistinguishable from a bad bearer.
        let signer = relay_device_id
            .filter(|id| !id.trim().is_empty())
            .map(|id| self.inner.device_signer(id.trim()));
        self.inner
            .start_sync(sse_factory(&url, signer, self.sync_link.clone()))?;
        Ok(())
    }

    /// Replace the bearer the sync driver presents on its next connect.
    ///
    /// Separate from [`SunriseCore::start_sync`] because a session outlives its
    /// tokens: the app renews against the issuer on its own schedule and hands
    /// the result over here, without tearing the driver down.
    pub fn set_sync_credential(&self, bearer: Option<String>) {
        self.inner.sync_credential().set(bearer);
    }

    /// Start the periodic routine-materialization timer.
    ///
    /// # Errors
    ///
    /// None today: a second call while the timer runs is a no-op.
    pub fn start_routine_timer(&self, interval_ms: u64) -> Result<(), BindingError> {
        let _guard = self.rt.enter();
        self.inner
            .start_routine_timer(std::time::Duration::from_millis(interval_ms))?;
        Ok(())
    }
}

impl SunriseCore {
    /// The open vault, for the client-layer flows that take one.
    pub(crate) fn core(&self) -> &Core {
        &self.inner
    }

    /// Hold an opened core.
    ///
    /// Only ever called from an async constructor, so a runtime is
    /// definitely installed and [`Handle::current`] cannot panic. That is the
    /// only place it is guaranteed.
    fn wrap(core: Core) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(core),
            rt: Handle::current(),
            undo: sunrise_client_core::history::UndoHistory::new(),
            sync_link: background::SyncLink::new(),
        })
    }
}

/// A vault root as it crossed the seam, checked for its width.
fn vault_root_bytes(vault_root: &[u8]) -> Result<[u8; 32], BindingError> {
    sunrise_client_core::host::vault_root(vault_root).map_err(|e| BindingError::BadVaultRoot {
        len: u32::try_from(e.len).unwrap_or(u32::MAX),
    })
}

/// The transport factory the sync driver dials with: the shared SSE factory,
/// every transport going out through `link` so [`SunriseCore::sync_once`] can
/// count and end it — see [`background`].
fn sse_factory(
    url: &str,
    signer: Option<Arc<dyn sunrise_sync::DeviceSigner>>,
    link: background::SyncLink,
) -> sunrise_core::TransportFactory {
    sunrise_client_core::sync::sse_factory(url, signer, Some(link))
}

/// What reporting a zone did. See [`sunrise_core::TimeZoneChange`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TimeZoneChange {
    /// The reader's zone before the report.
    pub previous: String,
    /// The reader's zone now.
    pub zone: String,
    /// Whether the zone changed.
    pub changed: bool,
    /// Open tasks whose resolved time or lateness moved.
    pub affected_tasks: u32,
    /// Whether to post the one local notification naming the new zone; the
    /// `notifications.timezone_changed.enabled` preference, off by default.
    pub notify: bool,
    /// Open planned tasks whose plan breaks a hard scheduling constraint in
    /// the new zone (#333). Flag them; the core never moves them.
    #[uniffi(default = [])]
    pub hard_violations: Vec<EntityRef>,
}

impl std::fmt::Debug for SunriseCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SunriseCore").finish_non_exhaustive()
    }
}
