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
use std::sync::{Arc, Mutex};

use sunrise_core::{Command, Core, CoreConfig, CoreError, Unlock};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_id::EntityRef;
use thiserror::Error;
use tokio::runtime::Handle;
use tokio::sync::broadcast::error::RecvError;

pub mod client;
pub mod command;
pub mod dto;
pub mod ical;
pub mod pairing;
pub mod query;
pub mod types;
pub mod vocab;

pub use client::{
    PrimaryView, SavedView, SavedViewFile, SavedViews, UndoRefusal, UndoState, UndoableOutcome,
};
pub use command::CoreCommand;
pub use dto::{CapturePreview, CommandOutcome, TaskItem};
pub use ical::{IcalImportReport, IcalImportedBlock, IcalNotice};
pub use pairing::{DevicePairing, PairingRole, PairingStep};
pub use query::{CoreQuery, CoreQueryResult};
pub use vocab::RelativeDay;

uniffi::setup_scaffolding!();

/// Everything that can go wrong across the seam.
///
/// `#[uniffi(flat_error)]` because [`sunrise_core::CoreError`] is a tree of
/// `#[error(transparent)]` wrappers around six other crates' error types, and
/// UniFFI cannot model that structurally. Flattening keeps the *variant* —
/// which is what a client branches on — and carries the detail as the message,
/// which is what a client shows.
#[derive(Debug, Error, uniffi::Error)]
#[uniffi(flat_error)]
#[non_exhaustive]
pub enum BindingError {
    /// The vault could not be opened, read or written.
    #[error("core: {0}")]
    Core(String),
    /// An id string was not a valid `EntityRef`.
    #[error("not an id: {id} ({cause})")]
    BadId {
        /// What was passed.
        id: String,
        /// Why it was rejected.
        cause: String,
    },
    /// A timestamp, date or time was not readable.
    #[error("not a time: {value} ({cause})")]
    BadTime {
        /// What was passed.
        value: String,
        /// Why it was rejected.
        cause: String,
    },
    /// A vault root key was not 32 bytes.
    #[error("vault root must be 32 bytes, got {len}")]
    BadVaultRoot {
        /// How many bytes arrived.
        len: u32,
    },
    /// The OIDC login flow failed.
    #[error("login: {0}")]
    Login(String),
    /// A recurrence phrase could not be read.
    ///
    /// Carries the phrase back so a routine editor can leave what was typed in
    /// the field. Never guessed at: a routine that silently fires on the wrong
    /// cadence is discovered weeks later, by which point it has generated the
    /// wrong tasks.
    #[error("not a recurrence: {text} ({cause})")]
    BadRecurrence {
        /// What was typed.
        text: String,
        /// Why it was rejected, in the domain's own words.
        cause: String,
    },
    /// A fixed-width field arrived at the wrong length, or was not hex.
    ///
    /// UniFFI cannot express `[u8; 32]`, so a key crosses as a `Vec<u8>` and a
    /// digest as a hex string. Their width is a contract the seam has to check
    /// somewhere, and here is the last place it can be checked cheaply.
    #[error("{field} must be {expected} bytes")]
    BadFixedBytes {
        /// Which field.
        field: String,
        /// How many bytes it must carry.
        expected: u32,
    },
    /// An attachment's metadata is on this device and its bytes are not.
    ///
    /// Its own variant rather than a `Core` message because it is the one
    /// attachment failure that is **not** an error in the usual sense: the
    /// vault is fine and the row is real. A client shows "not downloaded", not
    /// "something went wrong", and it can only tell the two apart if the seam
    /// keeps them apart.
    #[error("attachment {id} has no bytes on this device")]
    AttachmentNotHere {
        /// The attachment.
        id: String,
    },
    /// The attachment byte path failed for any other reason.
    #[error("attachment: {0}")]
    Attachment(String),
    /// An `.ics` import or export failed.
    ///
    /// Its own variant rather than a `Core` message: a malformed calendar file
    /// is the user's file being wrong, not the vault being wrong, and a client
    /// says something quite different about the two.
    #[error("calendar: {0}")]
    Calendar(String),
    /// A pairing step failed, was taken out of order, or was refused.
    ///
    /// Includes the SAS rejection, which is an error rather than a value on
    /// purpose: "the codes did not match" is the one pairing outcome a UI must
    /// never let fall through to the next screen.
    #[error("pairing: {0}")]
    Pairing(String),
}

impl From<sunrise_integrations::IntegrationError> for BindingError {
    fn from(e: sunrise_integrations::IntegrationError) -> Self {
        Self::Calendar(e.to_string())
    }
}

impl From<sunrise_pairing::PairingError> for BindingError {
    fn from(e: sunrise_pairing::PairingError) -> Self {
        Self::Pairing(e.to_string())
    }
}

impl From<sunrise_core::AttachError> for BindingError {
    fn from(e: sunrise_core::AttachError) -> Self {
        match e {
            sunrise_core::AttachError::BytesNotHere { id } => {
                Self::AttachmentNotHere { id: id.to_str() }
            }
            other => Self::Attachment(other.to_string()),
        }
    }
}

impl From<sunrise_auth::LoginError> for BindingError {
    fn from(e: sunrise_auth::LoginError) -> Self {
        Self::Login(e.to_string())
    }
}

impl From<CoreError> for BindingError {
    fn from(e: CoreError) -> Self {
        Self::Core(e.to_string())
    }
}

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
    undo_stacks: Mutex<client::UndoStacks>,
}

#[uniffi::export(async_runtime = "tokio")]
impl SunriseCore {
    /// Open (or create) the vault at `vault_dir`, keyed by a 32-byte root.
    ///
    /// The root comes from the platform keychain or a completed pairing; this
    /// seam does not derive it.
    #[uniffi::constructor]
    pub async fn open(
        vault_dir: String,
        vault_root: Vec<u8>,
        app_version: String,
    ) -> Result<Arc<Self>, BindingError> {
        let root: [u8; 32] =
            vault_root
                .as_slice()
                .try_into()
                .map_err(|_| BindingError::BadVaultRoot {
                    len: u32::try_from(vault_root.len()).unwrap_or(u32::MAX),
                })?;
        let cfg = CoreConfig::production(PathBuf::from(vault_dir), app_version);
        let core = Core::open(cfg, Unlock::DevicePaired(VaultRootKey::from_bytes(root))).await?;
        Ok(Arc::new(Self {
            inner: Arc::new(core),
            // Inside an async exported method, so a runtime is definitely
            // installed. This is the only place that is guaranteed.
            rt: Handle::current(),
            undo_stacks: Mutex::new(client::UndoStacks::default()),
        }))
    }

    /// Submit one mutating command.
    pub async fn submit(&self, cmd: CoreCommand) -> Result<CommandOutcome, BindingError> {
        let res = self.inner.submit(cmd.into_core()?).await?;
        Ok(CommandOutcome::from(&res))
    }

    /// Run one read query.
    pub async fn query(&self, q: CoreQuery) -> Result<CoreQueryResult, BindingError> {
        let res = self.inner.query(q.into_core()).await?;
        Ok(CoreQueryResult::from_core(res))
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
    /// a delete writes a tombstone the core cannot restore, and that fact
    /// belongs in front of the user *before* they rely on undo.
    pub async fn submit_undoable(
        &self,
        cmd: CoreCommand,
        label: String,
    ) -> Result<client::UndoableOutcome, BindingError> {
        let lowered = cmd.into_core()?;
        // A create is recorded *after* the write and everything else *before*
        // it, and neither order is a preference: an edit's inverse reads the
        // values the write is about to overwrite, and a create's inverse names
        // the entity the write is about to mint.
        let (res, recorded) = if sunrise_client_core::undo::is_create(&lowered) {
            let res = self.inner.submit(lowered.clone()).await?;
            let recorded = client::record_create(label, lowered, res.entity);
            (res, recorded)
        } else {
            // Read before write. The order is the whole correctness argument.
            let recorded = client::record(&self.inner, label, vec![lowered.clone()]).await;
            (self.inner.submit(lowered).await?, recorded)
        };
        let not_undoable = match recorded {
            Ok(entry) => {
                if let Ok(mut g) = self.undo_stacks.lock() {
                    g.push(entry);
                }
                None
            }
            Err(refusal) => Some(refusal),
        };
        Ok(client::UndoableOutcome {
            outcome: CommandOutcome::from(&res),
            not_undoable,
        })
    }

    /// Reverse the most recent recorded step. Returns its label, or `None`
    /// when there is nothing to undo.
    ///
    /// This is a **new write**, not a rollback: it converges like any other
    /// op, and a device undoing what another has since changed loses the LWW
    /// tie exactly as a manual edit would.
    pub async fn undo(&self) -> Result<Option<String>, BindingError> {
        let Some(mut entry) = self.undo_stacks.lock().ok().and_then(|mut g| g.take_undo()) else {
            return Ok(None);
        };
        let minted = self.apply(&entry.backward).await?;
        // If the step being applied contains a create — which is what the redo
        // of an undone create is — the way back out has to name the entity
        // this replay minted, not the one the original create did.
        sunrise_client_core::undo::rebind_creates(&entry.backward, &mut entry.forward, &minted);
        let label = entry.label.clone();
        if let Ok(mut g) = self.undo_stacks.lock() {
            g.push_undone(entry);
        }
        Ok(Some(label))
    }

    /// Replay the most recently undone step. Returns its label, or `None`.
    pub async fn redo(&self) -> Result<Option<String>, BindingError> {
        let Some(mut entry) = self.undo_stacks.lock().ok().and_then(|mut g| g.take_redo()) else {
            return Ok(None);
        };
        let minted = self.apply(&entry.backward).await?;
        // Replaying a create makes a *different* entity. Bind the undo that
        // goes back on the stack to that one, or the next undo would delete an
        // already-tombstoned id and leave this entity live for ever.
        sunrise_client_core::undo::rebind_creates(&entry.backward, &mut entry.forward, &minted);
        let label = entry.label.clone();
        if let Ok(mut g) = self.undo_stacks.lock() {
            g.push_redone(entry);
        }
        Ok(Some(label))
    }

    /// What can be undone or redone right now, for the menu.
    ///
    /// Sync: it reads two labels off a mutex and touches nothing else.
    #[must_use]
    pub fn undo_state(&self) -> client::UndoState {
        self.undo_stacks
            .lock()
            .map(|g| g.state())
            .unwrap_or_default()
    }

    /// Parse one capture line (`#stream @context ^when !priority ~duration`)
    /// and commit it, in the caller's timezone.
    ///
    /// `tz` is an IANA zone name; an unknown one falls back to UTC rather than
    /// refusing the capture, because losing what someone typed is the worse
    /// failure.
    pub async fn capture(&self, text: String, tz: String) -> Result<CommandOutcome, BindingError> {
        let zone = jiff::tz::TimeZone::get(&tz).unwrap_or(jiff::tz::TimeZone::UTC);
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
        let zone = jiff::tz::TimeZone::get(&tz).unwrap_or(jiff::tz::TimeZone::UTC);
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
    /// `stream_id` is where the Blocks land; `None` means the Inbox stream.
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
            Some(id) => id,
            None => sunrise_domain::inbox_stream_ref(),
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

    /// Attach a file to a task: seal the bytes, store them, record the
    /// metadata, and return the row that describes them.
    ///
    /// The bytes cross the seam whole rather than as a path. The core would
    /// otherwise have to open a file the app chose, and on macOS an
    /// app-chosen file arrives with a security scope the Rust side cannot
    /// hold — so the app reads it and hands over what it read.
    ///
    /// `mime_type` is the caller's: the platform's own type database is what
    /// knows that `.heic` is `image/heic`, and reimplementing that in the core
    /// would be a worse answer that also had to be maintained.
    ///
    /// # Errors
    ///
    /// [`BindingError::Attachment`] for an empty or oversized file, an unknown
    /// parent task, or a blob store that could not be written.
    pub async fn attach_file(
        &self,
        task: EntityRef,
        filename: String,
        mime_type: String,
        bytes: Vec<u8>,
    ) -> Result<dto::AttachmentItem, BindingError> {
        let att = self
            .inner
            .attach_file(task, filename, mime_type, &bytes)
            .await?;
        Ok(dto::AttachmentItem::from(&att))
    }

    /// One attachment's plaintext, reassembled and verified.
    ///
    /// # Errors
    ///
    /// [`BindingError::AttachmentNotHere`] when this device holds the metadata
    /// but not the chunks — which is a state to render, not a failure to
    /// report. Everything else is [`BindingError::Attachment`].
    pub async fn attachment_bytes(&self, id: EntityRef) -> Result<Vec<u8>, BindingError> {
        Ok(self.inner.attachment_bytes(id).await?)
    }

    /// Whether this device holds every chunk of `attachment`.
    ///
    /// A list of attachments needs this per row to choose between "open" and
    /// "not downloaded", and asking by reassembling each file would read every
    /// byte of every attachment to draw a list.
    ///
    /// # Errors
    ///
    /// [`BindingError::Attachment`] if the blob store cannot be read.
    pub fn attachment_is_local(
        &self,
        attachment: dto::AttachmentItem,
    ) -> Result<bool, BindingError> {
        Ok(self.inner.attachment_is_local(&attachment.to_domain()?)?)
    }

    /// Seal this vault's root into a confirmed pairing, on the existing
    /// device.
    ///
    /// The root never crosses the seam. A `vault_root()` getter would be the
    /// obvious shape and the wrong one: it would put the key that decrypts
    /// everything into a Swift `Data`, where it outlives the call, lands in
    /// whatever the app logs, and is one autocomplete away from a file. The
    /// only thing the app needs is the *ciphertext*, so that is the only thing
    /// it gets.
    ///
    /// # Errors
    ///
    /// [`BindingError::Pairing`] when the SAS has not been confirmed on this
    /// device, or when this device is the one being added.
    pub fn send_vault_root(
        &self,
        pairing: Arc<pairing::DevicePairing>,
    ) -> Result<String, BindingError> {
        pairing.seal_vault_root(
            self.inner
                .export_vault_root_for_pairing()
                .as_bytes()
                .to_vec(),
        )
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

    /// This device's stable id, hex-encoded — what [`SunriseLogin::begin`]
    /// binds the token to.
    #[must_use]
    pub fn device_id(&self) -> String {
        self.inner
            .device_id()
            .iter()
            .fold(String::new(), |mut acc, b| {
                use std::fmt::Write;
                let _ = write!(acc, "{b:02x}");
                acc
            })
    }

    /// This device's certificate (canonical CBOR), for pairing.
    #[must_use]
    pub fn device_cert(&self) -> Vec<u8> {
        self.inner.device_cert()
    }

    /// Start the live-sync driver against `url` (a relay `/sync` WebSocket),
    /// presenting `bearer` on the upgrade.
    ///
    /// `bearer` is empty only for a self-host relay: every other deployment
    /// refuses an unauthenticated upgrade with `401`. Renewing is
    /// [`SunriseCore::set_sync_credential`] — the driver picks the new token up
    /// on its next connect, and the caller does not restart sync.
    ///
    /// Deliberately sync: it only spawns. See the note on [`SunriseCore::rt`]
    /// for why the spawn cannot use `tokio::spawn`.
    pub fn start_sync(&self, url: String, bearer: Option<String>) -> Result<(), BindingError> {
        let _guard = self.rt.enter();
        let credential = self.inner.sync_credential();
        credential.set(bearer);
        self.inner.start_sync(ws_factory(&url, credential))?;
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
    pub fn start_routine_timer(&self, interval_ms: u64) -> Result<(), BindingError> {
        let _guard = self.rt.enter();
        self.inner
            .start_routine_timer(std::time::Duration::from_millis(interval_ms))?;
        Ok(())
    }

    /// Subscribe to domain changes. Cancel by dropping or cancelling the
    /// returned [`Subscription`].
    pub fn subscribe_changes(&self, listener: Arc<dyn ChangeListener>) -> Arc<Subscription> {
        let mut rx = self.inner.changes();
        let handle = self.rt.spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => listener.on_change(ChangeEvent::from(&ev)),
                    Err(RecvError::Lagged(n)) => listener.on_lagged(n),
                    Err(RecvError::Closed) => {
                        listener.on_closed();
                        break;
                    }
                }
            }
        });
        Arc::new(Subscription {
            handle: Mutex::new(Some(handle)),
        })
    }
}

impl SunriseCore {
    /// Submit a recorded batch, stopping at the first failure.
    ///
    /// Not exported: an undo step is a list of commands the seam built, never
    /// one a client hands in.
    /// Submit a recorded step, returning the entity each command touched.
    ///
    /// The ids matter for exactly one command shape: a create, whose entity is
    /// minted here and nowhere else. Everything else returns the id it was
    /// already given, which the caller ignores.
    async fn apply(&self, cmds: &[Command]) -> Result<Vec<EntityRef>, BindingError> {
        let mut touched = Vec::with_capacity(cmds.len());
        for cmd in cmds {
            touched.push(self.inner.submit(cmd.clone()).await?.entity);
        }
        Ok(touched)
    }
}

/// Build the transport factory the sync driver dials with, once per connection
/// attempt (initial connect and every reconnect).
///
/// The bearer is read from `credential` on every attempt rather than captured,
/// so a reconnect after a renewal presents the *current* token.
fn ws_factory(url: &str, credential: sunrise_core::TokenSource) -> sunrise_core::TransportFactory {
    let url = url.to_string();
    Arc::new(move || {
        let url = url.clone();
        let bearer = credential.get();
        Box::pin(async move {
            let t = sunrise_sync::WsTransport::connect_with_bearer(&url, bearer.as_deref()).await?;
            Ok(Box::new(t) as sunrise_core::BoxTransport)
        }) as sunrise_core::ConnectFuture
    })
}

/// One change to the local vault, addressed by entity.
///
/// Deliberately carries no payload beyond the id: a change notification is a
/// prompt to re-read, not a substitute for reading. Sending the entity would
/// make the stream a second, competing source of truth that can arrive out of
/// order with respect to the queries beside it.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum ChangeEvent {
    /// An entity was created.
    Created {
        /// The entity.
        entity: sunrise_id::EntityRef,
    },
    /// An entity was updated.
    Updated {
        /// The entity.
        entity: sunrise_id::EntityRef,
    },
    /// An entity was tombstoned.
    Deleted {
        /// The entity.
        entity: sunrise_id::EntityRef,
    },
    /// An entity's tombstone was compacted away.
    Forgotten {
        /// The entity.
        entity: sunrise_id::EntityRef,
    },
}

impl From<&sunrise_core::DomainEvent> for ChangeEvent {
    fn from(e: &sunrise_core::DomainEvent) -> Self {
        match *e {
            sunrise_core::DomainEvent::Created(entity) => Self::Created { entity },
            sunrise_core::DomainEvent::Updated(entity) => Self::Updated { entity },
            sunrise_core::DomainEvent::Deleted(entity) => Self::Deleted { entity },
            sunrise_core::DomainEvent::Forgotten(entity) => Self::Forgotten { entity },
        }
    }
}

/// The foreign side of the change stream.
///
/// # `on_lagged` is not optional
///
/// The channel behind this is a `tokio::sync::broadcast` with a bounded
/// buffer, and it is **lossy under load**. A spike measured `seen = 257,
/// lagged = 4743` pushing 5000 events past a slow consumer — which is exactly
/// what a sync catch-up burst looks like. A listener that implements only
/// [`ChangeListener::on_change`] will therefore show stale data after any
/// burst, silently.
///
/// Treat `on_lagged` as **"re-run every query you are showing"**. It is not
/// telemetry.
#[uniffi::export(with_foreign)]
pub trait ChangeListener: Send + Sync + 'static {
    /// One change arrived. Called from a runtime worker thread, never the
    /// foreign main thread.
    fn on_change(&self, event: ChangeEvent);
    /// `skipped` events were dropped because this listener fell behind.
    /// Re-read; do not try to reconstruct what was missed.
    fn on_lagged(&self, skipped: u64);
    /// The vault closed. No further calls will arrive.
    fn on_closed(&self);
}

/// Cancel handle for [`SunriseCore::subscribe_changes`].
#[derive(uniffi::Object)]
pub struct Subscription {
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[uniffi::export]
impl Subscription {
    /// Stop delivering. Idempotent, and safe to call from any thread.
    ///
    /// Deliberately **sync**: Swift's `AsyncStream.Continuation.onTermination`
    /// is a non-async closure and cannot await, and an async cancel forces a
    /// detached teardown that races with deallocation. `JoinHandle::abort`
    /// needs no reactor, so a sync method is safe here.
    pub fn cancel(&self) {
        if let Ok(mut g) = self.handle.lock() {
            if let Some(h) = g.take() {
                h.abort();
            }
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for SunriseCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SunriseCore").finish_non_exhaustive()
    }
}

/// Credentials a completed OIDC login yielded.
///
/// `refresh_token` is `None` when the issuer did not return one — some do not
/// for public clients — in which case renewal means logging in again.
#[derive(uniffi::Record)]
pub struct LoginCredentials {
    /// The bearer to hand to [`SunriseCore::set_sync_credential`].
    pub access_token: String,
    /// Used to obtain a new access token without user interaction.
    pub refresh_token: Option<String>,
    /// When `access_token` stops being accepted, epoch milliseconds.
    pub expires_at_ms: u64,
    /// When to renew — 75% of the token's lifetime, not its expiry, so a
    /// renewal has room to fail and be retried before anything breaks.
    pub renew_at_ms: u64,
}

// Hand-written, like `sunrise_auth::Credentials`' own: this record carries a
// live bearer AND a refresh token, and a derived `Debug` would put both in the
// first log line anyone writes while debugging the seam.
impl std::fmt::Debug for LoginCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginCredentials")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at_ms", &self.expires_at_ms)
            .field("renew_at_ms", &self.renew_at_ms)
            .finish()
    }
}

impl From<sunrise_auth::Credentials> for LoginCredentials {
    fn from(c: sunrise_auth::Credentials) -> Self {
        Self {
            access_token: c.access_token,
            refresh_token: c.refresh_token,
            expires_at_ms: c.expires_at_ms,
            renew_at_ms: c.renew_at_ms,
        }
    }
}

/// One OIDC login, driven from the foreign side.
///
/// Two calls, because a browser sits between them:
///
/// 1. [`SunriseLogin::begin`] returns the authorize URL to open, and parks a
///    loopback listener for the redirect.
/// 2. [`SunriseLogin::complete`] waits for that redirect and exchanges the code.
///
/// Storing the result is the caller's job, and on macOS it should be the OS
/// Keychain — reachable from Swift directly, which is why there is no Rust
/// keychain binding here. Hand the access token to
/// [`SunriseCore::set_sync_credential`]; a live session picks it up in band.
#[derive(uniffi::Object)]
pub struct SunriseLogin {
    client: sunrise_auth::OidcClient,
    /// The pending session between `begin` and `complete`.
    ///
    /// `wait_for_redirect` consumes the session, so it is taken out rather than
    /// borrowed — which also makes a second `complete` fail cleanly instead of
    /// waiting on a listener that is already gone.
    session: Mutex<Option<sunrise_auth::LoginSession>>,
}

// Hand-written: `OidcClient` holds the client id and the HTTP stack, and the
// pending session holds a PKCE verifier. Only whether a login is in flight is
// safe to print.
impl std::fmt::Debug for SunriseLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pending = self.session.lock().map(|g| g.is_some()).unwrap_or(false);
        f.debug_struct("SunriseLogin")
            .field("login_in_progress", &pending)
            .finish_non_exhaustive()
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl SunriseLogin {
    /// A login against `issuer` as `client_id`.
    #[uniffi::constructor]
    #[must_use]
    pub fn new(issuer: String, client_id: String) -> Arc<Self> {
        let http = Arc::new(sunrise_auth::HttpsClient::new()) as Arc<dyn sunrise_auth::HttpClient>;
        Arc::new(Self {
            client: sunrise_auth::OidcClient::new(issuer, client_id, http),
            session: Mutex::new(None),
        })
    }

    /// Discover the provider and start a login. Returns the URL to open.
    ///
    /// `device_id` binds the token to this device: the issuer stamps it into a
    /// `device_id` claim and the relay refuses a token whose claim names a
    /// different device, so a token lifted off this machine is useless
    /// elsewhere. Get it from [`SunriseCore::device_id`].
    pub async fn begin(&self, device_id: String) -> Result<String, BindingError> {
        let metadata = self.client.discover().await?;
        let session = self.client.begin_login(&metadata, &device_id).await?;
        let url = session.authorize_url().to_string();
        if let Ok(mut g) = self.session.lock() {
            *g = Some(session);
        }
        Ok(url)
    }

    /// Wait for the browser redirect, then exchange the code for tokens.
    ///
    /// Fails if [`SunriseLogin::begin`] has not run, or has already been
    /// completed.
    pub async fn complete(
        &self,
        timeout_ms: u64,
        now_ms: u64,
    ) -> Result<LoginCredentials, BindingError> {
        // Taken, not borrowed, and the guard is dropped before the await:
        // `wait_for_redirect` consumes the session, and holding a std guard
        // across an await point would not be `Send`.
        let session = self
            .session
            .lock()
            .ok()
            .and_then(|mut g| g.take())
            .ok_or_else(|| BindingError::Login("no login is in progress".into()))?;
        let capture = session
            .wait_for_redirect(std::time::Duration::from_millis(timeout_ms))
            .await?;
        Ok(self.client.exchange(&capture, now_ms).await?.into())
    }

    /// Exchange a refresh token for a fresh access token, without user
    /// interaction. Drive it from [`LoginCredentials::renew_at_ms`].
    pub async fn refresh(
        &self,
        refresh_token: String,
        now_ms: u64,
    ) -> Result<LoginCredentials, BindingError> {
        let metadata = self.client.discover().await?;
        Ok(self
            .client
            .refresh(&metadata, &refresh_token, now_ms)
            .await?
            .into())
    }
}
