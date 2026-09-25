import Foundation

/// Where the **relay's** id for this device lives between launches.
///
/// Not `Core.deviceID`, and the difference is the whole reason this type
/// exists. `Core.deviceID` is the vault's own 16-byte id, minted locally and
/// carried in the OIDC `device_id` claim. This is the ULID the relay mints at
/// `POST /api/v1/devices` and returns **only** to the registering device: it
/// names the row the relay checks `X-Sunrise-Device-Sig` against, it never
/// travels back through the op stream, and a client that loses it holds a
/// signing key it cannot say whose it is. A relay with `require_device_sig`
/// then refuses every request.
///
/// A protocol for the same reason `VaultRootStore` is one: the state machine
/// above it must be testable without touching the developer's login Keychain.
///
/// It records a ``RelayDeviceBinding`` rather than a bare id: an id names a
/// row only on the relay, and under the account, that minted it.
protocol RelayDeviceIDStore: Sendable {
    /// The recorded binding, or `nil` when this device has never registered.
    func load() throws -> RelayDeviceBinding?
    /// Record `binding`, replacing any previous value.
    func store(_ binding: RelayDeviceBinding) throws
    /// Forget it. Sync continues unbound, which a self-host relay accepts.
    func clear() throws
}

/// The relay and the account a relay device id was minted for.
///
/// The relay looks the id up as `(account, device)` — `active_device` in the
/// server's device store — so the same id sent to another relay, or under a
/// bearer for another account, names no row. The relay answers that as a bad
/// bearer, deliberately indistinguishable from a token problem, and so the
/// client cannot tell it apart after the fact (#183). It has to be told apart
/// before: a stored id is presented only while the scope it was minted in is
/// still the scope sync runs in, and otherwise the device registers again.
struct RelayDeviceScope: Codable, Equatable, Sendable {
    /// The relay's origin as configured, trimmed and without trailing slashes,
    /// so `https://relay.example/` and `https://relay.example` are one relay.
    let relayURL: String
    /// The bearer's OIDC `iss` and `sub`, which is what the relay resolves an
    /// account from (`Store::resolve_account`). `nil` with no bearer, or with
    /// one that is not a JWT the app can read claims from.
    let account: String?

    init(relayURL: String, bearer: String?) {
        var url = relayURL.trimmed
        while url.hasSuffix("/") { url.removeLast() }
        self.relayURL = url
        account = bearer.flatMap(Self.account(ofBearer:))
    }

    /// `iss` and `sub` from a JWT's payload, **unverified**. The relay verifies
    /// the token; the app only needs to know whether two tokens name the same
    /// subject, and a forged token buys nothing but a mismatched key.
    static func account(ofBearer bearer: String) -> String? {
        let parts = bearer.trimmed.split(separator: ".", omittingEmptySubsequences: false)
        guard parts.count == 3 else { return nil }
        var base64 = parts[1]
            .replacingOccurrences(of: "-", with: "+")
            .replacingOccurrences(of: "_", with: "/")
        base64 += String(repeating: "=", count: (4 - base64.count % 4) % 4)
        guard let data = Data(base64Encoded: base64),
              let claims = try? JSONDecoder().decode(Claims.self, from: data),
              !claims.iss.isEmpty, !claims.sub.isEmpty
        else { return nil }
        // A separator no URL and no JSON string can carry unescaped, so no
        // two (iss, sub) pairs join to the same key.
        return "\(claims.iss)\u{0}\(claims.sub)"
    }

    private struct Claims: Decodable {
        let iss: String
        let sub: String
    }
}

/// A relay device id together with the scope it is valid in.
struct RelayDeviceBinding: Codable, Equatable, Sendable {
    let id: String
    let scope: RelayDeviceScope
}

/// The real one: the Keychain, in the vault root's protection class.
///
/// ## Why the Keychain, and not the two alternatives
///
/// The id is **not a secret** — the relay minted it and the client sends it in
/// the clear on every request that uses it, which is exactly why the CLI keeps
/// it in a plain file rather than a mode-0600 one
/// (`sunrise_cli::livesync::RELAY_DEVICE_FILE`). So secrecy is not what picks
/// the store. Two other properties are.
///
/// **It has to be lost and restored together with the key it names.** The id
/// is one half of a pair; the other half is `D_S_priv`, wrapped under the vault
/// root. Split them and the failure is worse than either alone: a device
/// holding an id whose key it does not have signs with the wrong key and the
/// relay answers "bad bearer" — deliberately indistinguishable from a token
/// problem, so a caller cannot enumerate an account's devices, and therefore
/// undiagnosable from the client. Keeping it in the Keychain under the vault
/// root's own class, `…AfterFirstUnlockThisDeviceOnly`, is what makes the two
/// halves travel — or refuse to travel — as one. On iOS a restore onto new
/// hardware leaves neither behind; on the Mac, until #130, it leaves both, and
/// the pair is still consistent. That consistency is the point, not the
/// secrecy.
///
/// **Losing it silently un-binds the client.** `UserDefaults` is where the
/// relay URL and the vault registry live and is right for both — neither is a
/// secret and both must be repairable without a vault. It is wrong here: a
/// preference domain reset by a restore, a container migration or a `defaults
/// delete` costs a setting the user can retype, and costs this one a binding
/// nobody can retype, because the relay never sends the id again.
///
/// The vault itself was the third candidate. It has a real advantage — the id
/// would travel with the *account* rather than with the installation — and a
/// disqualifying shape: per-device data in a synced, converging store, so
/// every device replicates every other device's relay id and a merge has to
/// decide which of them is "this" one. The id belongs to the installation, and
/// so does its home.
struct KeychainRelayDeviceIDStore: RelayDeviceIDStore {
    /// Shown as "Sunrise relay device" in Keychain Access. Its own service, so
    /// a user clearing one item does not lose the others.
    static let service = "dev.sunrise.Sunrise.relay-device-id"

    /// The vault root's class, for the reason argued on the type: the id and
    /// the key it names have to be present or absent together.
    static let accessibility = KeychainAccessibility.afterFirstUnlockThisDeviceOnly

    private let item: KeychainItem
    private let migration: KeychainMigration

    /// Per vault, like `KeychainVaultRootStore`: two vaults are two accounts at
    /// the relay and therefore two device rows.
    init(vaultName: String = VaultRegistry.firstVaultID) {
        item = KeychainItem(
            service: Self.service,
            account: vaultName,
            accessibility: Self.accessibility,
            domain: KeychainDomain.current
        )
        migration = KeychainMigration(
            source: KeychainItem(
                service: Self.service,
                account: vaultName,
                accessibility: Self.accessibility,
                domain: .login
            ),
            destination: item
        )
    }

    func load() throws -> RelayDeviceBinding? {
        // In this store's own `load`, not chained to the vault root's, because
        // the two know nothing about each other — and they do not need to. The
        // pairing invariant argued on the type is about loss and restore, and a
        // migration that never leaves zero readable copies cannot lose either
        // half while the other survives.
        //
        // As `KeychainVaultRootStore.load` does, through the same shared step:
        // nothing on the ordinary path ever rewrites this item, so an id
        // recorded by a build that used a weaker class would keep it for the
        // life of the installation.
        //
        // A value that does not decode — a bare id, with no scope recorded —
        // reads as nothing: an id whose relay and account are unknown cannot
        // be shown to name a row, and registering again is the recovery.
        guard let data = try migration.loadMigratingIfNeeded(),
              let binding = try? JSONDecoder().decode(RelayDeviceBinding.self, from: data),
              !binding.id.trimmed.isEmpty
        else { return nil }
        return RelayDeviceBinding(id: binding.id.trimmed, scope: binding.scope)
    }

    func store(_ binding: RelayDeviceBinding) throws {
        let trimmed = binding.id.trimmed
        guard !trimmed.isEmpty else { throw RelayDeviceIDError.empty }
        try item.write(JSONEncoder().encode(RelayDeviceBinding(id: trimmed, scope: binding.scope)))
    }

    /// Across both domains, so the id and the `D_S_priv` it names stay a pair:
    /// an id `load` could still find is one this device is still bound by.
    func clear() throws { try item.deleteAcrossDomains() }
}

enum RelayDeviceIDError: Error, Equatable {
    /// An empty id would start a driver presenting an `X-Sunrise-Device` that
    /// names no row, and the relay's answer to that is indistinguishable from a
    /// bad bearer. Refusing it here is the only place it can be told apart.
    case empty
}

extension RelayDeviceIDError: LocalizedError {
    var errorDescription: String? {
        switch self {
        case .empty: "A relay device id cannot be empty."
        }
    }
}

/// The stored id, or the environment override, or nothing.
///
/// The override is the CLI's `SUNRISE_SYNC_DEVICE_ID`, spelled the same and
/// meaning the same thing: *this device registered somewhere else*. It is what
/// points the app at a relay whose `POST /api/v1/devices` was run by
/// `sunrise bootstrap`. The app registers on its own since #183 — see
/// ``RelayDeviceRegistration`` — so this is a development affordance, and an
/// override present means the app does not register. Reading it here rather than
/// at each call site keeps "which id" a single decision, and keeps the
/// precedence — override first, exactly as `SyncEnv::with_stored` orders the
/// CLI's two sources — in one place.
enum RelayDeviceID {
    /// The environment variable, named to match `sunrise_cli::livesync`.
    static let environmentKey = "SUNRISE_SYNC_DEVICE_ID"

    /// The id to present to the relay and account `scope` names, or `nil` for
    /// an unbound driver.
    ///
    /// A stored id minted in another scope — before the relay URL changed, or
    /// under another account's bearer — resolves to nothing rather than to an
    /// id that names no row there. The override is taken as given: whoever
    /// set it registered against the relay they are pointing at.
    static func resolve(
        store: RelayDeviceIDStore,
        scope: RelayDeviceScope,
        environment: [String: String] = ProcessInfo.processInfo.environment
    ) -> String? {
        if let override = environment[environmentKey]?.trimmed, !override.isEmpty {
            return override
        }
        guard let binding = (try? store.load()).flatMap({ $0 }), binding.scope == scope else {
            return nil
        }
        return binding.id
    }
}

/// The two moments this device learns its relay id, and what each does with it.
///
/// The relay mints the id at `POST /api/v1/devices` and returns it once. For a
/// long time the app received it and dropped it: the recovery ceremony read
/// the code off `AccountBootstrap` and discarded the rest, and a device
/// admitted by pairing never registered at all (#183). Both paths now end in
/// the vault's ``RelayDeviceIDStore``, and they are here, over closures, so
/// the order each one depends on is testable with no relay and no Keychain.
///
/// `@MainActor` because both callers are, and the closures they pass capture
/// main-actor state (the session's bridge and settings); a nonisolated method
/// would have to send those closures off the actor.
@MainActor
enum RelayDeviceRegistration {
    /// Publish the account, record the relay id it returned, and hand back the
    /// recovery code.
    ///
    /// **The order is the point.** The id is recorded only after the relay
    /// accepted the registration — an id recorded for a registration that
    /// failed names no row, and the relay reports that as a bad bearer. And a
    /// failure to *record* it does not fail the call: by then the relay holds
    /// the recovery blob, the code returned is the only thing that opens it,
    /// and a retry would seal a new seed the relay refuses as
    /// `RECOVERY_BLOB_EXISTS`. Losing the code to save the id would trade the
    /// account key for a binding ``bind(store:scope:environment:register:)`` can
    /// re-establish on the next sync start.
    ///
    /// `scope` is the relay and bearer `bootstrap` publishes to; the id is
    /// recorded as valid there and nowhere else.
    static func publish(
        store: any RelayDeviceIDStore,
        scope: RelayDeviceScope,
        bootstrap: () async throws -> AccountBootstrap
    ) async throws -> String? {
        let outcome = try await bootstrap()
        try? store.store(RelayDeviceBinding(id: outcome.deviceId, scope: scope))
        return outcome.recoveryCode
    }

    /// The id to present in `scope`, registering this device first if it has
    /// none there.
    ///
    /// An id already resolved for `scope` — stored, or the
    /// `SUNRISE_SYNC_DEVICE_ID` override — is returned without a request: a
    /// second registration is a second relay row for the same key, which is
    /// harmless to the relay and clutters the device list a user revokes from.
    /// Otherwise `register` runs against `scope` and its id replaces whatever
    /// was stored. That covers a device that never registered and one whose
    /// stored id was minted on another relay or for another account, which
    /// would otherwise be presented where it names no row. A record the
    /// Keychain refuses is not an error here: the next start registers again
    /// rather than the device staying unbound for good.
    static func bind(
        store: any RelayDeviceIDStore,
        scope: RelayDeviceScope,
        environment: [String: String] = ProcessInfo.processInfo.environment,
        register: () async throws -> String
    ) async throws -> String {
        if let known = RelayDeviceID.resolve(store: store, scope: scope, environment: environment) {
            return known
        }
        let id = try await register().trimmed
        guard !id.isEmpty else { throw RelayDeviceIDError.empty }
        try? store.store(RelayDeviceBinding(id: id, scope: scope))
        return id
    }
}

/// A relay device id that lives and dies with the process.
///
/// The configuration with no `VaultRegistry` is the unit tests and the UI-test
/// harness, and neither may write to the developer's login Keychain — a UI test
/// that left a device id behind would bind the developer's next real run to a
/// row the relay minted for a test. Not `#if DEBUG`, unlike
/// `InMemoryVaultRootStore`, because it is the default of a non-debug
/// initializer and has to exist in every configuration that compiles one.
final class InMemoryRelayDeviceIDStore: RelayDeviceIDStore, @unchecked Sendable {
    private let lock = NSLock()
    private var binding: RelayDeviceBinding?

    init(binding: RelayDeviceBinding? = nil) { self.binding = binding }

    func load() throws -> RelayDeviceBinding? {
        lock.lock()
        defer { lock.unlock() }
        return binding
    }

    func store(_ binding: RelayDeviceBinding) throws {
        let trimmed = binding.id.trimmed
        guard !trimmed.isEmpty else { throw RelayDeviceIDError.empty }
        lock.lock()
        defer { lock.unlock() }
        self.binding = RelayDeviceBinding(id: trimmed, scope: binding.scope)
    }

    func clear() throws {
        lock.lock()
        defer { lock.unlock() }
        binding = nil
    }
}
