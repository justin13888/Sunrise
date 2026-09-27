import Foundation

/// The account's renewal tick, and which open vault it was started against.
///
/// Its own type, and its own file, because `SessionModel.swift` sits at
/// SwiftLint's `file_length` warning, which `swiftlint lint --strict` makes an
/// error. ``SessionModel`` holds one and stops it wherever it lets go of a
/// vault: ``SessionModel/lock()`` and ``SessionModel/switchTo(_:)``.
///
/// A `Task` this object holds rather than a `.task` on a view, and that is the
/// whole of #307. A view's `.task` ends with the view, and on macOS the view
/// that ran this was the main window — while the vault, the menu bar item and
/// the sync driver all outlive it. Closing the window stopped renewal and left
/// sync presenting the last bearer until it expired.
@MainActor
final class SessionRenewal {
    private var tick: _Concurrency.Task<Void, Never>?
    private var vault: ObjectIdentifier?

    /// Whether a tick is running against `bridge` specifically. A tick against
    /// a `Core` a switch has already shut down renews nothing sync can use.
    func isRunning(against bridge: CoreBridge) -> Bool {
        tick != nil && vault == ObjectIdentifier(bridge)
    }

    /// Start `body` as the tick for `bridge`, unless one is already running
    /// against it. Returns whether it started one.
    ///
    /// A no-op the second time because every window calls it: a Mac can open
    /// a second main window, and a window closed and reopened calls it again.
    /// Each used to run a tick of its own.
    @discardableResult
    func start(against bridge: CoreBridge, _ body: @escaping @MainActor () async -> Void) -> Bool {
        guard !isRunning(against: bridge) else { return false }
        tick?.cancel()
        vault = ObjectIdentifier(bridge)
        tick = _Concurrency.Task { @MainActor in await body() }
        return true
    }

    /// End the tick. It stops at its next sleep; a renewal already in flight
    /// finishes, and hands its token to nothing.
    func stop() {
        tick?.cancel()
        tick = nil
        vault = nil
    }
}

extension SessionModel {
    /// Whether the account is being renewed against the vault open now.
    var isRenewingSession: Bool {
        bridge.map(renewal.isRunning(against:)) ?? false
    }

    /// Keep the account's session renewed for as long as this vault is open,
    /// and hand each renewed bearer to its sync driver.
    ///
    /// Called by each shell once the account has taken its first look — see
    /// ``AccountModel/restoreIfUnread()`` — so the tick's first look sees the
    /// restored token. The caller's lifetime does not bound the tick: closing
    /// the window that called this leaves it running, and only letting go of
    /// the vault ends it.
    ///
    /// The settings are read afresh on every look, from the same defaults the
    /// Settings screen writes, so an issuer edited there is the one the next
    /// renewal asks. `every` and `sleep` are for tests.
    func renewSessionWhileOpen(
        every interval: Duration = AccountModel.renewalCheckInterval,
        sleep: @escaping @Sendable (Duration) async throws -> Void = {
            try await _Concurrency.Task.sleep(for: $0)
        }
    ) {
        guard let bridge else { return }
        let (account, defaults) = (account, settingsDefaults)
        renewal.start(against: bridge) {
            await account.renewWhileRunning(
                issuer: { AppSettings(defaults: defaults).oidcIssuer },
                clientID: { AppSettings(defaults: defaults).oidcClientID },
                now: { await bridge.nowMs() },
                every: interval,
                sleep: sleep,
                tokenChanged: { await bridge.setSyncCredential($0) }
            )
        }
    }
}
