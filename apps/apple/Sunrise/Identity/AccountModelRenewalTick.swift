import Foundation

// MARK: - The renewal tick

/// Its own file because `AccountModel.swift` sits at SwiftLint's
/// `file_length` warning, which `swiftlint lint --strict` makes an error.
extension AccountModel {
    /// How often ``renewWhileRunning(issuer:clientID:now:every:sleep:tokenChanged:)``
    /// looks at the clock.
    ///
    /// ``refreshIfNeeded(issuer:clientID:nowMs:)`` decides *whether* to renew
    /// — at the token's own renewal point, 75% of its life — so this only
    /// bounds how late after that point the renewal starts. A look that is
    /// not due is one clock read and one comparison; thirty seconds is far
    /// inside the quarter of a token's life that is left when it comes due.
    nonisolated static let renewalCheckInterval: Duration = .seconds(30)

    /// Renew the session for as long as the calling task runs.
    ///
    /// The one production caller of ``refreshIfNeeded(issuer:clientID:nowMs:)``.
    /// Run by ``SessionModel/renewSessionWhileOpen(every:sleep:)`` for as long
    /// as the vault is open — not by a window, whose closing left sync on the
    /// last bearer (#307). Cancellation stops it at the next sleep; a renewal
    /// already in flight finishes under ``sessionGeneration``'s rules.
    ///
    /// Looks once before the first sleep, so a launch that restored a token
    /// already past its renewal point renews it at once rather than an
    /// interval later. A look that changed the bearer hands it to
    /// `tokenChanged` — the sync driver, which no window is left to tell.
    ///
    /// The settings are read on every look rather than captured once, so an
    /// issuer edited in Settings is the one the next renewal asks. `now` and
    /// `sleep` are parameters so a test drives the loop on a fake clock.
    func renewWhileRunning(
        issuer: @escaping @MainActor () -> String,
        clientID: @escaping @MainActor () -> String,
        now: @escaping @MainActor () async -> UInt64,
        every interval: Duration = AccountModel.renewalCheckInterval,
        sleep: @escaping (Duration) async throws -> Void = { try await _Concurrency.Task.sleep(for: $0) },
        tokenChanged: @escaping @MainActor (String?) async -> Void = { _ in }
    ) async {
        while !_Concurrency.Task.isCancelled {
            let (nowMs, before) = (await now(), accessToken)
            await refreshIfNeeded(issuer: issuer(), clientID: clientID(), nowMs: nowMs)
            // Not after a cancel: the owner has let go of the vault it names.
            if accessToken != before, !_Concurrency.Task.isCancelled { await tokenChanged(accessToken) }
            do {
                try await sleep(interval)
            } catch {
                return
            }
        }
    }
}
