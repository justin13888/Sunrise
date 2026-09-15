import Foundation

/// Per-device configuration: where the relay is, and who issues tokens for it.
///
/// `UserDefaults`, not the vault. None of it is user data, none of it syncs,
/// and a device pointed at the wrong relay has to be repairable without
/// opening the vault it cannot reach.
@MainActor
@Observable
final class AppSettings {
    /// Relay `/sync` WebSocket URL. Empty means fully offline, which is a
    /// supported way to run: the core is local-first and needs no server.
    var relayURL: String {
        didSet { defaults.set(relayURL, forKey: Key.relayURL) }
    }

    /// OIDC issuer. Empty is normal for a self-host relay, which accepts an
    /// unauthenticated upgrade; every other deployment answers `401`.
    var oidcIssuer: String {
        didSet { defaults.set(oidcIssuer, forKey: Key.oidcIssuer) }
    }

    /// OIDC client id.
    var oidcClientID: String {
        didSet { defaults.set(oidcClientID, forKey: Key.oidcClientID) }
    }

    /// The address this account is registered under.
    ///
    /// Sent once, by `POST /api/v1/accounts`, and only as a fallback: the
    /// identity provider owns the address and the relay overwrites this with
    /// the `email` claim wherever it has one. It is here because the route
    /// refuses an empty one outright, and a self-host relay running
    /// `NullVerifier` emits no claim to fall back from.
    var accountEmail: String {
        didSet { defaults.set(accountEmail, forKey: Key.accountEmail) }
    }

    private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        relayURL = defaults.string(forKey: Key.relayURL) ?? ""
        oidcIssuer = defaults.string(forKey: Key.oidcIssuer) ?? ""
        oidcClientID = defaults.string(forKey: Key.oidcClientID) ?? ""
        accountEmail = defaults.string(forKey: Key.accountEmail) ?? ""
    }

    /// Whether a login can even be attempted.
    var canSignIn: Bool {
        !oidcIssuer.trimmed.isEmpty && !oidcClientID.trimmed.isEmpty
    }

    /// Whether sync should be started at all.
    var syncIsConfigured: Bool { !relayURL.trimmed.isEmpty }

    /// Whether this device can publish its vault to the relay — which is what
    /// seals a recovery blob, and therefore what puts a second copy of
    /// `ID_D_priv` anywhere at all.
    var canBootstrapAccount: Bool {
        syncIsConfigured && !accountEmail.trimmed.isEmpty
    }

    private enum Key {
        static let relayURL = "sync.relayURL"
        static let oidcIssuer = "auth.oidcIssuer"
        static let oidcClientID = "auth.oidcClientID"
        static let accountEmail = "account.email"
    }
}

extension String {
    var trimmed: String { trimmingCharacters(in: .whitespacesAndNewlines) }
}
