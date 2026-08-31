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

    private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        relayURL = defaults.string(forKey: Key.relayURL) ?? ""
        oidcIssuer = defaults.string(forKey: Key.oidcIssuer) ?? ""
        oidcClientID = defaults.string(forKey: Key.oidcClientID) ?? ""
    }

    /// Whether a login can even be attempted.
    var canSignIn: Bool {
        !oidcIssuer.trimmed.isEmpty && !oidcClientID.trimmed.isEmpty
    }

    /// Whether sync should be started at all.
    var syncIsConfigured: Bool { !relayURL.trimmed.isEmpty }

    private enum Key {
        static let relayURL = "sync.relayURL"
        static let oidcIssuer = "auth.oidcIssuer"
        static let oidcClientID = "auth.oidcClientID"
    }
}

extension String {
    var trimmed: String { trimmingCharacters(in: .whitespacesAndNewlines) }
}
