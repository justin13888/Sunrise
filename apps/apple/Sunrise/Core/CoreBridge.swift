import Foundation

/// The app's one way into the Rust core.
///
/// An actor, so that the handle, its live change subscription and its sync
/// state have a single owner. The generated `SunriseCore` is itself
/// thread-safe — `@unchecked Sendable`, backed by an `Arc` — so this is not
/// about data races on the handle. It is about the things *around* it: the one
/// FFI change subscription has to be opened exactly once however many screens
/// ask for a feed, and a `shutdown` racing an in-flight `submit` would surface
/// as a `rustPanic`.
///
/// Every method here is a pass-through. The bridge deliberately owns no
/// vocabulary: what a task row says, when something is overdue, how a duration
/// reads are all questions the core answers (see `vocab.rs`), and a bridge
/// that started answering them would be the second source of truth this whole
/// design exists to avoid.
actor CoreBridge {
    private let core: SunriseCore
    /// The one FFI subscription, opened lazily by ``startListening()``.
    private var subscription: Subscription?
    /// Pumps that subscription into ``broadcast``.
    private var upstream: Task<Void, Never>?
    /// Where the one feed becomes as many feeds as there are screens.
    private let broadcast = ChangeBroadcast()
    private var isShutDown = false

    /// Open — or create — the vault at `directory`, keyed by a 32-byte root.
    ///
    /// The root comes from the Keychain or a completed pairing. This type does
    /// not derive it and does not keep a copy.
    ///
    /// `pairedBundle` is what a completed pairing produced, and is passed
    /// **once**, on the first open of a device that was just added. Since
    /// ADR-0024 the root no longer implies the key schedule — Stream keys are
    /// random, not derived — so a device handed only a root would open a vault
    /// full of ciphertext it could never read. `nil` everywhere else.
    static func open(
        directory: URL,
        vaultRoot: Data,
        appVersion: String,
        pairedBundle: Data? = nil
    ) async throws -> CoreBridge {
        try FileManager.default.createDirectory(
            at: directory,
            withIntermediateDirectories: true
        )
        let core = try await SunriseCore.open(
            vaultDir: directory.path(percentEncoded: false),
            vaultRoot: vaultRoot,
            appVersion: appVersion,
            pairedBundle: pairedBundle
        )
        return CoreBridge(core: core)
    }

    private init(core: SunriseCore) {
        self.core = core
    }

    // MARK: - Reads and writes

    func submit(_ command: CoreCommand) async throws -> CommandOutcome {
        try await core.submit(cmd: command)
    }

    func query(_ query: CoreQuery) async throws -> CoreQueryResult {
        try await core.query(q: query)
    }

    /// Submit a command **and record how to reverse it**.
    ///
    /// `label` is what the Undo menu item will say. The returned outcome
    /// carries why nothing went on the stack when a command has no inverse —
    /// a delete, most of all — and callers are expected to show that rather
    /// than leave an Undo item that would do nothing.
    func submitUndoable(_ command: CoreCommand, label: String) async throws -> UndoableOutcome {
        try await core.submitUndoable(cmd: command, label: label)
    }

    /// Reverse the most recent recorded step; the label of what was undone,
    /// or `nil` when there is nothing.
    func undo() async throws -> String? { try await core.undo() }

    /// Replay the most recently undone step.
    func redo() async throws -> String? { try await core.redo() }

    /// What the Undo and Redo menu items should say right now.
    func undoState() -> UndoState { core.undoState() }

    /// Parse a capture line without writing anything. Debounce before calling:
    /// each one costs two vault reads to resolve `#stream` and `@context`.
    func previewCapture(_ text: String, timeZone: String) async throws -> CapturePreview {
        try await core.previewCapture(text: text, tz: timeZone)
    }

    /// The core's clock. One reading should drive a whole screen, so that two
    /// rows in the same list cannot disagree about what "today" is.
    func nowMs() -> UInt64 { core.nowMs() }

    // MARK: - Attachments

    /// Seal a file's bytes into the vault and record them against a task.
    ///
    /// The bytes go over whole rather than as a path: a file the user picked
    /// arrives with a security scope this process holds and the Rust side
    /// cannot, so reading it is the app's job. `mimeType` is the app's too —
    /// `UTType` is what knows a `.heic` is `image/heic`.
    func attachFile(
        to task: EntityRef,
        filename: String,
        mimeType: String,
        bytes: Data
    ) async throws -> AttachmentItem {
        try await core.attachFile(task: task, filename: filename, mimeType: mimeType, bytes: bytes)
    }

    /// One attachment's plaintext, reassembled and hash-checked by the core.
    ///
    /// Throws `BindingError.AttachmentNotHere` when this device holds the row
    /// and not the chunks — a state to render, not a failure to report.
    func attachmentBytes(_ id: EntityRef) async throws -> Data {
        try await core.attachmentBytes(id: id)
    }

    /// Whether this device holds every chunk of `attachment`.
    func attachmentIsLocal(_ attachment: AttachmentItem) throws -> Bool {
        try core.attachmentIsLocal(attachment: attachment)
    }

    /// Download one attachment's bytes on demand, whatever its size.
    ///
    /// What the Download button calls. Under the core's 10 MiB auto-fetch
    /// threshold nothing needs this — the sync driver fetches those unasked —
    /// and over it this is the only route to the bytes at all.
    ///
    /// Returns when they are here. It has no timeout by design, and cancelling
    /// the Swift `Task` awaiting it will not stop it: a UniFFI async call
    /// carries no cancellation across the seam, so the way to end one is
    /// `cancelAttachmentFetch`.
    ///
    /// Throws `BindingError.AttachmentDownloadCancelled` when that happens,
    /// which is the user's own decision rather than a failure to report.
    func fetchAttachment(_ id: EntityRef) async throws {
        try await core.fetchAttachment(id: id)
    }

    /// Stop a running download and mark the attachment partial.
    ///
    /// Synchronous and immediate: it releases the pending `fetchAttachment`
    /// whether or not a relay is answering, which is the state a user is most
    /// likely to be cancelling from. A no-op for an id with nothing
    /// outstanding.
    func cancelAttachmentFetch(_ id: EntityRef) throws {
        try core.cancelAttachmentFetch(id: id)
    }

    /// This device's cache state for one attachment's bytes.
    ///
    /// Durable, so a row still reads `.partial` after a relaunch — which is
    /// what makes it a cache state rather than a view state, and why the model
    /// reads it back rather than remembering it.
    func attachmentFetchState(_ id: EntityRef) throws -> AttachmentFetchState {
        try core.attachmentFetchState(id: id)
    }

    // MARK: - Recovery

    /// Whether this vault holds the account identity's unwrapping key.
    ///
    /// True on the device that created the account, false on one admitted by
    /// pairing. Where no recovery blob has been sealed it means *this vault is
    /// the only place `ID_D_priv` exists, and losing it destroys that key
    /// permanently* — which is why it is worth showing and not only acting on
    /// (#144).
    func holdsIdentityKey() async -> Bool {
        core.holdsIdentityKey()
    }

    /// Whether a revocation of `device` is still owed to the relay.
    ///
    /// The second half of what a device list must say after revoking
    /// something. The vault half is committed when the command returns; this
    /// one is a queued intent that needs a session, and a client reporting only
    /// the first would let someone with no network believe a stolen laptop had
    /// been cut off from the server (#160).
    func relayRevocationPending(_ device: EntityRef) async throws -> Bool {
        try core.relayRevocationPending(device: device)
    }

    /// Publish this vault to the relay and come back with the recovery code.
    ///
    /// The Apple clients' `sunrise bootstrap`, and one call rather than three
    /// on purpose: the seed is drawn, sealed, uploaded and dropped on the Rust
    /// side, so the code that comes back is always the code the stored blob
    /// opens. `recoveryCode` is `nil` on a device admitted by pairing, which
    /// holds no key to seal with — not a failure.
    func bootstrapAccount(
        relayURL: String,
        bearer: String,
        email: String,
        nickname: String
    ) async throws -> AccountBootstrap {
        try await core.bootstrapAccount(
            relayUrl: relayURL,
            bearer: bearer,
            email: email,
            nickname: nickname
        )
    }

    // MARK: - iCalendar

    /// Read an `.ics` document's `VEVENT`s into the vault as time blocks.
    ///
    /// The document crosses as **text**, not a path, for the reason
    /// ``attachFile(to:filename:mimeType:bytes:)`` takes bytes: the file the
    /// user picked carries a security scope this process holds and the Rust
    /// side cannot, so reading it is the app's job.
    ///
    /// `stream` is where the Blocks are filed — `nil` is the Inbox. `source`
    /// is *not* a parameter, and that is the load-bearing part: the Block an
    /// event lands on is BLAKE3 over `(source, UID)`, so the source is half
    /// the identity that makes a second import of the same file update the
    /// same Blocks instead of duplicating them. The core's default is the one
    /// shared `ics` name, and there is no reason for a file picker to want
    /// anything else.
    ///
    /// The returned report's notices name everything the file held that a
    /// Block cannot. They are the caller's to show.
    func importIcal(text: String, into stream: EntityRef? = nil) async throws -> IcalImportReport {
        try await core.importIcal(text: text, streamId: stream, source: nil)
    }

    /// Render one window of the calendar as an `.ics` document.
    ///
    /// `atMs` defaults to the core's own clock, so "today" and "this week" mean
    /// what the rest of the app means by them rather than what this device's
    /// `Date()` happens to say.
    func exportIcal(window: ExportWindow, atMs: UInt64? = nil) async throws -> String {
        try await core.exportIcal(window: window, atMs: atMs ?? core.nowMs())
    }

    // MARK: - Pairing

    /// Whether this vault can add a device at all.
    ///
    /// False on a vault that was itself added by pairing: since #105 it holds
    /// the account's public identity and no signing key, so it cannot issue the
    /// certificate a joining device needs. Ask before offering the button — a
    /// user who walks eight legs to a failure at the last one has been told the
    /// wrong thing for seven of them.
    func canSponsorPairing() -> Bool { core.canSponsorPairing() }

    /// Seal message 1 — this account's identity — into a confirmed pairing.
    ///
    /// Nothing secret reaches Swift on this leg or any other. This one carries
    /// no secret at all: the vault key and the Stream keys do not move until
    /// ``sendPairingGrant(to:request:)``, which runs on a request this vault
    /// accepted. What comes back is ciphertext only the device on the other end
    /// of the confirmed handshake can open.
    func sendPairingOffer(to pairing: DevicePairing) throws -> String {
        try core.sendPairingOffer(pairing: pairing)
    }

    /// Issue the joining device's certificate and seal message 3.
    ///
    /// `request` is the sealed block that device produced. The certificate is
    /// signed inside the core, over the keys that block names — `ID_S_priv`
    /// does not reach this seam, let alone Swift — and the vault key and every
    /// Stream key are sealed alongside it.
    func sendPairingGrant(to pairing: DevicePairing, request: String) throws -> String {
        try core.sendPairingGrant(pairing: pairing, sealedRequest: request)
    }

    /// This device's stable id, hex-encoded — what a login binds its token to.
    func deviceId() -> String { core.deviceId() }

    /// This device's certificate, canonical CBOR. A peer must trust it before
    /// it will accept this device's ops.
    func deviceCertificate() -> Data { core.deviceCert() }

    // MARK: - Sync

    /// Start the live-sync driver. `bearer` is `nil` only against a self-host
    /// relay; every other deployment refuses an unauthenticated upgrade.
    ///
    /// `relayDeviceID` is the ADR-0022 device binding — the ULID the relay
    /// minted at registration, not `deviceId()`, which is this vault's own id
    /// and names no relay row. `nil` starts an unbound driver, which a relay
    /// with `require_device_sig` refuses.
    func startSync(url: String, bearer: String?, relayDeviceID: String?) throws {
        try core.startSync(url: url, bearer: bearer, relayDeviceId: relayDeviceID)
    }

    /// Hand the driver a renewed token. It is picked up on the next connect,
    /// without tearing the session down.
    func setSyncCredential(_ bearer: String?) {
        core.setSyncCredential(bearer: bearer)
    }

    /// Start the periodic routine-materialisation timer.
    func startRoutineTimer(everyMs: UInt64) throws {
        try core.startRoutineTimer(intervalMs: everyMs)
    }

    // MARK: - Changes

    /// A stream of repaints, one per caller.
    ///
    /// Every screen calls this and every screen gets its own stream with its
    /// own lifetime: ending one — a view disappearing, a `.task` cancelled —
    /// leaves the others untouched. There is still exactly **one** FFI
    /// subscription underneath, started on the first call and fanned out by
    /// ``ChangeBroadcast``.
    ///
    /// It did not used to be that way, and the cost was not subtle. This
    /// method cancelled the previous subscription, so the last screen to
    /// appear owned the feed and every other one silently stopped repainting —
    /// on sync, on another device's writes, and on writes from elsewhere in
    /// this app. It also cost the notification scheduler its ability to follow
    /// changes at all; see ``ReminderScheduler/follow(debounce:)``.
    ///
    /// Consumers **must** honour `ChangeBatch.isComplete`. When it is `false`
    /// the feed lost notifications and the id list is not the whole story —
    /// re-run every query on screen rather than patching the rows named. Every
    /// live consumer is told about a lag, not just the one that was subscribed
    /// first.
    ///
    /// **The first batch is always a prime** — empty `touched`, `isComplete`
    /// false — and it is what makes the subscription safe to open late. The
    /// subscription itself is established before this method returns; the
    /// prime then tells the consumer to re-read, so anything written before it
    /// subscribed is picked up by the query rather than waited for on a feed
    /// that will never mention it. See ``primed()`` for what that closes.
    /// **Except onto a vault that has already shut down**, where a prime would
    /// be an instruction to re-read a core that will refuse the read.
    /// `Core.query` returns `CoreError.Closed` once `shutdown()` has run,
    /// nothing on this type guards a query on that, and a model such as
    /// `TaskListModel` paints the throw — so a feed opened during teardown put
    /// "core is closed" on screen. Every `follow()` loop guards on
    /// `isClosed` and would otherwise have been correct; it was the prime in
    /// front of the close batch that made them all re-read first.
    ///
    /// Subscribing and reading the flag are **one** hop into
    /// ``ChangeBroadcast``, not two. As two, a `finish()` processed between
    /// them left the flag reading `false` after the close, so the prime went
    /// out anyway and every `follow()` loop re-read a shut-down core — the
    /// window was narrower than before and the symptom identical. The sticky
    /// flag does not rescue that: stickiness orders the batches, and what goes
    /// wrong is *when the consumer's query runs*, which is after the close
    /// either way. ``ChangeBroadcast/subscribeIfOpen()`` answers both under
    /// one isolation, so nothing can run between them. `primed()` itself is
    /// unchanged, so the decision lives in one place and
    /// `PrimedChangeStreamTests` goes on testing the extension in isolation.
    func changes(window: Duration = .milliseconds(50)) async -> AsyncStream<ChangeBatch> {
        startListening()
        let (raw, wasOpen) = await broadcast.subscribeIfOpen()
        let batches = raw.coalesced(window: window)
        return wasOpen ? batches.primed() : batches
    }

    /// Open the one FFI subscription, on the first screen that asks for it.
    ///
    /// Kept for the life of the bridge rather than reference-counted down to
    /// zero: tearing it down when the last screen disappears would leave a
    /// window — between the last detach and the next attach — in which changes
    /// arrive with nobody subscribed and are simply lost, which is the same
    /// class of bug in a smaller costume.
    private func startListening() {
        guard subscription == nil, !isShutDown else { return }
        let (raw, listener) = ChangeFeed.make()
        subscription = core.subscribeChanges(listener: listener)
        let broadcast = self.broadcast
        upstream = Task {
            for await change in raw {
                await broadcast.publish(change)
            }
            // The listener finished without an `onClosed` — a cancelled
            // subscription, or a torn-down vault. Consumers still have to be
            // released rather than left awaiting a stream nothing will feed.
            await broadcast.finish()
        }
    }

    /// Stop the sync driver, end every change stream and release the vault
    /// lock. Idempotent.
    func shutdown() async {
        isShutDown = true
        subscription?.cancel()
        subscription = nil
        upstream?.cancel()
        upstream = nil
        await broadcast.finish()
        await core.shutdown()
    }
}
