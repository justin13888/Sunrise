import Foundation

/// Per-device configuration: where the relay is, and who issues tokens for it.
///
/// Kept by the core, outside every vault (#489). The relay URL and the OIDC
/// issuer and client id are the core's bootstrap preferences —
/// `sync.relay_url`, `auth.oidc_issuer`, `auth.oidc_client_id` in
/// `preferences.bootstrap.json` — and the account address is a client setting
/// in `client-settings.json`, both in ``directory``. Neither file is a vault:
/// none of it is user data, none of it syncs, and a device pointed at the
/// wrong relay has to be repairable without opening the vault it cannot reach.
///
/// The values an earlier build kept in `UserDefaults` move into those files
/// the first time this is built, and their keys are deleted once written.
@MainActor
@Observable
final class AppSettings {
    /// Relay `/sync` WebSocket URL. Empty means fully offline, which is a
    /// supported way to run: the core is local-first and needs no server.
    ///
    /// A value the core refuses as a URL — one still being typed — stays
    /// here and is not written; the last one that was stays on disk, every
    /// other reader uses it, and ``refusals`` says so until a value is taken.
    var relayURL: String {
        didSet { setBootstrap(Key.relayURL, to: relayURL) }
    }

    /// OIDC issuer. Empty is normal for a self-host relay, which accepts an
    /// unauthenticated upgrade; every other deployment answers `401`.
    var oidcIssuer: String {
        didSet { setBootstrap(Key.oidcIssuer, to: oidcIssuer) }
    }

    /// OIDC client id.
    var oidcClientID: String {
        didSet { setBootstrap(Key.oidcClientID, to: oidcClientID) }
    }

    /// The address this account is registered under.
    ///
    /// Sent once, by `POST /api/v1/accounts`, and only as a fallback: the
    /// identity provider owns the address and the relay overwrites this with
    /// the `email` claim wherever it has one. It is here because the route
    /// refuses an empty one outright, and a self-host relay running
    /// `NullVerifier` emits no claim to fall back from. Not a preference
    /// (`docs/02-domain/preferences.md`), so it is a client setting.
    var accountEmail: String {
        didSet {
            let email = accountEmail.trimmed
            _ = try? setAccountEmail(dir: path, email: email.isEmpty ? nil : email)
        }
    }

    /// Why the bootstrap file refused the value shown for a key, by key
    /// (`sync.relay_url`, `auth.oidc_issuer`, `auth.oidc_client_id`). A key
    /// is here while what is shown differs from what is stored: the field
    /// says so, rather than leaving a value on screen that no reader uses and
    /// the next launch drops.
    private(set) var refusals: [String: String] = [:]

    /// What the relay field says while ``relayURL`` is not the stored one.
    var relayURLRefusal: String? {
        refusals[Key.relayURL].map { L10n.Account.notSaved(reason: $0) }
    }

    /// What the issuer field says while ``oidcIssuer`` is not the stored one.
    var oidcIssuerRefusal: String? {
        refusals[Key.oidcIssuer].map { L10n.Account.notSaved(reason: $0) }
    }

    /// Where the bootstrap file and the client settings file live.
    let directory: URL

    /// `~/Library/Application Support/Sunrise`, beside the `vault` directory.
    static var standardDirectory: URL {
        URL.applicationSupportDirectory.appending(path: "Sunrise")
    }

    /// `defaults` is read only for the values an earlier build kept there.
    init(defaults: UserDefaults = .standard, directory: URL = AppSettings.standardDirectory) {
        self.directory = directory
        let path = directory.path(percentEncoded: false)
        let refused = Self.moveLegacyValues(from: defaults, into: path)

        let bootstrap = (try? bootstrapPreferences(dir: path)) ?? []
        func stored(_ key: String) -> String {
            for item in bootstrap where item.key == key {
                if case let .text(value)? = item.value { return value }
            }
            // A value the bootstrap file refused stays in `UserDefaults`, and
            // is still this device's setting.
            return defaults.string(forKey: Key.legacy[key] ?? "") ?? ""
        }
        relayURL = stored(Key.relayURL)
        oidcIssuer = stored(Key.oidcIssuer)
        oidcClientID = stored(Key.oidcClientID)
        accountEmail = (try? clientSettings(dir: path))?.accountEmail
            ?? defaults.string(forKey: Key.legacyAccountEmail) ?? ""
        refusals = refused
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

    private var path: String { directory.path(percentEncoded: false) }

    /// Write one bootstrap key, trimmed; blank removes it. A refusal is
    /// kept in ``refusals`` until a value for the key is written.
    private func setBootstrap(_ key: String, to value: String) {
        let trimmed = value.trimmed
        do {
            try setBootstrapPreference(dir: path, key: key, value: trimmed.isEmpty ? nil : trimmed)
            refusals[key] = nil
        } catch {
            refusals[key] = error.localizedDescription
        }
    }

    /// Move what an earlier build kept in `UserDefaults` into the files, once.
    ///
    /// Here rather than in the core's `migrate_legacy_defaults`, which needs
    /// an open vault: these are read on the locked screen, before there is
    /// one. A key is deleted only once its value is written, so a value the
    /// file refuses stays where it was and is still read from there; why it
    /// was refused is returned, by bootstrap key.
    private static func moveLegacyValues(from defaults: UserDefaults, into path: String) -> [String: String] {
        var refused: [String: String] = [:]
        for (key, old) in Key.legacy {
            guard let value = defaults.string(forKey: old) else { continue }
            let trimmed = value.trimmed
            do {
                try setBootstrapPreference(dir: path, key: key, value: trimmed.isEmpty ? nil : trimmed)
                defaults.removeObject(forKey: old)
            } catch {
                refused[key] = error.localizedDescription
            }
        }
        if let email = defaults.string(forKey: Key.legacyAccountEmail) {
            let trimmed = email.trimmed
            if (try? setAccountEmail(dir: path, email: trimmed.isEmpty ? nil : trimmed)) != nil {
                defaults.removeObject(forKey: Key.legacyAccountEmail)
            }
        }
        return refused
    }

    enum Key {
        static let relayURL = "sync.relay_url"
        static let oidcIssuer = "auth.oidc_issuer"
        static let oidcClientID = "auth.oidc_client_id"
        /// The `UserDefaults` key each bootstrap key was kept under.
        static let legacy = [
            relayURL: "sync.relayURL",
            oidcIssuer: "auth.oidcIssuer",
            oidcClientID: "auth.oidcClientID"
        ]
        static let legacyAccountEmail = "account.email"
    }
}

extension String {
    var trimmed: String { trimmingCharacters(in: .whitespacesAndNewlines) }
}
