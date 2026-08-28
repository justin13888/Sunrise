import Foundation

/// The app's one way into the Rust core.
///
/// An actor, so that the handle, its live change subscription and its sync
/// state have a single owner. The generated `SunriseCore` is itself
/// thread-safe — `@unchecked Sendable`, backed by an `Arc` — so this is not
/// about data races on the handle. It is about the things *around* it: a
/// second `subscribeChanges` would silently double every repaint, and a
/// `shutdown` racing an in-flight `submit` would surface as a `rustPanic`.
///
/// Every method here is a pass-through. The bridge deliberately owns no
/// vocabulary: what a task row says, when something is overdue, how a duration
/// reads are all questions the core answers (see `vocab.rs`), and a bridge
/// that started answering them would be the second source of truth this whole
/// design exists to avoid.
actor CoreBridge {
    private let core: SunriseCore
    private var subscription: Subscription?

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

    /// Parse a capture line without writing anything. Debounce before calling:
    /// each one costs two vault reads to resolve `#stream` and `@context`.
    func previewCapture(_ text: String, timeZone: String) async throws -> CapturePreview {
        try await core.previewCapture(text: text, tz: timeZone)
    }

    /// The core's clock. One reading should drive a whole screen, so that two
    /// rows in the same list cannot disagree about what "today" is.
    func nowMs() -> UInt64 { core.nowMs() }

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

    /// A stream of repaints.
    ///
    /// One subscription per bridge: calling this twice cancels the first, so a
    /// view that re-subscribes on appear cannot leave a doubled feed behind.
    ///
    /// Consumers **must** honour `ChangeBatch.isComplete`. When it is `false`
    /// the feed lost notifications and the id list is not the whole story —
    /// re-run every query on screen rather than patching the rows named.
    func changes(window: Duration = .milliseconds(50)) -> AsyncStream<ChangeBatch> {
        subscription?.cancel()
        let (raw, listener) = ChangeFeed.make()
        subscription = core.subscribeChanges(listener: listener)
        return raw.coalesced(window: window)
    }

    /// Stop the sync driver, end the change stream and release the vault lock.
    /// Idempotent.
    func shutdown() async {
        subscription?.cancel()
        subscription = nil
        await core.shutdown()
    }
}
