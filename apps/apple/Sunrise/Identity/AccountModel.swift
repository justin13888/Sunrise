import Foundation

/// One OIDC login, as the app drives it.
///
/// A protocol over the generated `SunriseLogin` so the state machine around it
/// is testable without an issuer, a browser, or a loopback listener.
protocol LoginDriver: Sendable {
    /// Discover the provider and start a login; returns the URL to open.
    func begin(deviceID: String) async throws -> URL
    /// Wait for the browser redirect and exchange the code.
    func complete(timeoutMs: UInt64, nowMs: UInt64) async throws -> StoredCredentials
    /// Exchange a refresh token, with no user interaction.
    func refresh(refreshToken: String, nowMs: UInt64) async throws -> StoredCredentials
}

/// The real one.
struct OIDCLoginDriver: LoginDriver {
    private let login: SunriseLogin

    init(issuer: String, clientID: String) {
        login = SunriseLogin(issuer: issuer, clientId: clientID)
    }

    func begin(deviceID: String) async throws -> URL {
        let url = try await login.begin(deviceId: deviceID)
        guard let parsed = URL(string: url) else { throw AccountError.badAuthorizeURL(url) }
        return parsed
    }

    func complete(timeoutMs: UInt64, nowMs: UInt64) async throws -> StoredCredentials {
        StoredCredentials(try await login.complete(timeoutMs: timeoutMs, nowMs: nowMs))
    }

    func refresh(refreshToken: String, nowMs: UInt64) async throws -> StoredCredentials {
        StoredCredentials(try await login.refresh(refreshToken: refreshToken, nowMs: nowMs))
    }
}

enum AccountError: Error, Equatable, LocalizedError {
    case notConfigured
    case badAuthorizeURL(String)

    var errorDescription: String? {
        switch self {
        case .notConfigured:
            "Set an OIDC issuer and client ID before signing in."
        case .badAuthorizeURL:
            "The identity provider returned an address Sunrise could not open."
        }
    }
}

/// Whether this device holds a token the relay will accept.
///
/// Signed out is a first-class state, not an error: a self-host relay accepts
/// an unauthenticated upgrade, and the core works with no relay at all.
@MainActor
@Observable
final class AccountModel {
    enum State: Equatable {
        case signedOut
        /// The browser is open and the loopback listener is parked.
        case awaitingBrowser
        case signedIn(expiresAtMs: UInt64)
        case failed(String)
    }

    /// What a refused sign-out has left behind, and how far the user has got
    /// with it.
    ///
    /// One value rather than a flag per row. As two independent booleans the
    /// message and the retry were simultaneously true in the ordinary refused
    /// state, so nothing here said which of the two rows the screen should
    /// carry and the view had to invent the rule — off the model, where no
    /// test reaches it. Every combination that can exist is a case here, and
    /// no two of them are true at once.
    enum SignOutResidue: Equatable {
        /// The Keychain let go of the credential, or was never asked.
        case none
        /// It refused, and the message has not been acknowledged yet. The
        /// payload is the Keychain's own localized prose.
        case unread(String)
        /// The message was acknowledged. The credential it named is still
        /// stored, so the control that re-runs the removal stays.
        case acknowledged
        /// The retry was dismissed too, so the screen says nothing more about
        /// this refusal. A sign-out refused again starts the sequence over.
        case retired
    }

    /// What the Account screen renders about the last sign-out — the whole of
    /// it, as one value.
    ///
    /// The view switches over this and keeps no rule of its own, so it cannot
    /// render both rows, nor neither, nor the wrong one of the two when the
    /// conditions behind them overlap.
    enum SignOutDisclosure: Equatable {
        /// The screen says nothing about the last sign-out.
        case none
        /// The full disclosure, carrying the Keychain's own message.
        case incomplete(String)
        /// The message has been read; the credential it named is still
        /// stored, so the way to act on it is still offered.
        case retry
    }

    private(set) var state: State = .signedOut

    /// The bearer to present to the relay, or `nil` when signed out.
    private(set) var accessToken: String?

    /// What the last sign-out could **not** do, and how much of it the user
    /// has acknowledged.
    ///
    /// Anything but ``SignOutResidue/none`` means the local session ended and
    /// the stored credential did not: the token is still in the Keychain, and
    /// the next launch's ``restore()`` reads it back. Carried to the caller
    /// rather than logged, because a log line is not a disclosure — the rule
    /// `core.device.revoke_incomplete` states in
    /// `docs/10-cross-cutting/log-events.md`, and the one
    /// ``DeviceListModel/lastRevocation`` already follows in this app.
    ///
    /// Session scoped: it says what happened in this process, and the
    /// credential it refers to is still stored until a `clear()` or a
    /// `save()` replaces it. All three writes that replace it — ``signOut()``,
    /// ``signIn(issuer:clientID:deviceID:nowMs:)`` and
    /// ``refreshIfNeeded(issuer:clientID:nowMs:)`` — retire it. The two
    /// asynchronous ones replace nothing, and so retire nothing, when a
    /// sign-out has landed since they began: see ``sessionGeneration``.
    private(set) var signOutResidue: SignOutResidue = .none

    /// The message a refused sign-out left unread, or `nil` once it has been
    /// acknowledged, retired, or made untrue by a replaced credential.
    var signOutIncomplete: String? {
        if case let .unread(message) = signOutResidue { message } else { nil }
    }

    /// Whether the Keychain has refused a sign-out whose credential is still
    /// the stored one. Unlike ``signOutIncomplete`` this outlives
    /// ``dismissSignOutIncomplete()`` and ``dismissSignOutRetry()``:
    /// acknowledging a message does not remove the credential it is about.
    var signOutRefusedThisSession: Bool { signOutResidue != .none }

    /// What the Account screen renders about the last sign-out.
    ///
    /// Which of the cases wins is decided here rather than at the render site,
    /// so a change to it fails a test instead of only a screenshot. The
    /// message is suppressed under `.signedIn`, where its text would predict a
    /// re-admission that has already happened, and stands aside under
    /// `.awaitingBrowser` for the browser in front of it. The retry is
    /// suppressed under `.signedIn` alone, whose own arm carries a **Sign
    /// out**: an abandoned login parks `.awaitingBrowser` for
    /// ``redirectTimeoutMs`` with no cancel and no other control that reaches
    /// `store.clear()`.
    var signOutDisclosure: SignOutDisclosure {
        switch signOutResidue {
        case .none, .retired:
            return .none
        case let .unread(message) where stateTheMessageIsTrueIn:
            return .incomplete(message)
        case .unread, .acknowledged:
            return stateTheRetryIsUsefulIn ? .retry : .none
        }
    }

    /// Whether the screen should carry a plain **Sign out** of its own,
    /// alongside whatever ``signOutDisclosure`` says — which, where this is
    /// `true`, is nothing.
    ///
    /// ``dismissSignOutRetry()`` retires the disclosure, not the credential.
    /// Without this the retired state is the end state this whole change
    /// exists to remove: the token is still in the Keychain, the next launch
    /// reads it back, and no control on the Account screen reaches
    /// `store.clear()` — **Sign in…** needs a `save()` the same lock refuses,
    /// and the `.signedIn` arm's own **Sign out** is a state away. It is
    /// reached by consent here rather than by a dismissal that destroyed the
    /// only retry, which is a real difference and not a difference in end
    /// state. This is the control that keeps it from being a trap.
    ///
    /// It carries no warning text, because the user has said twice that they
    /// do not want to be told again; and unlike the row it replaces it does
    /// clear, because a sign-out that succeeds sets the residue to
    /// ``SignOutResidue/none`` and this with it.
    var offersBareSignOut: Bool {
        guard signOutRefusedThisSession, signOutDisclosure == .none else { return false }
        switch state {
        // The two settled states. Not `.signedIn`, whose arm has a **Sign
        // out** already, and not `.awaitingBrowser`, where a login is in front
        // of the user and a retired residue is the one thing they asked to
        // stop hearing about.
        case .signedOut, .failed: return true
        case .signedIn, .awaitingBrowser: return false
        }
    }

    /// The states the message's own text is true and wanted in.
    private var stateTheMessageIsTrueIn: Bool {
        switch state {
        case .signedOut, .failed: true
        case .signedIn, .awaitingBrowser: false
        }
    }

    /// The states a retry is worth offering in: every one where the Account
    /// screen has no other control that reaches ``signOut()``.
    private var stateTheRetryIsUsefulIn: Bool {
        switch state {
        case .signedOut, .failed, .awaitingBrowser: true
        case .signedIn: false
        }
    }

    private let store: any CredentialStore
    private let makeDriver: @Sendable (String, String) -> any LoginDriver
    private let openURL: @Sendable (URL) -> Void
    private var credentials: StoredCredentials?

    /// How many times the session has been ended since this model was made.
    ///
    /// A login is not atomic, and it suspends twice.
    /// ``signIn(issuer:clientID:deviceID:nowMs:)`` waits at
    /// `driver.begin(...)` for a discovery and an authorize round trip, and
    /// again at `driver.complete(...)` for up to ``redirectTimeoutMs`` with
    /// the browser in front of the user; the Account screen offers a **Sign
    /// out** across both, the retry row under `.awaitingBrowser` and all three
    /// controls under the `.signedOut` or `.failed` that precedes it. Nothing
    /// cancels the login when it is pressed: the driver is a local of the
    /// suspended frame and the task that runs it is unstructured and
    /// unstored. So the counter is what tells the resumed login that the
    /// session it was establishing is not wanted any more — checked at each of
    /// the five points a resumed login or renewal would otherwise write, the
    /// transient state included, because a discarded login that has already
    /// written `.awaitingBrowser` leaves a screen with no control on it.
    ///
    /// Bumped in ``signOut()`` rather than at the button, so every caller is
    /// covered — the four on the Account screen, the two inside
    /// ``refreshIfNeeded(issuer:clientID:nowMs:)``, and any added later
    /// without this being noticed.
    private var sessionGeneration: UInt64 = 0

    /// How long to wait for the browser redirect. Long enough for a password
    /// manager and a second factor, short enough that an abandoned login does
    /// not hold a loopback port forever.
    private static let redirectTimeoutMs: UInt64 = 5 * 60 * 1000

    init(
        store: any CredentialStore = KeychainCredentialStore(),
        makeDriver: @escaping @Sendable (String, String) -> any LoginDriver = {
            OIDCLoginDriver(issuer: $0, clientID: $1)
        },
        // Hops to the main actor rather than calling straight through.
        // `NSWorkspace.open` is nonisolated, but `UIApplication.open` is not,
        // so the shared spelling has to be `@MainActor` — and this closure is
        // called from wherever the login driver happens to be.
        openURL: @escaping @Sendable (URL) -> Void = { url in
            _Concurrency.Task { @MainActor in Platform.openExternal(url) }
        }
    ) {
        self.store = store
        self.makeDriver = makeDriver
        self.openURL = openURL
    }

    /// Restore a previous session, if there is one.
    func restore() {
        credentials = try? store.load()
        publish()
    }

    /// Run a login. `deviceID` binds the token to this device: the issuer
    /// stamps it into a claim and the relay refuses a token whose claim names
    /// a different device, so one lifted off this Mac is useless elsewhere.
    func signIn(issuer: String, clientID: String, deviceID: String, nowMs: UInt64) async {
        guard !issuer.trimmed.isEmpty, !clientID.trimmed.isEmpty else {
            state = .failed(AccountError.notConfigured.localizedDescription)
            return
        }
        let generation = sessionGeneration
        let driver = makeDriver(issuer.trimmed, clientID.trimmed)
        do {
            let url = try await driver.begin(deviceID: deviceID)
            // The same rule as the guard below, on the suspension before it —
            // and here the write order is inverted, which is why it needs its
            // own check rather than the one after `complete`. `begin` is a
            // discovery round trip and an authorize round trip, and every
            // control that reaches ``signOut()`` is on screen throughout both.
            // A sign-out that lands there has already written `.signedOut`;
            // assigning `.awaitingBrowser` over it would leave the guard below
            // returning into a spinner with no control on screen that can move
            // it. Return before the transient state and before the browser: a
            // login the user has already ended needs neither.
            guard sessionGeneration == generation else { return }
            state = .awaitingBrowser
            openURL(url)
            let fresh = try await driver.complete(
                timeoutMs: Self.redirectTimeoutMs,
                nowMs: nowMs
            )
            // A sign-out taken while this login was parked in the browser ends
            // this login too. ``signOut()`` has already removed the credential
            // and left the screen where the user asked for it; saving `fresh`
            // would put a new one back and `publish()` would sign them in
            // again — minutes later, with nothing on screen to say so, which
            // is the silent readmission the disclosure below exists to
            // prevent. Drop the token instead and touch nothing.
            guard sessionGeneration == generation else { return }
            try store.save(fresh)
            // A warning about the previous sign-out must not sit under a new
            // session — and this is the line that ends it: `save` has just
            // overwritten the credential the warning is about. Clearing on
            // entry instead would also fire on the `guard` above and the
            // `catch` below, neither of which establishes a session; there the
            // old credential is still stored and the warning is still true.
            signOutResidue = .none
            credentials = fresh
            publish()
        } catch {
            // The discard path once more. `credentials` and `accessToken` are
            // already nil — ``signOut()`` cleared them — so what this guards is
            // `.failed`: an abandoned login that times out on
            // ``redirectTimeoutMs`` minutes later would otherwise change the
            // screen by itself, replacing the `.signedOut` the user asked for
            // with an error about a login they deliberately walked away from.
            guard sessionGeneration == generation else { return }
            credentials = nil
            accessToken = nil
            state = .failed(error.localizedDescription)
        }
    }

    /// Renew if the token has reached its renewal point. A no-op otherwise, so
    /// it is safe to call on every tick.
    ///
    /// Renewal is silent by design: the point of a refresh token is that the
    /// user is not interrupted, and interrupting them at 75% of a token's life
    /// would be an interruption for nothing.
    func refreshIfNeeded(issuer: String, clientID: String, nowMs: UInt64) async {
        guard let current = credentials, current.needsRenewal(nowMs: nowMs) else { return }
        guard let refreshToken = current.refreshToken else {
            // No refresh token: nothing to do until the access token actually
            // expires, at which point the user has to sign in again.
            if current.hasExpired(nowMs: nowMs) { signOut() }
            return
        }
        let generation = sessionGeneration
        let driver = makeDriver(issuer.trimmed, clientID.trimmed)
        do {
            let fresh = try await driver.refresh(refreshToken: refreshToken, nowMs: nowMs)
            // The other `store.save` in this type, under the same rule: a
            // sign-out taken while the renewal was in flight ended the session
            // this was renewing, and writing the renewed token here would hand
            // it straight back. Silent by design is what makes it worth
            // guarding — the screen stays `.signedIn` throughout, so the
            // **Sign out** at `AccountView.accountRow` is live the whole time.
            guard sessionGeneration == generation else { return }
            try store.save(fresh)
            // The renewal has replaced the credential a refused sign-out left
            // behind, so the residue about it stops being true here for the
            // same reason, and on the same line, as the save in `signIn()`.
            // ``signOutResidue`` states the invariant "until a `clear()` or a
            // `save()` replaces it", and this is such a save.
            signOutResidue = .none
            credentials = fresh
            publish()
        } catch {
            // A failed renewal is not a signed-out session: the current token
            // is still good until `expiresAtMs`, and the next tick tries
            // again. Dropping it here would sign the user out early, every
            // time the network blinked.
            //
            // Nor is it one when a sign-out has already ended the session:
            // `current` was captured at entry and that sign-out has already
            // invalidated it, so without this guard a renewal failing in the
            // background calls ``signOut()`` a SECOND time — a `store.clear()`
            // nobody asked for, and on a refusal a fresh `.unread` re-arming
            // the disclosure the user retired, with no user action behind it.
            guard sessionGeneration == generation else { return }
            if current.hasExpired(nowMs: nowMs) {
                // `signOut()` assigns `.signedOut` last, so the renewal error
                // has to be written after it or it is never shown. It used to
                // be written first, which made this assignment dead.
                signOut()
                state = .failed(error.localizedDescription)
            }
        }
    }

    /// Forget the token, here and in the Keychain — and say so when the
    /// Keychain will not let go of it.
    ///
    /// The local half is unconditional, deliberately. Refusing to drop the
    /// in-memory bearer because the Keychain was locked would leave the user
    /// holding a live session with no way out of it, which is worse than the
    /// state this method exists to reach.
    ///
    /// The Keychain half can genuinely refuse. ``KeychainItem/delete()`` maps
    /// `errSecItemNotFound` to success and throws for every other status, so
    /// "nothing was there" never arrives here and a locked keychain
    /// (`errSecInteractionNotAllowed`) or a dismissed prompt
    /// (`errSecUserCanceled`) does. `try?` used to absorb it and present
    /// `.signedOut` anyway: the refresh token survived, ``restore()`` read it
    /// back on the next launch, and the user was signed in again on a session
    /// they had deliberately ended — silently, and repeatably for as long as
    /// the refusal held.
    func signOut() {
        do {
            try store.clear()
            signOutResidue = .none
        } catch {
            // A fresh refusal starts the sequence over even where the user had
            // retired an earlier one: this message is new and unread, and the
            // credential it names is the one still in the Keychain now.
            signOutResidue = .unread(error.localizedDescription)
        }
        credentials = nil
        accessToken = nil
        state = .signedOut
        // Ends any login or renewal still in flight, which is the other half
        // of "forget the token". See ``sessionGeneration``: neither can be
        // cancelled, so they are discarded on the way back in instead.
        sessionGeneration &+= 1
    }

    /// Acknowledge ``signOutIncomplete``. The token it describes is still
    /// stored; dismissing says the user has read that, not that it is gone —
    /// which is why the residue moves to ``SignOutResidue/acknowledged``
    /// rather than away, keeping the control that re-runs the removal.
    func dismissSignOutIncomplete() {
        if case .unread = signOutResidue { signOutResidue = .acknowledged }
    }

    /// Dismiss the retry as well, and stop SAYING anything about this refusal.
    ///
    /// Not stop offering anything about it: the credential is still stored, so
    /// ``offersBareSignOut`` keeps a plain **Sign out** on the screen with no
    /// warning attached to it.
    ///
    /// The retry needs an exit of its own. It renders for the rest of the
    /// process and is otherwise cleared only by a `clear()` or a `save()` that
    /// SUCCEEDS — and the user it exists for is precisely the one who cannot
    /// unlock the Keychain, for whom neither ever does. Without this it is the
    /// alarming row with no way to clear it that this change rejected the
    /// other shape for. The refusal itself is not forgotten:
    /// ``signOutRefusedThisSession`` stands, and a sign-out refused again
    /// re-arms the disclosure from the top.
    func dismissSignOutRetry() {
        switch signOutResidue {
        case .unread, .acknowledged: signOutResidue = .retired
        case .none, .retired: break
        }
    }

    private func publish() {
        guard let credentials else {
            accessToken = nil
            state = .signedOut
            return
        }
        accessToken = credentials.accessToken
        state = .signedIn(expiresAtMs: credentials.expiresAtMs)
    }
}
