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
protocol RelayDeviceIDStore: Sendable {
    /// The recorded id, or `nil` when this device has never registered.
    func load() throws -> String?
    /// Record `id`, replacing any previous value.
    func store(_ id: String) throws
    /// Forget it. Sync continues unbound, which a self-host relay accepts.
    func clear() throws
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

    /// Per vault, like `KeychainVaultRootStore`: two vaults are two accounts at
    /// the relay and therefore two device rows.
    init(vaultName: String = VaultRegistry.firstVaultID) {
        item = KeychainItem(
            service: Self.service,
            account: vaultName,
            accessibility: Self.accessibility
        )
    }

    func load() throws -> String? {
        // As `KeychainVaultRootStore.load` does: nothing on the ordinary path
        // ever rewrites this item, so an id recorded by a build that used a
        // weaker class would keep it for the life of the installation.
        try item.upgradeAccessibilityIfNeeded()
        guard let data = try item.read(),
              let id = String(data: data, encoding: .utf8)?.trimmed,
              !id.isEmpty
        else { return nil }
        return id
    }

    func store(_ id: String) throws {
        let trimmed = id.trimmed
        guard !trimmed.isEmpty else { throw RelayDeviceIDError.empty }
        try item.write(Data(trimmed.utf8))
    }

    func clear() throws { try item.delete() }
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
/// `sunrise bootstrap`, and until the app can register on its own it is the
/// only way an Apple client is device-bound at all. Reading it here rather than
/// at each call site keeps "which id" a single decision, and keeps the
/// precedence — override first, exactly as `SyncEnv::with_stored` orders the
/// CLI's two sources — in one place.
enum RelayDeviceID {
    /// The environment variable, named to match `sunrise_cli::livesync`.
    static let environmentKey = "SUNRISE_SYNC_DEVICE_ID"

    /// The id to present, or `nil` for an unbound driver.
    static func resolve(
        store: RelayDeviceIDStore,
        environment: [String: String] = ProcessInfo.processInfo.environment
    ) -> String? {
        if let override = environment[environmentKey]?.trimmed, !override.isEmpty {
            return override
        }
        return (try? store.load()).flatMap { $0 }
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
    private var id: String?

    init(id: String? = nil) { self.id = id }

    func load() throws -> String? {
        lock.lock()
        defer { lock.unlock() }
        return id
    }

    func store(_ id: String) throws {
        let trimmed = id.trimmed
        guard !trimmed.isEmpty else { throw RelayDeviceIDError.empty }
        lock.lock()
        defer { lock.unlock() }
        self.id = trimmed
    }

    func clear() throws {
        lock.lock()
        defer { lock.unlock() }
        id = nil
    }
}
