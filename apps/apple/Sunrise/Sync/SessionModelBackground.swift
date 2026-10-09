import Foundation

/// What the OS grants a background run, less a margin for what follows the
/// sync: re-planning reminders and refreshing the widgets. An app refresh
/// task and a silent push both get about thirty seconds.
let backgroundSyncBudgetMs: UInt64 = 25000

extension SessionModel {
    /// One background sync of this session's vault — what a refresh task, a
    /// maintenance window and a silent push all run
    /// (`docs/07-clients/mobile-ios.md` §Background sync).
    ///
    /// Opens the vault first when the OS launched the app straight into the
    /// background, through the same ``start()`` a window uses — single-flight,
    /// so a window appearing meanwhile joins this open rather than racing it.
    ///
    /// Does nothing, and says so with `.noData`, when there is nothing it may
    /// do: the vault did not open (it is locked, the Keychain would not
    /// answer before the first unlock, or there is no vault yet), or the sync
    /// plan is off. The credential is renewed first if it is due, because a
    /// background run is often the first thing to happen in hours.
    ///
    /// `afterSync` runs once the sync has, with the vault it synced: the
    /// shell re-plans local notifications and redraws the widgets there.
    ///
    /// `budgetMs` covers the whole run, not the sync alone: what opening the
    /// vault, renewing the token and binding the relay device spent comes out
    /// of what the sync is given. A cancelled run — the OS ended the budget —
    /// stops at the next step and answers `.failed`; the sync itself ends at
    /// once (`CoreBridge.syncOnce`), and nothing runs after it.
    func backgroundSync(
        budgetMs: UInt64 = backgroundSyncBudgetMs,
        afterSync: @MainActor (CoreBridge) async -> Void = { _ in }
    ) async -> BackgroundSyncResult {
        let began = ContinuousClock.now
        if mayOpenInBackground { await start() }
        guard phase == .unlocked, let bridge else { return .noData }

        account.restoreIfUnread()
        let settings = AppSettings(defaults: settingsDefaults)
        await account.refreshIfNeeded(
            issuer: settings.oidcIssuer,
            clientID: settings.oidcClientID,
            nowMs: await bridge.nowMs()
        )
        await bridge.setSyncCredential(account.accessToken)
        await bindRelayDevice()

        let token = account.accessToken
        guard case let .connect(url, bearer, deviceID) = SyncPlan(
            relayURL: settings.relayURL,
            accessToken: token,
            relayDeviceID: relayDeviceID(relayURL: settings.relayURL, bearer: token)
        ) else { return .noData }

        let spentMs = UInt64(max(0, (ContinuousClock.now - began) / .milliseconds(1)))
        guard !Task.isCancelled, spentMs < budgetMs else { return .failed }
        let outcome: SyncOnceOutcome
        do {
            outcome = try await bridge.syncOnce(
                url: url,
                bearer: bearer,
                relayDeviceID: deviceID,
                budgetMs: budgetMs - spentMs
            )
        } catch {
            return .failed
        }
        guard !Task.isCancelled else { return .failed }
        await afterSync(bridge)
        return BackgroundSyncResult(outcome)
    }

    /// Whether a background run may open the vault itself.
    ///
    /// Only from the two phases nobody chose: the launch that has not opened
    /// anything yet — the OS started the app straight into the background —
    /// and a Keychain that would not answer, which before the first unlock
    /// after a restart is expected and may have cleared since. A vault the
    /// user locked stays locked, and a failure or a missing key waits for
    /// someone to look at it.
    var mayOpenInBackground: Bool {
        switch phase {
        case .starting, .locked(.keychainUnavailable): true
        default: false
        }
    }

    /// Where this device's push token goes, or `nil` while it has nowhere to
    /// go: no open vault, no relay, no account bearer, or no relay device id
    /// yet — the id is what the upload is signed as and filed under.
    func pushUploadTarget() -> PushUploadTarget? {
        guard phase == .unlocked, let bridge else { return nil }
        let settings = AppSettings(defaults: settingsDefaults)
        guard settings.syncIsConfigured, let bearer = account.accessToken else { return nil }
        let relayURL = settings.relayURL.trimmed
        guard case let .success(id?) = relayDeviceID(relayURL: relayURL, bearer: bearer) else {
            return nil
        }
        return PushUploadTarget(relayURL: relayURL, bearer: bearer, relayDeviceID: id) { token in
            try await bridge.registerPushToken(
                relayURL: relayURL,
                bearer: bearer,
                relayDeviceID: id,
                token: token
            )
        }
    }
}
