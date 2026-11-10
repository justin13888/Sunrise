import Foundation
import Testing

@testable import Sunrise

/// The relay and sign-in settings, kept by the core outside every vault.
@MainActor
struct AppSettingsTests {
    /// A scratch defaults suite and a scratch directory, under one name, so
    /// no case writes the developer's own settings.
    private func scratch() throws -> (UserDefaults, URL) {
        let name = "sunrise-tests-\(UUID().uuidString)"
        let defaults = try #require(UserDefaults(suiteName: name))
        return (defaults, FileManager.default.temporaryDirectory.appending(path: name))
    }

    private func discard(_ directory: URL) {
        UserDefaults.standard.removePersistentDomain(forName: directory.lastPathComponent)
        try? FileManager.default.removeItem(at: directory)
    }

    /// The relay config is the core's bootstrap preferences, readable with no
    /// vault open — which is what the locked screen has.
    @Test
    func theRelayConfigIsReadableWithNoVault() throws {
        let (defaults, directory) = try scratch()
        defer { discard(directory) }

        let settings = AppSettings(defaults: defaults, directory: directory)
        settings.relayURL = "wss://relay.example/sync"
        settings.oidcIssuer = "https://issuer.example"
        settings.oidcClientID = "sunrise"
        settings.accountEmail = " user@example.com "

        let bootstrap = try bootstrapPreferences(dir: directory.path(percentEncoded: false))
        let relay = bootstrap.first { $0.key == AppSettings.Key.relayURL }?.value
        #expect(relay == .text(value: "wss://relay.example/sync"))

        let read = AppSettings(defaults: defaults, directory: directory)
        #expect(read.relayURL == "wss://relay.example/sync")
        #expect(read.oidcIssuer == "https://issuer.example")
        #expect(read.oidcClientID == "sunrise")
        #expect(read.accountEmail == "user@example.com", "trimmed by the core")
        #expect(read.canSignIn)
        #expect(read.canBootstrapAccount)

        read.relayURL = ""
        #expect(AppSettings(defaults: defaults, directory: directory).relayURL.isEmpty, "blank removes it")
    }

    /// Each value an earlier build kept in `UserDefaults` moves into the files
    /// once, and its key is deleted; the next build reads the files alone.
    @Test
    func eachOldKeyMigratesOnceAndIsDeleted() throws {
        let (defaults, directory) = try scratch()
        defer { discard(directory) }
        let old = [
            "sync.relayURL": "wss://relay.example/sync",
            "auth.oidcIssuer": "https://issuer.example",
            "auth.oidcClientID": "sunrise",
            "account.email": "user@example.com"
        ]
        for (key, value) in old { defaults.set(value, forKey: key) }

        let first = AppSettings(defaults: defaults, directory: directory)
        for key in old.keys {
            #expect(defaults.object(forKey: key) == nil, "\(key) is deleted once moved")
        }
        #expect(first.relayURL == "wss://relay.example/sync")
        #expect(first.accountEmail == "user@example.com")

        let second = AppSettings(defaults: defaults, directory: directory)
        #expect(second.relayURL == "wss://relay.example/sync")
        #expect(second.oidcIssuer == "https://issuer.example")
        #expect(second.oidcClientID == "sunrise")
        #expect(second.accountEmail == "user@example.com")
    }

    /// A value the bootstrap file refuses keeps its old key, and is still
    /// this device's setting, rather than being dropped.
    @Test
    func aValueTheFileRefusesStaysWhereItWas() throws {
        let (defaults, directory) = try scratch()
        defer { discard(directory) }
        defaults.set("relay.example", forKey: "sync.relayURL")

        let settings = AppSettings(defaults: defaults, directory: directory)
        #expect(defaults.string(forKey: "sync.relayURL") == "relay.example")
        #expect(settings.relayURL == "relay.example")
        #expect(settings.relayURLRefusal != nil, "the field says the value is not the stored one")
    }

    /// An address the core refuses — one typed without its scheme, or cut
    /// off mid-edit — is not written, and the field says so until one is:
    /// every other reader builds its settings from the file, which still
    /// holds the address saved before.
    @Test
    func aRefusedAddressIsReportedUntilOneIsTaken() throws {
        let (defaults, directory) = try scratch()
        defer { discard(directory) }

        let settings = AppSettings(defaults: defaults, directory: directory)
        settings.relayURL = "wss://relay.example/sync"
        settings.oidcIssuer = "https://issuer.example"
        #expect(settings.relayURLRefusal == nil)
        #expect(settings.oidcIssuerRefusal == nil)

        settings.relayURL = "relay.example/sync"
        settings.oidcIssuer = "https://"
        #expect(settings.relayURLRefusal != nil)
        #expect(settings.oidcIssuerRefusal != nil)
        let stored = AppSettings(defaults: defaults, directory: directory)
        #expect(stored.relayURL == "wss://relay.example/sync", "the address saved before is kept")
        #expect(stored.oidcIssuer == "https://issuer.example")

        settings.relayURL = "wss://relay.example/v2"
        settings.oidcIssuer = ""
        #expect(settings.relayURLRefusal == nil, "a value taken clears the refusal")
        #expect(settings.oidcIssuerRefusal == nil, "blank is a value: it removes the key")
        #expect(AppSettings(defaults: defaults, directory: directory).relayURL == "wss://relay.example/v2")
    }
}
