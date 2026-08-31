import Foundation

/// How an intent gets at the vault — and gives it back.
///
/// # Where intents run, and why that decides everything else
///
/// These `AppIntent`s are declared in the **app target**, not an extension, so
/// the App Intents runtime executes them *inside this app's process*, launching
/// Sunrise first if it is not already running. That is not a detail:
/// `crates/sunrise-core/src/vault_lock.rs` admits exactly one open vault per
/// process — a process-local registry checked *before* the OS advisory lock —
/// so "the intent opens its own `Core`" and "the app has a `Core` open" cannot
/// both be true. There is one vault or there is an error naming this very
/// process as the holder.
///
/// So there are exactly two ways an intent can have a vault, and this type is
/// both of them.
///
/// ## 1. The app hands one over (``adopt(_:)``)
///
/// When the window is open the app already holds the lock, and the only
/// workable answer is to use *its* `CoreBridge`. The app publishes it here
/// when a vault attaches and withdraws it when one detaches.
///
/// > Important: **The publishing call does not exist yet.** It belongs in
/// > `SunriseApp.swift`, which this change does not own — `AppSurfaces.attach`
/// > would call `IntentVault.adopt(bridge)` and `AppSurfaces.releaseVault`
/// > would call `IntentVault.adopt(nil)`, both one line, both already on the
/// > exact code path that knows. Until it lands, an intent invoked while
/// > Sunrise has its vault open falls through to case 2, fails to take the
/// > lock, and says so — see ``IntentError/vaultHeldByThisApp``. It reports;
/// > it does not quietly do nothing.
///
/// ## 2. The intent opens one itself, briefly (``withVault(_:)``)
///
/// The cold case, and the common one for automation: nobody has Sunrise open,
/// the system launches it in the background to run the intent, and no window
/// is ever built — so the app's own `SessionModel` never opens anything. Here
/// the intent opens the vault through the *same launch decision* the window
/// makes (`SessionModel.start()`), uses it, and **closes it again**.
///
/// Closing is not tidiness. The process survives the intent — a `MenuBarExtra`
/// app does not exit — and a vault left open by an intent would refuse the
/// window the user opens five minutes later, on this same lock, for as long as
/// the app ran. Leases are counted so two intents in one Shortcut share one
/// open and close it once.
///
/// It never *creates* a vault. `SessionModel.createVault` is the one operation
/// that can make existing data unreadable, and an automation must not be the
/// thing that decides to run it.
@MainActor
enum IntentVault {
    // MARK: - The app's vault

    /// The vault the app has open, when it has one.
    private static var adopted: CoreBridge?

    /// Publish — or withdraw — the app's open vault.
    ///
    /// Idempotent and order-independent: the app may attach a new vault
    /// without detaching the old one (see `AppSurfaces.attach`), and passing
    /// the new bridge simply replaces the old.
    static func adopt(_ bridge: CoreBridge?) {
        adopted = bridge
    }

    /// The vault, only if one is already open. Never opens anything, never
    /// launches anything.
    ///
    /// Used by ``TaskEntityQuery/suggestedEntities()``: the system asks for
    /// suggestions speculatively — while indexing, while drawing a tile — and
    /// decrypting a vault to fill a picker nobody opened is not work an
    /// automation surface should cause.
    static func existing() -> CoreBridge? {
        #if DEBUG
        if let override { return override }
        #endif
        return adopted ?? owned
    }

    #if DEBUG
    /// Test seam: a scratch vault to run intents against, in place of the
    /// app's. Mirrors `UITestHarness.scratchVault()` — a test must not touch
    /// the developer's vault or their login Keychain. Leases are skipped for
    /// it, because the test owns its lifetime.
    static var override: CoreBridge?
    #endif

    // MARK: - Leases

    /// The vault this type opened itself, if any.
    private static var owned: CoreBridge?
    /// How many in-flight intents are using ``owned``.
    private static var leases = 0
    /// An open already in progress, so two intents starting together share it
    /// rather than racing each other for the lock.
    private static var opening: Task<CoreBridge, any Error>?

    /// Run `body` against an open vault, releasing it afterwards if this type
    /// was what opened it.
    ///
    /// Throws rather than handing back `nil`: every caller is an intent, and
    /// an intent that reported success having done nothing is the defect this
    /// whole surface exists to rule out.
    static func withVault<T: Sendable>(
        _ body: @Sendable (CoreBridge) async throws -> T
    ) async throws -> T {
        #if DEBUG
        if let override { return try await body(override) }
        #endif
        if let adopted { return try await body(adopted) }

        let bridge = try await lease()
        do {
            let value = try await body(bridge)
            await release()
            return value
        } catch {
            await release()
            throw error
        }
    }

    private static func lease() async throws -> CoreBridge {
        if let owned {
            leases += 1
            return owned
        }
        if let opening {
            let bridge = try await opening.value
            leases += 1
            return bridge
        }
        let task = Task { try await openOurOwn() }
        opening = task
        defer { opening = nil }
        let bridge = try await task.value
        owned = bridge
        leases += 1
        return bridge
    }

    private static func release() async {
        leases = max(0, leases - 1)
        guard leases == 0, let bridge = owned else { return }
        owned = nil
        await bridge.shutdown()
    }

    /// Open the vault the way the window would.
    ///
    /// A `SessionModel` of our own, deliberately: `SessionModel.standard()`
    /// builds a fresh one every call and opens nothing until `start()` is
    /// asked to, so this makes the same Keychain-and-directory decision the
    /// window makes and lands in the same five states. Anything other than
    /// `.unlocked` is a sentence for the user, never a retry.
    private static func openOurOwn() async throws -> CoreBridge {
        let session = SessionModel.standard()
        await session.start()
        switch session.phase {
        case .unlocked:
            guard let bridge = session.bridge else { throw IntentError.vaultUnavailable }
            return bridge
        case .firstRun:
            throw IntentError.noVault
        case let .locked(reason):
            throw IntentError.vaultLocked(reason.summary)
        case let .failed(message):
            throw failure(message)
        case .starting:
            throw IntentError.vaultBusy
        }
    }

    /// Tell "somebody already holds this vault" apart from every other way
    /// opening can fail.
    ///
    /// Matched on the message because that is all `SessionModel` keeps —
    /// `.failed` carries a `localizedDescription`, and the typed
    /// `VaultLockError::AlreadyHeld` behind it is flattened into
    /// `BindingError::Core` at the seam. The match only chooses the wording:
    /// an unrecognised message still reports, verbatim, so a changed error
    /// string costs a good sentence and never a silent success.
    private static func failure(_ message: String) -> IntentError {
        message.contains("vault locked by pid")
            ? .vaultHeldByThisApp
            : .vaultFailed(message)
    }
}
