import Foundation

/// What the app is, at launch and after.
///
/// Three of these are routinely collapsed into one "not signed in" screen, and
/// collapsing them is how a client loses a vault: told there is no key when
/// the Keychain merely would not answer, the obvious repair — make a new one —
/// permanently orphans the encrypted data already on disk.
@MainActor
@Observable
final class SessionModel {
    enum Phase: Equatable {
        /// Reading the Keychain and the vault directory.
        case starting
        /// No vault and no key. Nothing is at risk; offer to create one.
        case firstRun
        /// A vault exists, or a key may, and it cannot be opened right now.
        /// **Never** a cue to generate a new key.
        case locked(LockReason)
        /// Open. `bridge` is live.
        case unlocked
        /// Something failed in a way retrying will not fix on its own.
        case failed(String)
    }

    enum LockReason: Equatable {
        /// The Keychain refused, was locked, or returned something unusable.
        case keychainUnavailable(String)
        /// The vault is on disk and its key is not in this Keychain — the
        /// state a restored-from-backup machine lands in. Recovering it means
        /// pairing with a device that still has the key, not making one up.
        case keyMissingForExistingVault

        var summary: String {
            switch self {
            case let .keychainUnavailable(detail):
                "Sunrise could not read its key from the Keychain. \(detail)"
            case .keyMissingForExistingVault:
                """
                There is a vault on this Mac, but its key is not in this \
                Keychain. Pair with a device that still has it — creating a \
                new key would leave the existing data unreadable.
                """
            }
        }
    }

    private(set) var phase: Phase = .starting
    private(set) var bridge: CoreBridge?

    private let location: VaultLocation
    private let rootStore: any VaultRootStore
    private let appVersion: String
    private let openBridge: @Sendable (URL, Data, String) async throws -> CoreBridge
    /// Set when the app could not even work out where its vault goes. Checked
    /// first by `start`, so retrying reports the real cause rather than
    /// falling through to a first-run screen against a nonsense path.
    private let configurationError: String?

    init(
        location: VaultLocation,
        rootStore: any VaultRootStore,
        appVersion: String,
        configurationError: String? = nil,
        openBridge: @escaping @Sendable (URL, Data, String) async throws -> CoreBridge = {
            try await CoreBridge.open(directory: $0, vaultRoot: $1, appVersion: $2)
        }
    ) {
        self.location = location
        self.rootStore = rootStore
        self.appVersion = appVersion
        self.configurationError = configurationError
        self.openBridge = openBridge
    }

    /// The app's own configuration. Falls straight to `.failed` when the
    /// Application Support directory is unusable, rather than pretending it is
    /// a first run.
    static func standard() -> SessionModel {
        let version = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString")
        let appVersion = (version as? String) ?? "0.0.0"
        #if DEBUG
        if let scratch = UITestHarness.scratchVault() {
            // A UI test drives the real window, and the real window must not
            // open the developer's vault or write to their login Keychain.
            return SessionModel(
                location: VaultLocation(directory: scratch),
                rootStore: InMemoryVaultRootStore(),
                appVersion: appVersion
            )
        }
        #endif
        do {
            return SessionModel(
                location: try VaultLocation.standard(),
                rootStore: KeychainVaultRootStore(),
                appVersion: appVersion
            )
        } catch {
            return SessionModel(
                location: VaultLocation(directory: URL(filePath: "/dev/null")),
                rootStore: KeychainVaultRootStore(),
                appVersion: appVersion,
                configurationError: error.localizedDescription
            )
        }
    }

    /// Decide the launch state and, where possible, open the vault.
    func start() async {
        guard phase != .unlocked else { return }
        if let configurationError {
            phase = .failed(configurationError)
            return
        }
        phase = .starting

        let stored: Data?
        do {
            stored = try rootStore.load()
        } catch {
            phase = .locked(.keychainUnavailable(error.localizedDescription))
            return
        }

        guard let root = stored else {
            phase = location.exists() ? .locked(.keyMissingForExistingVault) : .firstRun
            return
        }
        await open(with: root)
    }

    /// Create the vault. Valid only from `.firstRun`.
    ///
    /// The guard is not defensive tidiness: this is the one operation that can
    /// make existing data unreadable, so it refuses to run from any state
    /// where existing data might be there.
    func createVault() async {
        guard phase == .firstRun else { return }
        phase = .starting
        do {
            let root = try VaultRoot.generate()
            // Stored *before* opening: a vault opened under a key that was
            // never persisted is unreadable on the next launch, which is the
            // same loss by a slower route.
            try rootStore.store(root)
            await open(with: root)
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }

    private func open(with root: Data) async {
        do {
            bridge = try await openBridge(location.directory, root, appVersion)
            phase = .unlocked
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }

    /// Close the vault and return to the launch decision.
    func lock() async {
        await bridge?.shutdown()
        bridge = nil
        phase = .starting
    }
}
