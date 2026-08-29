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
    static func open(
        directory: URL,
        vaultRoot: Data,
        appVersion: String
    ) async throws -> CoreBridge {
        try FileManager.default.createDirectory(
            at: directory,
            withIntermediateDirectories: true
        )
        let core = try await SunriseCore.open(
            vaultDir: directory.path(percentEncoded: false),
            vaultRoot: vaultRoot,
            appVersion: appVersion
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

    // MARK: - Pairing

    /// Seal this vault's root into a confirmed pairing.
    ///
    /// The root itself never reaches Swift. What comes back is ciphertext only
    /// the device on the other end of the confirmed handshake can open.
    func sendVaultRoot(to pairing: DevicePairing) throws -> String {
        try core.sendVaultRoot(pairing: pairing)
    }

    /// This device's stable id, hex-encoded — what a login binds its token to.
    func deviceId() -> String { core.deviceId() }

    /// This device's certificate, canonical CBOR. A peer must trust it before
    /// it will accept this device's ops.
    func deviceCertificate() -> Data { core.deviceCert() }

    // MARK: - Sync

    /// Start the live-sync driver. `bearer` is `nil` only against a self-host
    /// relay; every other deployment refuses an unauthenticated upgrade.
    func startSync(url: String, bearer: String?) throws {
        try core.startSync(url: url, bearer: bearer)
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
    func changes(window: Duration = .milliseconds(50)) async -> AsyncStream<ChangeBatch> {
        startListening()
        return await broadcast.subscribe().coalesced(window: window)
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
