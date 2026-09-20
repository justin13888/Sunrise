import Foundation
import Security
import Testing

@testable import Sunrise

/// What the Account screen says about a sign-out the Keychain refused.
///
/// Split out of ``AccountModelTests`` rather than merged into it: that file
/// stands three lines under SwiftLint's `file_length` warning of 520, which
/// `swiftlint lint --strict` makes a red gate, and these cases are a subject of
/// their own — the value the view renders, rather than the state machine that
/// feeds it. `project.yml` globs `SunriseTests/`, so a new file here is picked
/// up by both the macOS and the iOS unit bundle with no project edit.
///
/// The helpers come from ``AccountModelTests``' file: `StubCredentialStore`,
/// `StubLoginDriver`, `StubLoginError` and `credentials(accessToken:)` are all
/// internal to this target.
@MainActor
struct AccountDisclosureTests {
    private func model(
        store: StubCredentialStore,
        driver: StubLoginDriver = StubLoginDriver(),
        opened: (@Sendable (URL) -> Void)? = nil
    ) -> AccountModel {
        AccountModel(store: store, makeDriver: { _, _ in driver }, openURL: opened ?? { _ in })
    }

    private func model(
        store: StubCredentialStore,
        makeDriver: @escaping @Sendable (String, String) -> any LoginDriver
    ) -> AccountModel {
        AccountModel(store: store, makeDriver: makeDriver, openURL: { _ in })
    }

    /// A Keychain that holds a credential and will not let go of it.
    private func lockedStore() -> StubCredentialStore {
        StubCredentialStore(value: credentials(accessToken: "access-old"), clearFailure: refusal)
    }

    private var refusal: KeychainError { .unexpected(errSecInteractionNotAllowed) }
    /// What the user is shown: the Keychain's own prose, not `unexpected(-25308)`.
    private var refusalText: String { refusal.localizedDescription }

    /// The pairing `AccountView` renders on, asserted on the rule itself.
    ///
    /// `signOutDisclosure` is one value and the view is a total switch over it,
    /// so this is the whole of the render decision: no ordering left in the
    /// view to get wrong, and no combination that produces two rows or none.
    /// The conjunction it encodes is load bearing rather than pedantic — a
    /// refusal alone does not imply the user is signed out, because
    /// `restore()` reloads the survivor and `publish()` moves to `.signedIn`
    /// without touching the residue. That pairing is real, and it is the one
    /// the screen must not carry the message under, since the message's own
    /// text asserts the user is signed out. Dropping the residue there instead
    /// would hide exactly the readmission the disclosure exists to report.
    @Test
    func theDisclosureIsPairedWithTheStatesWhoseTextItMatches() async {
        let store = lockedStore()
        let account = model(store: store, driver: StubLoginDriver(failure: StubLoginError()))
        account.restore()
        #expect(account.state == .signedIn(expiresAtMs: 4_000))
        #expect(account.signOutDisclosure == .none, "nothing to disclose yet")

        account.signOut()
        #expect(account.state == .signedOut)
        #expect(
            account.signOutDisclosure == .incomplete(refusalText),
            "shown: signed out, the token stayed, and it carries the Keychain's own words"
        )

        await account.signIn(
            issuer: "https://issuer.example",
            clientID: "client",
            deviceID: "abcd",
            nowMs: 0
        )
        #expect(account.state == .failed("the issuer refused"))
        #expect(
            account.signOutDisclosure == .incomplete(refusalText),
            "shown: the sign-in failed, the token stayed"
        )

        account.restore()
        #expect(account.state == .signedIn(expiresAtMs: 4_000))
        #expect(account.signOutIncomplete != nil, "the residue survives the readmission")
        #expect(
            account.signOutDisclosure == .none,
            "NOT shown: the message's text would now be false, and that arm has its own Sign out"
        )
    }

    /// Refused, acknowledged, unlocked, retried — the sequence the retry row's
    /// own **Sign out** exists for. Dismissing the message must not take the
    /// retry with it, since under `.signedOut` no other control reaches
    /// `signOut()`; and nothing else in the suite reaches the line in the
    /// do-branch that ends the residue once the credential is genuinely gone.
    @Test
    func dismissingKeepsTheRetryAndTheRetryRemovesTheCredential() {
        let store = lockedStore()
        let account = model(store: store)
        account.signOut()
        #expect(account.signOutDisclosure == .incomplete(refusalText))

        account.dismissSignOutIncomplete()
        #expect(
            account.signOutDisclosure == .retry,
            "the message is acknowledged; the way to act on it is not"
        )

        account.signOut()
        #expect(
            account.signOutDisclosure == .incomplete(refusalText),
            "still locked: the retry re-discloses"
        )

        store.stopRefusingClears()
        account.signOut()

        #expect(account.signOutDisclosure == .none, "the credential is gone, so the row is too")
        #expect(store.stored == nil, "gone from the Keychain, not just from the screen")
        #expect(store.clearCount == 3, "refused, retried under the lock, then the one that worked")
    }

    /// The retry has a dismissal of its own, and needs one.
    ///
    /// It renders for the rest of the process and is otherwise cleared only by
    /// a `clear()` or a `save()` that SUCCEEDS — and the user it exists for is
    /// precisely the one who cannot unlock the Keychain, for whom neither ever
    /// does. Without this the screen carries an alarming row that user can
    /// never clear, which is the failure this change rejected the other shape
    /// for. Retiring it does not un-refuse the Keychain: the credential is
    /// still there, and a sign-out refused again is a new fact that re-arms
    /// the disclosure from the top.
    @Test
    func theRetryCanBeRetiredAndAFreshRefusalReArmsIt() {
        let store = lockedStore()
        let account = model(store: store)
        account.signOut()
        account.dismissSignOutIncomplete()
        #expect(account.signOutDisclosure == .retry)

        account.dismissSignOutRetry()
        #expect(account.signOutDisclosure == .none, "told twice, and done being told")
        #expect(
            account.signOutRefusedThisSession,
            "but the refusal is not forgotten — the model still knows the credential stayed"
        )
        #expect(store.stored != nil, "because it did")

        account.signOut()
        #expect(
            account.signOutDisclosure == .incomplete(refusalText),
            "a second refusal is a new fact, not one the earlier dismissal covers"
        )
    }

    /// The retry stands where the message steps aside: under `.failed`, where
    /// the only other control is **Try again**, and not under `.signedIn`,
    /// whose own arm carries a **Sign out**.
    @Test
    func theRetryIsOfferedUnderAFailedSignInButNotALiveSession() async {
        let store = lockedStore()
        let account = model(store: store, driver: StubLoginDriver(failure: StubLoginError()))
        account.signOut()
        account.dismissSignOutIncomplete()
        #expect(account.signOutDisclosure == .retry, "signed out")

        await account.signIn(
            issuer: "https://issuer.example",
            clientID: "client",
            deviceID: "abcd",
            nowMs: 0
        )
        #expect(account.state == .failed("the issuer refused"))
        #expect(account.signOutDisclosure == .retry, "and still, under a sign-in that failed")

        account.restore()
        #expect(account.state == .signedIn(expiresAtMs: 4_000))
        #expect(account.signOutDisclosure == .none, "but not under a session that has its own")
    }

    /// The retry survives a login the user walks away from.
    ///
    /// `signIn()` parks `.awaitingBrowser` for `redirectTimeoutMs` — five
    /// minutes — behind a spinner, with no cancel and no other control that
    /// reaches `store.clear()`. Suppressing the retry there, as the message's
    /// own state set does, is five minutes with no way out. The probe runs
    /// from the `openURL` hook, which `signIn()` calls on the line after it
    /// assigns `.awaitingBrowser`, so it looks at the screen the user is
    /// looking at.
    @Test
    func theRetryIsOfferedWhileAnAbandonedLoginIsParked() async {
        let probe = ParkedProbe()
        let account = model(store: lockedStore(), opened: { _ in
            MainActor.assumeIsolated { probe.sample() }
        })
        probe.account = account
        account.signOut()
        account.dismissSignOutIncomplete()

        await account.signIn(
            issuer: "https://issuer.example",
            clientID: "client",
            deviceID: "abcd",
            nowMs: 0
        )

        #expect(probe.state == .awaitingBrowser, "the probe ran with the browser open")
        #expect(probe.seen == .retry, "and the retry was on offer there")
    }

    /// The same window, with the message still unread: it steps aside for the
    /// browser, and rather than leaving the screen with nothing the retry
    /// takes its place. Dismissing that row retires the whole residue — the
    /// user has been told the fact the message carries.
    @Test
    func theMessageStandsAsideForTheBrowserAndTheRetryTakesItsPlace() async {
        let probe = ParkedProbe()
        let account = model(store: lockedStore(), opened: { _ in
            MainActor.assumeIsolated { probe.sample() }
        })
        probe.account = account
        account.signOut()
        #expect(account.signOutDisclosure == .incomplete(refusalText))

        await account.signIn(
            issuer: "https://issuer.example",
            clientID: "client",
            deviceID: "abcd",
            nowMs: 0
        )

        #expect(probe.state == .awaitingBrowser)
        #expect(probe.seen == .retry, "the alarming row waits; the control does not")

        account.signOut()
        account.dismissSignOutRetry()
        #expect(account.signOutDisclosure == .none, "dismissing the retry retires the message too")
    }

    /// A renewal is a `save()`, and ``AccountModel/signOutResidue`` states the
    /// invariant that it stands only "until a `clear()` or a `save()` replaces
    /// it". `refreshIfNeeded` used to replace the credential and retire
    /// nothing, so the screen went on offering a retry that named a credential
    /// the renewal had already overwritten.
    ///
    /// Latent while nothing ticks the renewal — but reachable today without
    /// one, because `restore()` runs from a per-window `.task` against the
    /// SHARED model, so opening a second window re-enters `.signedIn` over a
    /// residue that is still standing.
    @Test
    func aRenewalThatSavesRetiresTheResidue() async {
        let store = lockedStore()
        let renewed = credentials(accessToken: "access-new", expiresAtMs: 9_000, renewAtMs: 8_000)
        let account = model(store: store, makeDriver: { _, _ in
            RenewingButNotSigningInDriver(renewed: renewed)
        })
        account.signOut()
        #expect(account.signOutDisclosure == .incomplete(refusalText))

        account.restore()
        #expect(account.state == .signedIn(expiresAtMs: 4_000), "a second window reloads it")
        #expect(account.signOutRefusedThisSession, "and the residue stands behind the session")

        await account.refreshIfNeeded(issuer: "https://issuer.example", clientID: "c", nowMs: 3_000)
        #expect(store.stored?.accessToken == "access-new", "the renewal replaced the credential")
        #expect(!account.signOutRefusedThisSession, "so the residue about the old one is spent")

        await account.signIn(
            issuer: "https://issuer.example",
            clientID: "client",
            deviceID: "abcd",
            nowMs: 0
        )
        #expect(account.state == .failed("the issuer refused"))
        #expect(
            account.signOutDisclosure == .none,
            "and no retry naming a credential the renewal already overwrote"
        )
    }

    /// What the disclosure NAMES, pinned.
    ///
    /// It used to say "the refresh token", which ``StoredCredentials`` is
    /// allowed a `nil` one for: on a public client whose issuer returns none,
    /// that copy named a token the user never had — in copy whose whole
    /// purpose is precision about what survived. Nothing else in either suite
    /// reads this text, because the rows' bodies are never evaluated here,
    /// which is why the strings live outside them.
    @Test
    func theDisclosureNamesTheStoredCredentialAndNotARefreshToken() {
        let headline = SignOutCopy.incompleteHeadline(message: "the Keychain is locked")

        #expect(headline.contains("the stored credential"))
        #expect(headline.contains("the Keychain is locked"), "the Keychain's own words, quoted")
        #expect(SignOutCopy.incompleteCaption.contains("The stored credential is still in the Keychain"))
        #expect(SignOutCopy.incompleteCaption.contains("Unlock your Keychain, then Sign out here"))
        #expect(SignOutCopy.retryCaption.contains("A credential the last sign-out could not remove"))
        #expect(SignOutCopy.retryCaption.contains("Unlock your Keychain, then Sign out here"))

        for text in [headline, SignOutCopy.incompleteCaption, SignOutCopy.retryCaption] {
            #expect(
                !text.lowercased().contains("refresh token"),
                "a public client may never have had one, so the copy must not name it"
            )
        }
    }
}

/// Renews, but will not start a login.
///
/// `StubLoginDriver`'s one `failure` knob short-circuits all three calls, so a
/// renewal followed by a sign-in that fails — the sequence a stale residue
/// surfaces in — is inexpressible through it.
struct RenewingButNotSigningInDriver: LoginDriver {
    let renewed: StoredCredentials

    func begin(deviceID: String) async throws -> URL { throw StubLoginError() }

    func complete(timeoutMs: UInt64, nowMs: UInt64) async throws -> StoredCredentials {
        throw StubLoginError()
    }

    func refresh(refreshToken: String, nowMs: UInt64) async throws -> StoredCredentials { renewed }
}

/// Looks at the model from inside `signIn()`'s `openURL` hook, which is the
/// one moment a test can observe `.awaitingBrowser` — the state is
/// `private(set)` and the call that leaves it returns before the `await` does.
@MainActor
final class ParkedProbe {
    var account: AccountModel?
    private(set) var state: AccountModel.State?
    private(set) var seen: AccountModel.SignOutDisclosure?

    func sample() {
        state = account?.state
        seen = account?.signOutDisclosure
    }
}
