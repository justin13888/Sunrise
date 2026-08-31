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
