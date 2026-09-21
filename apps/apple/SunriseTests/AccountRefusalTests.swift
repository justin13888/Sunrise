import Foundation
import Security
import Testing

@testable import Sunrise

/// What a Keychain that **refuses to answer** costs, and what the Account
/// screen's one remaining control is allowed to do about it.
///
/// Split out of `AccountModelTests.swift` for the reason its two siblings
/// state about themselves, and this time the merge is what forced it: the
/// refused-sign-out cases and these refused-*read* cases landed on that file
/// from different changes and together carried it to 620 lines, and its type
/// body to 358, against SwiftLint's `file_length` warning of 520 and
/// `type_body_length` warning of 320 — both of which `swiftlint lint --strict`
/// makes errors. They are a subject of their own in any case: not what a
/// sign-out leaves behind, but what a read that never happened means for the
/// **Sign in…** the user is offered next.
///
/// The helpers come from ``AccountModelTests``' file — `StubCredentialStore`,
/// `StubLoginDriver`, `OpenedURLs` and `credentials(accessToken:)` — and
/// `ParkedProbe` from ``AccountDisclosureTests``' file; all are internal to
/// this target. `project.yml` globs `SunriseTests/`, so a new file here joins
/// both the macOS and the iOS unit bundle with no project edit.
@MainActor
struct AccountRefusalTests {
    /// The refusal the repair is about: the *other* keychain was reached and
    /// would not answer, so whether a token is in it is unknown.
    private static let refusal = KeychainError.otherDomainUnreadable(errSecInteractionNotAllowed)

    /// The other shape `restore()` can be handed, and the one that behaves
    /// oppositely: both copies were read, they disagree, and a sign-in is the
    /// repair rather than the harm.
    private static let unverified = KeychainError.migrationUnverified

    private func model(
        store: StubCredentialStore,
        driver: StubLoginDriver = StubLoginDriver(),
        opened: (@Sendable (URL) -> Void)? = nil
    ) -> AccountModel {
        AccountModel(
            store: store,
            makeDriver: { _, _ in driver },
            openURL: opened ?? { _ in }
        )
    }

    /// A store that *refuses* is not a store that answered nothing, and this
    /// is the difference the whole cross-domain read exists to keep. Reading a
    /// refusal as "signed out" offers Sign in, and signing in writes a second
    /// token beside the one that may still be sitting in the unreadable
    /// keychain — two secrets under one `(service, account)`, which is
    /// `migrationUnverified` on every later launch.
    @Test
    func aRefusedReadIsReportedInsteadOfLookingLikeASignedOutSession() {
        let store = StubCredentialStore(loadError: Self.refusal)
        let account = model(store: store)

        account.restore()

        #expect(account.state == .failed(Self.refusal.localizedDescription))
        #expect(account.state != .signedOut, "a refusal that renders as Sign in invites the second token")
        #expect(account.accessToken == nil)
    }

    /// And the Try again that state offers has to mean "look again". The view
    /// cannot know what the store answered, so the model is the only place the
    /// second token can be stopped: a store that has started answering hands
    /// back the session it was holding, without a browser and without writing
    /// over it.
    @Test
    func tryingAgainAfterARefusalLooksAgainRatherThanSigningInAfresh() async {
        let store = StubCredentialStore(
            value: credentials(accessToken: "access-old"),
            loadError: Self.refusal
        )
        let opened = OpenedURLs()
        let account = model(store: store, opened: { opened.record($0) })
        account.restore()
        #expect(account.state == .failed(Self.refusal.localizedDescription))

        store.loadError = nil
        await account.signIn(
            issuer: "https://issuer.example",
            clientID: "client",
            deviceID: "abcd",
            nowMs: 0
        )

        #expect(account.state == .signedIn(expiresAtMs: 4_000))
        #expect(account.accessToken == "access-old")
        #expect(store.stored?.accessToken == "access-old", "a second token was written over the first")
        #expect(opened.first == nil, "the refusal was spent on a login rather than a second look")
    }

    /// The other half of the same guard: the refusal is a reason to look
    /// again, not a latch. Once the store answers and there is genuinely
    /// nothing in it, signing in is safe and has to happen.
    @Test
    func aStoreThatAnswersNothingAfterARefusalStillReachesTheLogin() async {
        let store = StubCredentialStore(loadError: Self.refusal)
        let account = model(store: store)
        account.restore()

        store.loadError = nil
        await account.signIn(
            issuer: "https://issuer.example",
            clientID: "client",
            deviceID: "abcd",
            nowMs: 0
        )

        #expect(account.state == .signedIn(expiresAtMs: 4_000))
        #expect(store.stored?.accessToken == "access-1")
    }

    /// The branch the three cases above do not reach, and the one the whole
    /// guard exists for: the store is **still** refusing on the second look, so
    /// the login must not be reached. Delete the `!storeRefusedToAnswer` half of
    /// `signIn`'s guard and all three stay green — A never calls `signIn`, B
    /// returns because a token came back, C returns because the store answered
    /// nothing. This is the case that reddens.
    @Test
    func aStoreStillRefusingOnTheSecondLookDoesNotReachTheLogin() async {
        let store = StubCredentialStore(loadError: Self.refusal)
        let opened = OpenedURLs()
        let account = model(store: store, opened: { opened.record($0) })
        account.restore()

        await account.signIn(
            issuer: "https://issuer.example",
            clientID: "client",
            deviceID: "abcd",
            nowMs: 0
        )

        #expect(opened.first == nil, "a store still refusing was spent on a login")
        #expect(store.stored == nil, "a second token was written beside an unread copy")
        #expect(account.state == .failed(Self.refusal.localizedDescription))
        #expect(account.accessToken == nil)
    }

    /// And the refusal that must **not** latch. `migrationUnverified` is the one
    /// shape where both copies have been read and are known to disagree, so no
    /// copy survives unread and there is no second token to invite: a sign-in's
    /// `writeAcrossDomains` writes this domain and deletes the other, which is
    /// the collapse that ends the state. Gating the second look on any throw
    /// made this a dead end — every later Try again re-threw and returned.
    @Test
    func anUnverifiedMigrationFallsThroughToTheLoginThatCollapsesIt() async {
        let store = StubCredentialStore(loadError: Self.unverified)
        let opened = OpenedURLs()
        let account = model(store: store, opened: { opened.record($0) })
        account.restore()
        #expect(account.state == .failed(Self.unverified.localizedDescription))

        await account.signIn(
            issuer: "https://issuer.example",
            clientID: "client",
            deviceID: "abcd",
            nowMs: 0
        )

        #expect(opened.first != nil, "the one refusal a sign-in repairs was refused one")
        #expect(store.stored?.accessToken == "access-1")
        #expect(account.accessToken == "access-1")
        #expect(account.state == .signedIn(expiresAtMs: 4_000))
    }
    /// The two guards in ``AccountModel/signIn(issuer:clientID:deviceID:nowMs:)``
    /// meet here, and this is the only case that puts both of them in one run.
    ///
    /// They guard different things and are checked at different times. The
    /// second look above turns on the **shape** of the last refusal and runs
    /// synchronously, before the first suspension; the generation guard turns
    /// on a counter captured across a suspension and runs after one. Because
    /// they are sequenced rather than nested, a login that the second look lets
    /// through is an ordinary login from that point on — and has to be just as
    /// discardable as one that never met a refusal at all.
    ///
    /// Nothing else puts the two together. Of the five cases above, three
    /// return inside the prologue and so never reach a suspension at all, and
    /// the two that do reach the login — the one where the store answers
    /// nothing and the `migrationUnverified` one, which skips the prologue
    /// because that shape never sets the flag — run to a completed login with
    /// no sign-out anywhere near them, so no generation guard is ever the
    /// thing that decides. From the other side, no case in
    /// ``AccountDiscardTests`` passes a `loadError`, so `storeRefusedToAnswer`
    /// is false throughout all of them and the prologue never runs. Until this
    /// case, a login admitted by the second look had never been discarded.
    ///
    /// **What it proves is composition, not placement, and the difference is
    /// worth stating.** Moving `let generation = sessionGeneration` above the
    /// prologue leaves this green — `restore()` cannot bump the counter, so the
    /// captured value is the same either way — and so does a prologue that
    /// reset it, for the same reason. Where the capture sits is pinned by
    /// ``AccountDiscardTests/aSignOutDuringTheBrowserHandoffIsNotClobberedByTheLogin``
    /// and by nothing here. What reddens this case is either guard being made
    /// to disable the other: a second look that consumed the discard, or a
    /// discard that stopped firing for a login the second look admitted.
    ///
    /// The `.awaitingBrowser` the probe sees is what proves the prologue let
    /// this login through rather than returning at it: a store still refusing
    /// never opens a browser, which is the case directly above.
    @Test
    func aLoginReachedThroughTheSecondLookIsStillDiscardedByASignOut() async {
        let probe = ParkedProbe()
        let store = StubCredentialStore(loadError: Self.refusal)
        let account = model(store: store, opened: { _ in
            MainActor.assumeIsolated { probe.sample() }
        })
        probe.account = account
        probe.act = { $0.signOut() }
        account.restore()
        #expect(account.state == .failed(Self.refusal.localizedDescription))

        // The user unlocks the Keychain and presses Sign in. The second look
        // now answers, and answers nothing, so the login is reached.
        store.loadError = nil
        await account.signIn(
            issuer: "https://issuer.example",
            clientID: "client",
            deviceID: "abcd",
            nowMs: 0
        )

        #expect(probe.state == .awaitingBrowser, "the second look did not let the login through")
        #expect(account.state == .signedOut, "the sign-out taken under the browser stands")
        #expect(store.stored == nil, "the late token was written over the removal")
        #expect(account.accessToken == nil)
    }
}
