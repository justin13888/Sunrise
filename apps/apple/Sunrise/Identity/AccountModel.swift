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

    private(set) var state: State = .signedOut

    /// The bearer to present to the relay, or `nil` when signed out.
    private(set) var accessToken: String?

    private let store: any CredentialStore
    private let makeDriver: @Sendable (String, String) -> any LoginDriver
    private let openURL: @Sendable (URL) -> Void
    private var credentials: StoredCredentials?

    /// Whether the last look at the store was refused in a way that may have
    /// left a copy of the token somewhere **nobody read**.
    ///
    /// Held rather than derived, because the two states it separates render the
    /// same: `signedOut` and a `failed` carrying a Keychain sentence both mean
    /// "no token in hand", and only this says whether asking again is the
    /// remedy or whether signing in is. ``signIn(issuer:clientID:deviceID:nowMs:)``
    /// is its only reader; ``restore()`` sets it either way on every call, so it
    /// never outlives the condition it describes.
    ///
    /// **It is the shape of the refusal that sets it, not the fact of one**, and
    /// the difference is a remedy: see ``mayHaveLeftACopyUnread(_:)``.
    private var storeRefusedToAnswer = false

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
    ///
    /// **`nil` and a throw are different answers, and this is the method where
    /// that starts to matter.** `CredentialStore.load` answers `nil` only when
    /// *both* keychains reported not-found — genuinely signed out — and throws
    /// when one of them was reached and refused, which leaves it unknown whether
    /// a token is sitting in it. `KeychainError.otherDomainUnreadable` is the
    /// shape that arrives when the refusing store is the one this build did not
    /// resolve to; `KeychainError.migrationUnverified` is the shape that arrives
    /// once two tokens already disagree.
    ///
    /// A `try?` here collapsed the second answer into the first, and the
    /// collapse was not cosmetic. The signed-out row offers exactly one thing,
    /// Sign in; signing in writes a *fresh* token into the domain this build
    /// resolved to while the unreadable one stays where it was; and two secrets
    /// under one `(service, account)` is `migrationUnverified` on every later
    /// launch — signed out in silence, for good, by the remedy the app itself
    /// held out. `KeychainCredentialStore.save` describes the same loop from the
    /// writing end, and names the one thing that ends it: a sign-in's
    /// cross-domain write collapses the pair.
    ///
    /// So the refusal is reported. It renders through `failed`, which already
    /// carries a sentence — for `otherDomainUnreadable` one naming the *other*
    /// keychain rather than the working one — and a Try again that
    /// ``signIn(issuer:clientID:deviceID:nowMs:)`` turns into a second look at
    /// the store rather than a second token. No new `State` case, because the
    /// difference a new case would encode is not one the view can act on
    /// differently: unlocking the keychain is the remedy either way, and Try
    /// again is how the app finds out it happened.
    ///
    /// **What Try again does with it is decided by the shape, not by the
    /// throw**, and this is where that is recorded. For every shape that may
    /// have left a copy unread it is a second look; for
    /// ``KeychainError/migrationUnverified`` it is a sign-in, because there the
    /// sign-in *is* the repair. ``mayHaveLeftACopyUnread(_:)`` is the whole of
    /// the distinction, and gating on any throw instead is what made that one
    /// shape a state with no way out of it at all.
    func restore() {
        do {
            credentials = try store.load()
            storeRefusedToAnswer = false
            publish()
        } catch {
            credentials = nil
            accessToken = nil
            storeRefusedToAnswer = Self.mayHaveLeftACopyUnread(error)
            state = .failed(error.localizedDescription)
        }
    }

    /// Whether a refusal out of ``CredentialStore/load()`` may have left a copy
    /// of the token somewhere **nobody read** — the one condition under which
    /// offering a sign-in writes a *second* secret beside a first that survives.
    ///
    /// Exhaustive on purpose: a new ``KeychainError`` case has to be placed here
    /// rather than inheriting a default, because the default is a user-visible
    /// dead end in one direction and a lost session in the other.
    ///
    /// - ``KeychainError/otherDomainUnreadable(_:)`` is the condition by
    ///   definition — the other keychain was reached, refused, and may be
    ///   holding a copy.
    /// - ``KeychainError/unexpected(_:)`` and ``KeychainError/malformedItem``
    ///   leave an item that exists and was not read; so does an error from a
    ///   ``CredentialStore`` this model knows nothing about, which is why the
    ///   non-`KeychainError` answer is `true`.
    /// - ``KeychainError/writtenButOtherDomainRefused(_:)`` names a surviving
    ///   copy in as many words.
    /// - ``KeychainError/accessibilityNotRaised(_:)`` belongs with them, and the
    ///   grouping is the one that is not obvious.
    ///   ``KeychainItem/upgradeAccessibilityIfNeeded()`` asks only for
    ///   `kSecReturnAttributes`, so it establishes that an item **exists**
    ///   without ever reading it, and
    ///   ``KeychainMigration/loadMigratingIfNeeded()`` raises it one line before
    ///   the cross-domain read that is the only thing that ever looks in the
    ///   other keychain. Nor can a sign-in repair it: the write it runs is the
    ///   same `SecItemUpdate` that was just refused. The remedy that method
    ///   names for itself is the next look, which is exactly what this returns.
    ///
    /// ``KeychainError/migrationUnverified`` is the exception, and it is the
    /// only one. There *both* copies were read and compared — that is what the
    /// shape means — so nothing survives unread, and a sign-in's
    /// ``KeychainItem/writeAcrossDomains(_:)`` writes this domain and deletes
    /// the other, which is the collapse that ends the disagreement. Blocking the
    /// login there does not prevent a second token; it prevents the repair.
    private static func mayHaveLeftACopyUnread(_ error: any Error) -> Bool {
        guard let refusal = error as? KeychainError else { return true }
        switch refusal {
        case .migrationUnverified:
            return false
        case .malformedItem, .unexpected, .accessibilityNotRaised,
             .writtenButOtherDomainRefused, .otherDomainUnreadable:
            return true
        }
    }

    /// Run a login. `deviceID` binds the token to this device: the issuer
    /// stamps it into a claim and the relay refuses a token whose claim names
    /// a different device, so one lifted off this Mac is useless elsewhere.
    func signIn(issuer: String, clientID: String, deviceID: String, nowMs: UInt64) async {
        // A sign-in run while the store may be holding a token nobody read is
        // the second token `restore()` describes, and this is the only place it
        // can be stopped — the view's Try again cannot know what the store
        // answered. So look again first, and let the answer decide: still
        // refused, and `restore()` has just re-stated the refusal with a
        // current status; answered with a token, and the session is restored
        // rather than replaced. Only "answered, and there is nothing there"
        // falls through to a login from here.
        //
        // What is guarded is the *shape* of the last refusal, not the fact of
        // one. `migrationUnverified` never sets the flag and so never reaches
        // this block: there the two copies already exist and have both been
        // read, and the login below is what collapses them. Gating on any throw
        // is what left that shape with no remedy at all — every Try again
        // re-threw and returned. See `mayHaveLeftACopyUnread(_:)`.
        if storeRefusedToAnswer {
            restore()
            guard !storeRefusedToAnswer, credentials == nil else { return }
        }
        guard !issuer.trimmed.isEmpty, !clientID.trimmed.isEmpty else {
            state = .failed(AccountError.notConfigured.localizedDescription)
            return
        }
        let driver = makeDriver(issuer.trimmed, clientID.trimmed)
        do {
            let url = try await driver.begin(deviceID: deviceID)
            state = .awaitingBrowser
            openURL(url)
            let fresh = try await driver.complete(
                timeoutMs: Self.redirectTimeoutMs,
                nowMs: nowMs
            )
            try store.save(fresh)
            credentials = fresh
            publish()
        } catch {
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
        let driver = makeDriver(issuer.trimmed, clientID.trimmed)
        do {
            let fresh = try await driver.refresh(refreshToken: refreshToken, nowMs: nowMs)
            try store.save(fresh)
            credentials = fresh
            publish()
        } catch {
            // A failed renewal is not a signed-out session: the current token
            // is still good until `expiresAtMs`, and the next tick tries
            // again. Dropping it here would sign the user out early, every
            // time the network blinked.
            if current.hasExpired(nowMs: nowMs) {
                state = .failed(error.localizedDescription)
                signOut()
            }
        }
    }

    /// Forget the token, here and in the Keychain.
    func signOut() {
        try? store.clear()
        credentials = nil
        accessToken = nil
        state = .signedOut
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
