import Foundation
import Security
import Testing

@testable import Sunrise

/// What a login or a renewal that a sign-out has already ended must **not**
/// touch on its way back in.
///
/// Split out rather than merged into either sibling for the reason
/// ``AccountDisclosureTests`` states about itself: `AccountModelTests.swift`
/// stands at 470 of SwiftLint's `file_length` warning of 520 and
/// `AccountDisclosureTests.swift` at 512, `swiftlint lint --strict` makes that
/// warning a red gate, and these cases will not fit in either. They are also a
/// subject of their own — not the state machine, and not the value the view
/// renders, but the *discard* path both of those reach through: the one that
/// runs when ``AccountModel/sessionGeneration`` has moved since the suspension
/// began. `project.yml` globs `SunriseTests/`, so a new file here joins both
/// the macOS and the iOS unit bundle with no project edit.
///
/// The helpers come from ``AccountModelTests``' file — `StubCredentialStore`,
/// `StubLoginDriver`, `StubLoginError`, `OpenedURLs` and
/// `credentials(accessToken:)` — and `ParkedProbe` and `AbandonedLoginDriver`
/// from ``AccountDisclosureTests``' file; all are internal to this target.
@MainActor
struct AccountDiscardTests {
    private func model(
        store: StubCredentialStore,
        driver: StubLoginDriver = StubLoginDriver(),
        opened: (@Sendable (URL) -> Void)? = nil
    ) -> AccountModel {
        AccountModel(store: store, makeDriver: { _, _ in driver }, openURL: opened ?? { _ in })
    }

    private func model(
        store: StubCredentialStore,
        makeDriver: @escaping @Sendable (String, String) -> any LoginDriver,
        opened: (@Sendable (URL) -> Void)? = nil
    ) -> AccountModel {
        AccountModel(store: store, makeDriver: makeDriver, openURL: opened ?? { _ in })
    }

    /// The one sign-in every case here drives; only the driver differs.
    private func signIn(_ account: AccountModel) async {
        await account.signIn(
            issuer: "https://issuer.example",
            clientID: "client",
            deviceID: "abcd",
            nowMs: 0
        )
    }

    /// A Keychain that holds a credential and will not let go of it.
    private func lockedStore() -> StubCredentialStore {
        StubCredentialStore(value: credentials(accessToken: "access-old"), clearFailure: refusal)
    }

    private var refusal: KeychainError { .unexpected(errSecInteractionNotAllowed) }
    /// What the user is shown: the Keychain's own prose, not `unexpected(-25308)`.
    private var refusalText: String { refusal.localizedDescription }

    /// A sign-out landing in the OTHER suspension: `driver.begin`, before
    /// `.awaitingBrowser` has been written at all.
    ///
    /// `begin` is a discovery round trip and an authorize round trip with no
    /// client-side timeout in this seam, and the retry row's **Sign out** is on
    /// screen for both. There the write order is inverted from the `complete`
    /// window the sibling cases cover — the sign-out reaches `.signedOut`
    /// first — so an unguarded `state = .awaitingBrowser` clobbers it and the
    /// guard after `complete` then returns into a screen that is a spinner and
    /// a line of text, with no control on it that can move the state again.
    ///
    /// This is also the only case in any of the three files that pins WHERE
    /// the generation is captured. Every other interleaving arrives through the
    /// `openURL` hook, which runs strictly after the capture, so moving
    /// `let generation = sessionGeneration` below `begin` leaves all of them
    /// green while making the whole `begin` window unguarded.
    @Test
    func aSignOutDuringTheBrowserHandoffIsNotClobberedByTheLogin() async {
        let probe = ParkedProbe()
        let store = lockedStore()
        let opened = OpenedURLs()
        let account = model(
            store: store,
            makeDriver: { _, _ in InterruptedBeginDriver(interrupt: { probe.sample() }) },
            opened: { opened.record($0) }
        )
        probe.account = account
        probe.act = { parked in
            store.stopRefusingClears()
            parked.signOut()
        }
        account.signOut()
        account.dismissSignOutIncomplete()
        #expect(account.signOutDisclosure == .retry, "the row whose Sign out this presses")

        await signIn(account)

        #expect(probe.seen == .retry, "the sign-out ran from a control that was really on screen")
        #expect(probe.saidAfter(.none), "and it worked — the Keychain let go this time")
        #expect(account.state == .signedOut, "so the login does not write its spinner over that")
        #expect(opened.first == nil, "and opens no browser for a login the user has already ended")
        #expect(store.stored == nil, "the late token is discarded, not written over the removal")
        #expect(account.accessToken == nil)
    }

    /// The `catch` is on the discard path too, and it is the slowest arm of it.
    ///
    /// The user signs out under the open browser and the screen returns to
    /// **Sign in…**. Five minutes later the tab they abandoned times out on
    /// ``AccountModel/redirectTimeoutMs`` and, unguarded, the `catch` writes
    /// `.failed` over the `.signedOut` they asked for: the Account screen
    /// changes by itself into an error about a login they deliberately walked
    /// away from.
    @Test
    func anAbandonedLoginTimingOutDoesNotReportItselfOverTheSignOut() async {
        let probe = ParkedProbe()
        let store = lockedStore()
        let account = model(
            store: store,
            makeDriver: { _, _ in AbandonedLoginDriver() },
            opened: { _ in MainActor.assumeIsolated { probe.sample() } }
        )
        probe.account = account
        probe.act = { parked in
            store.stopRefusingClears()
            parked.signOut()
        }
        account.signOut()
        account.dismissSignOutIncomplete()

        await signIn(account)

        #expect(probe.state == .awaitingBrowser, "the sign-out ran inside the suspension")
        #expect(
            account.state == .signedOut,
            "and the timeout five minutes later is not news about a session that has ended"
        )
        #expect(account.accessToken == nil)
        #expect(store.stored == nil, "the Keychain let go when they asked, and stays empty")
    }

    /// The other `catch`, where the same shape is worse in kind: it does not
    /// only write, it calls ``AccountModel/signOut()`` a second time.
    ///
    /// `hasExpired` is tested against the credential captured at entry, which a
    /// concurrent sign-out has already invalidated. Unguarded, a renewal that
    /// fails on the network after that sign-out runs `store.clear()` again —
    /// nobody asked for it, and under a lock that refuses it the refusal is
    /// fresh, re-arming from the top a disclosure the user may have retired,
    /// from a background event with no user action behind it.
    @Test
    func aRenewalFailingAfterASignOutDoesNotSignTheUserOutAgain() async {
        let probe = ParkedProbe()
        let store = lockedStore()
        let account = model(store: store, makeDriver: { _, _ in
            InterruptedFailingRenewalDriver(interrupt: { probe.sample() })
        })
        probe.account = account
        probe.act = { $0.signOut() }
        account.signOut()
        account.dismissSignOutIncomplete()
        account.dismissSignOutRetry()
        #expect(account.signOutDisclosure == .none, "told twice, and done being told")

        account.restore()
        #expect(account.state == .signedIn(expiresAtMs: 4_000), "a second window reloads it")

        await account.refreshIfNeeded(issuer: "https://issuer.example", clientID: "c", nowMs: 4_001)

        #expect(probe.state == .signedIn(expiresAtMs: 4_000), "the sign-out ran mid-renewal")
        #expect(
            store.clearCount == 2,
            "the user's two sign-outs, and no third one from a renewal failing behind the screen"
        )
        #expect(
            account.state == .signedOut,
            "so the screen stays where the sign-out put it, not on the renewal's network error"
        )
        #expect(
            account.signOutDisclosure == .incomplete(refusalText),
            "and what it discloses is the user's own refusal, not one the app went and provoked"
        )
    }
}

/// Lets the caller act on the model while `begin` is still in flight.
///
/// That is the suspension before `.awaitingBrowser` is written, which
/// ``ParkedProbe``'s `openURL` hook runs strictly after and so cannot reach.
/// `begin` is `nonisolated async`, so it hops to the main actor explicitly,
/// exactly as ``InterruptedRenewalDriver`` does for `refresh`.
struct InterruptedBeginDriver: LoginDriver {
    let interrupt: @MainActor @Sendable () -> Void
    /// Everything but the interruption, borrowed rather than restated.
    var inner = StubLoginDriver()

    func begin(deviceID: String) async throws -> URL {
        await MainActor.run { interrupt() }
        return try await inner.begin(deviceID: deviceID)
    }

    func complete(timeoutMs: UInt64, nowMs: UInt64) async throws -> StoredCredentials {
        try await inner.complete(timeoutMs: timeoutMs, nowMs: nowMs)
    }

    func refresh(refreshToken: String, nowMs: UInt64) async throws -> StoredCredentials {
        try await inner.refresh(refreshToken: refreshToken, nowMs: nowMs)
    }
}

/// A renewal the caller can act on, which then fails — the network blink
/// ``AccountModel/refreshIfNeeded(issuer:clientID:nowMs:)``'s `catch` exists
/// for, arriving after a sign-out rather than instead of one.
///
/// ``InterruptedRenewalDriver`` succeeds, so it reaches the guard on the save
/// and never the one in the `catch`.
struct InterruptedFailingRenewalDriver: LoginDriver {
    let interrupt: @MainActor @Sendable () -> Void

    func begin(deviceID: String) async throws -> URL { throw StubLoginError() }

    func complete(timeoutMs: UInt64, nowMs: UInt64) async throws -> StoredCredentials {
        throw StubLoginError()
    }

    func refresh(refreshToken: String, nowMs: UInt64) async throws -> StoredCredentials {
        await MainActor.run { interrupt() }
        throw StubLoginError()
    }
}
