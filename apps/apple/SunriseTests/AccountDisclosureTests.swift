import Foundation
import Security
import Testing

@testable import Sunrise

/// What the Account screen says about a sign-out the Keychain refused.
///
/// Split out of ``AccountModelTests`` rather than merged into it: that file
/// was three lines under SwiftLint's `file_length` warning of 520 when these
/// cases were written, which `swiftlint lint --strict` makes a red gate, and
/// these cases are a subject of their own — the value the view renders, not
/// the state machine that feeds it. `project.yml` globs `SunriseTests/`, so a
/// file here joins both the macOS and the iOS unit bundle with no project edit.
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
        makeDriver: @escaping @Sendable (String, String) -> any LoginDriver,
        opened: (@Sendable (URL) -> Void)? = nil
    ) -> AccountModel {
        AccountModel(store: store, makeDriver: makeDriver, openURL: opened ?? { _ in })
    }

    /// The one sign-in every case in this file drives; only the driver differs.
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

        await signIn(account)
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

        await signIn(account)
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
        let store = lockedStore()
        let account = model(
            store: store,
            makeDriver: { _, _ in AbandonedLoginDriver() },
            opened: { _ in MainActor.assumeIsolated { probe.sample() } }
        )
        probe.account = account
        account.signOut()
        account.dismissSignOutIncomplete()

        await signIn(account)

        #expect(probe.state == .awaitingBrowser, "the probe ran with the browser open")
        #expect(probe.seen == .retry, "and the retry was on offer there")
        // The end state, which a driver that completed instantly hid: the
        // login really is abandoned, so it writes nothing, and the residue and
        // the credential it names are exactly where the sign-out left them.
        #expect(account.state == .failed("the redirect never came"))
        #expect(store.stored?.accessToken == "access-old", "no save, so no new credential")
        #expect(account.signOutDisclosure == .retry, "and the retry still stands under it")
    }

    /// The same window, with the message still unread: it steps aside for the
    /// browser, and rather than leaving the screen with nothing the retry
    /// takes its place. Dismissing that row retires the whole residue — the
    /// user has been told the fact the message carries.
    ///
    /// The dismissal has to happen HERE, on the parked screen:
    /// `.awaitingBrowser` is the only state a user can reach
    /// ``AccountModel/dismissSignOutRetry()`` from while the message is still
    /// unread, because everywhere else an unread residue renders the OTHER
    /// row, whose **Dismiss** calls ``AccountModel/dismissSignOutIncomplete()``.
    @Test
    func theMessageStandsAsideForTheBrowserAndTheRetryTakesItsPlace() async {
        let probe = ParkedProbe()
        let store = lockedStore()
        let account = model(
            store: store,
            makeDriver: { _, _ in AbandonedLoginDriver() },
            opened: { _ in MainActor.assumeIsolated { probe.sample() } }
        )
        probe.account = account
        probe.act = { $0.dismissSignOutRetry() }
        account.signOut()
        #expect(account.signOutDisclosure == .incomplete(refusalText))

        await signIn(account)

        #expect(probe.state == .awaitingBrowser)
        #expect(probe.seen == .retry, "the alarming row waits; the control does not")
        #expect(probe.saidAfter(.none), "and its Dismiss retires the message it stood in for")
        #expect(
            probe.offeredBareAfter == false,
            "and no bare Sign out takes its place mid-login — there is a browser in front of them"
        )
        #expect(
            account.signOutRefusedThisSession,
            "retired, not un-refused — the credential it was about is still stored"
        )
        #expect(store.stored != nil, "because it is")
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

        await signIn(account)
        #expect(account.state == .failed("the issuer refused"))
        #expect(
            account.signOutDisclosure == .none,
            "and no retry naming a credential the renewal already overwrote"
        )
    }

    /// The way out of a refusal the user has retired.
    ///
    /// After both dismissals the screen says nothing — by the user's own
    /// request, twice — but the credential is still in the Keychain and the
    /// next launch reads it back. Under `.signedOut` and `.failed` nothing
    /// else on the screen reaches `store.clear()`: **Sign in…** needs a
    /// `save()` the lock that refused the `clear()` refuses too. That is the
    /// end state this change exists to remove, reached here by consent.
    @Test
    func aRetiredRefusalStillOffersAPlainSignOut() async {
        let store = lockedStore()
        let account = model(store: store, driver: StubLoginDriver(failure: StubLoginError()))
        account.signOut()
        #expect(!account.offersBareSignOut, "the message is on screen and carries its own")

        account.dismissSignOutIncomplete()
        #expect(!account.offersBareSignOut, "and so does the retry")

        account.dismissSignOutRetry()
        #expect(account.signOutDisclosure == .none, "nothing left to say")
        #expect(account.offersBareSignOut, "but still something to do about it")

        await signIn(account)
        #expect(account.state == .failed("the issuer refused"))
        #expect(account.offersBareSignOut, "and under a sign-in that failed, for the same reason")

        account.restore()
        #expect(account.state == .signedIn(expiresAtMs: 4_000))
        #expect(!account.offersBareSignOut, "not under a session whose own arm carries one")

        store.stopRefusingClears()
        account.signOut()
        #expect(store.stored == nil, "pressing it is what finally removes the credential")
        #expect(!account.offersBareSignOut, "so it goes — unlike the row it stands in for")
    }

    /// A sign-out taken while the browser is still open is not undone by the
    /// login it interrupted.
    ///
    /// `.awaitingBrowser` is not an idle state: it is `signIn()` suspended at
    /// `driver.complete(...)`, and the retry row renders throughout it. The
    /// user unlocks the Keychain, presses that row's **Sign out**, and the
    /// credential is genuinely removed — then minutes later the tab they left
    /// open completes the authorization. Without the generation check the
    /// login's own `store.save` puts a fresh credential back and `publish()`
    /// signs them in, silently: the harm the disclosure exists to report,
    /// produced by the control that reports it.
    @Test
    func aSignOutWhileTheBrowserIsOpenDiscardsTheLoginThatLandsLate() async {
        let probe = ParkedProbe()
        let store = lockedStore()
        let account = model(store: store, opened: { _ in
            MainActor.assumeIsolated { probe.sample() }
        })
        probe.account = account
        probe.act = { parked in
            store.stopRefusingClears()
            parked.signOut()
        }
        account.signOut()
        account.dismissSignOutIncomplete()
        #expect(account.signOutDisclosure == .retry, "the row whose Sign out this presses")

        await signIn(account)

        #expect(probe.state == .awaitingBrowser, "the sign-out ran inside the suspension")
        #expect(probe.saidAfter(.none), "and it worked — the Keychain let go this time")
        #expect(store.stored == nil, "the late token is discarded, not written over the removal")
        #expect(account.state == .signedOut, "the user asked to be signed out, and still is")
        #expect(account.accessToken == nil, "so there is no bearer for the relay or the sync bridge")
        #expect(!account.signOutRefusedThisSession, "and nothing left behind to disclose")
    }

    /// The same rule on the other `store.save` in the type.
    ///
    /// A renewal is silent by design — the screen stays `.signedIn` and its
    /// own **Sign out** is live for the whole of it — so a sign-out lands
    /// inside `refreshIfNeeded`'s suspension just as readily, and the renewed
    /// token would be written over the removal the user just asked for.
    @Test
    func aSignOutDuringARenewalDiscardsTheTokenItWasRenewing() async {
        let probe = ParkedProbe()
        let store = StubCredentialStore(value: credentials(accessToken: "access-old"))
        let renewed = credentials(accessToken: "access-new", expiresAtMs: 9_000, renewAtMs: 8_000)
        let account = model(store: store, makeDriver: { _, _ in
            InterruptedRenewalDriver(renewed: renewed, interrupt: { probe.sample() })
        })
        probe.account = account
        probe.act = { $0.signOut() }
        account.restore()
        #expect(account.state == .signedIn(expiresAtMs: 4_000))

        await account.refreshIfNeeded(issuer: "https://issuer.example", clientID: "c", nowMs: 3_000)

        #expect(probe.state == .signedIn(expiresAtMs: 4_000), "the sign-out ran mid-renewal")
        #expect(store.stored == nil, "the renewed token is discarded, not saved over the removal")
        #expect(account.state == .signedOut)
        #expect(account.accessToken == nil)
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

/// A login the user walks away from: the browser opens, the redirect never
/// comes, and `complete` gives up on `redirectTimeoutMs`.
///
/// `StubLoginDriver`'s single `failure` knob throws from `begin` too, so a
/// driver built with it never reaches `.awaitingBrowser` at all — and one
/// built without it returns a credential at once, which is a login completed,
/// not one abandoned.
struct AbandonedLoginDriver: LoginDriver {
    /// Borrowed from ``StubLoginDriver``, so this holds no URL literal of its own.
    private let authorizeURL = StubLoginDriver().authorizeURL
    private let timedOut = AbandonedLoginError()

    func begin(deviceID: String) async throws -> URL { authorizeURL }
    func complete(timeoutMs: UInt64, nowMs: UInt64) async throws -> StoredCredentials { throw timedOut }
    func refresh(refreshToken: String, nowMs: UInt64) async throws -> StoredCredentials { throw timedOut }
}

struct AbandonedLoginError: Error, LocalizedError {
    let errorDescription: String? = "the redirect never came"
}

/// Looks at the model from inside `signIn()`'s `openURL` hook, which is the
/// one moment a test can observe `.awaitingBrowser` — the state is
/// `private(set)` and the call that leaves it returns before the `await` does.
///
/// It is also the one moment a test can ACT in: ``act`` runs on the parked
/// screen, where the user pressing the retry's buttons stands, and the two
/// `After` properties are what the screen said once they had.
@MainActor
final class ParkedProbe {
    var account: AccountModel?
    /// What the user does while the browser is open, if anything.
    var act: (@MainActor (AccountModel) -> Void)?
    private(set) var state: AccountModel.State?
    private(set) var seen: AccountModel.SignOutDisclosure?
    private(set) var seenAfter: AccountModel.SignOutDisclosure?
    private(set) var offeredBareAfter: Bool?

    /// What the screen said once ``act`` had run. A method rather than a bare
    /// comparison because `seenAfter == .none` reads as `Optional.none`, which
    /// is a different question and one the compiler warns about.
    func saidAfter(_ disclosure: AccountModel.SignOutDisclosure) -> Bool { seenAfter == disclosure }

    func sample() {
        state = account?.state
        seen = account?.signOutDisclosure
        guard let account, let act else { return }
        act(account)
        seenAfter = account.signOutDisclosure
        offeredBareAfter = account.offersBareSignOut
    }
}

/// Renews, and lets the caller act on the model while the renewal is still in
/// flight.
///
/// ``AccountModel/refreshIfNeeded(issuer:clientID:nowMs:)`` has no `openURL`
/// hook, so the interleaving ``ParkedProbe`` observes for `signIn()` has to
/// arrive through the driver instead. `refresh` is `nonisolated async`, so it
/// hops to the main actor explicitly rather than assuming it is already there.
struct InterruptedRenewalDriver: LoginDriver {
    let renewed: StoredCredentials
    let interrupt: @MainActor @Sendable () -> Void

    func begin(deviceID: String) async throws -> URL { throw StubLoginError() }
    func complete(timeoutMs: UInt64, nowMs: UInt64) async throws -> StoredCredentials {
        throw StubLoginError()
    }

    func refresh(refreshToken: String, nowMs: UInt64) async throws -> StoredCredentials {
        await MainActor.run { interrupt() }
        return renewed
    }
}
