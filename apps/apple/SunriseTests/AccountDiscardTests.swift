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
