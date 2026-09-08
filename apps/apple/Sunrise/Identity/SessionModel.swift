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
        /// The user closed the vault. The key is where it was and the data is
        /// where it was; only this process let go. Reopening is one button.
        case lockedByUser

        var summary: String {
            switch self {
            case let .keychainUnavailable(detail):
                "Sunrise could not read its key from the Keychain. \(detail)"
            case .keyMissingForExistingVault:
                """
                There is a vault on this device, but its key is not in this \
                Keychain. Pair with a device that still has it — creating a \
                new key would leave the existing data unreadable.
                """
            case .lockedByUser:
                """
                Your vault is closed. Nothing was lost — the key is still in \
                your Keychain, and unlocking opens it again.
                """
            }
        }

        /// What the button that resolves this reason should say.
        var repairTitle: String {
            self == .lockedByUser ? "Unlock" : "Try again"
        }
    }

    private(set) var phase: Phase = .starting
    private(set) var bridge: CoreBridge?

    /// The vaults this Mac knows about, or `nil` in the single-vault
    /// configuration the tests and the UI-test harness use. Non-`nil` is what
    /// puts the switcher on screen.
    let vaults: VaultRegistry?

    /// Both `var`: switching vaults replaces them together, and replacing only
    /// one would file a vault's key under another vault's name.
    private var location: VaultLocation
    private var rootStore: any VaultRootStore
    /// The relay's id for this vault's device. Re-pointed with the two above
    /// and for the same reason: an id from one vault names a device row the
    /// next vault's signing key does not open, and the relay reports that as a
    /// bad bearer rather than as the mismatch it is.
    private var relayDeviceStore: any RelayDeviceIDStore
    private let appVersion: String
    private let openBridge: @Sendable (URL, Data, String, Data?) async throws -> CoreBridge
    private let resolve: @Sendable (VaultDescriptor) throws -> VaultBinding
    /// Set when the app could not even work out where its vault goes. Checked
    /// first by `start`, so retrying reports the real cause rather than
    /// falling through to a first-run screen against a nonsense path.
    private var configurationError: String?

    /// The single-vault configuration: the unit tests and the UI-test harness.
    ///
    /// `relayDeviceStore` defaults to the in-memory one because that is what
    /// both of its callers want — neither may write a device id into the
    /// developer's login Keychain. Production reaches ``standard()``'s other
    /// branch, which resolves a real store per vault through `resolve`.
    init(
        location: VaultLocation,
        rootStore: any VaultRootStore,
        appVersion: String,
        configurationError: String? = nil,
        relayDeviceStore: any RelayDeviceIDStore = InMemoryRelayDeviceIDStore(),
        openBridge: @escaping @Sendable (URL, Data, String, Data?) async throws -> CoreBridge = {
            try await CoreBridge.open(
                directory: $0,
                vaultRoot: $1,
                appVersion: $2,
                pairedBundle: $3
            )
        }
    ) {
        vaults = nil
        resolve = { descriptor in throw VaultLocationError.unusableIdentifier(descriptor.id) }
        self.location = location
        self.rootStore = rootStore
        self.appVersion = appVersion
        self.configurationError = configurationError
        self.relayDeviceStore = relayDeviceStore
        self.openBridge = openBridge
    }

    /// The multi-vault configuration: a registry, and a way to turn any entry
    /// in it into a directory and a Keychain account.
    init(
        vaults: VaultRegistry,
        appVersion: String,
        resolve: @escaping @Sendable (VaultDescriptor) throws -> VaultBinding,
        openBridge: @escaping @Sendable (URL, Data, String, Data?) async throws -> CoreBridge = {
            try await CoreBridge.open(
                directory: $0,
                vaultRoot: $1,
                appVersion: $2,
                pairedBundle: $3
            )
        }
    ) {
        self.vaults = vaults
        self.resolve = resolve
        self.appVersion = appVersion
        self.openBridge = openBridge

        // Resolved here rather than lazily, so a Mac with no usable
        // Application Support directory reports that instead of looking like a
        // first run against a path that does not exist.
        var resolved: VaultBinding?
        var failure: String?
        do {
            resolved = try resolve(vaults.selected)
        } catch {
            failure = error.localizedDescription
        }
        location = resolved?.location ?? VaultLocation(directory: URL(filePath: "/dev/null"))
        rootStore = resolved?.rootStore ?? KeychainVaultRootStore(vaultName: vaults.selectedID)
        relayDeviceStore = resolved?.relayDeviceStore
            ?? KeychainRelayDeviceIDStore(vaultName: vaults.selectedID)
        configurationError = failure
    }

    /// The app's own configuration. Falls straight to `.failed` when the
    /// Application Support directory is unusable, rather than pretending it is
    /// a first run.
    ///
    /// There is no memoized `shared` behind this and no `active` in front of
    /// it. There used to be, so that `OnboardingView`, `LockedView` and
    /// `AccountView` — which `RootView` built without passing the session down
    /// — could reach one; they are handed it explicitly now, and a global that
    /// nothing reads is a global that will eventually be read by mistake.
    ///
    /// `crates/sunrise-core/src/vault_lock.rs` still admits one open vault per
    /// process, and that invariant does not depend on this being memoized: a
    /// `SessionModel` opens nothing until ``start()`` is called, and the only
    /// caller is the view built from the `@State` SwiftUI actually kept.
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
        return SessionModel(
            vaults: VaultRegistry(),
            appVersion: appVersion,
            resolve: { descriptor in
                VaultBinding(
                    location: try VaultLocation.forVault(descriptor.id),
                    // The `vaultName` parameter that has existed since this
                    // store was written and has never been passed anything.
                    rootStore: KeychainVaultRootStore(vaultName: descriptor.id),
                    relayDeviceStore: KeychainRelayDeviceIDStore(vaultName: descriptor.id)
                )
            }
        )
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

    /// Take what a completed pairing produced, and open with it.
    ///
    /// The counterpart to `createVault`, and the opposite of it in the way
    /// that matters: this root is not new, so it is the one thing that can
    /// legitimately be written over a vault that already exists on disk. That
    /// is the entire point of `LockReason.keyMissingForExistingVault` — the
    /// vault is here, the key is not, and pairing is how the key comes back.
    ///
    /// `bundle` is the rest of what the pairing carried: the account identity
    /// and every Stream key. It goes in on this open and only this one, because
    /// the identity a vault belongs to is decided when the vault is created —
    /// handing it over afterwards would have nothing left to join. Only the
    /// root is stored; the bundle is spent here.
    ///
    /// Refused from `.unlocked`, where there is a live core holding the lock
    /// and a root that is already working.
    func adoptPairing(root: Data, bundle: Data) async {
        guard phase != .unlocked else { return }
        guard root.count == VaultRoot.byteCount else {
            phase = .failed(VaultRootError.wrongLength(root.count).localizedDescription)
            return
        }
        phase = .starting
        do {
            // Stored before opening, for the same reason `createVault` does:
            // a vault opened under a key that was never persisted is
            // unreadable on the next launch.
            try rootStore.store(root)
        } catch {
            phase = .failed(error.localizedDescription)
            return
        }
        await open(with: root, bundle: bundle)
    }

    /// Close the vault that is open and open another one.
    ///
    /// **The order here is the whole method.** `vault_lock.rs` admits one open
    /// vault per process — a process-local registry of canonicalized paths,
    /// checked before the OS lock — and `CoreBridge.shutdown()` is what drops
    /// the Rust `Core` that holds it. Opening the new vault before closing the
    /// old one does not fail to compile and does not fail on the second vault
    /// either: it fails on whichever `Core::open` loses the race, thirteen
    /// retries later, with a message naming this very process as the holder.
    ///
    /// Resolving comes first because it can fail and costs nothing — a URL and
    /// a Keychain query descriptor — so a bad descriptor leaves the vault that
    /// is currently open exactly where it was.
    func switchTo(_ descriptor: VaultDescriptor) async {
        guard let vaults else { return }
        guard descriptor.id != vaults.selectedID || phase != .unlocked else { return }

        let binding: VaultBinding
        do {
            binding = try resolve(descriptor)
        } catch {
            phase = .failed(error.localizedDescription)
            return
        }

        // 1. Let go. Nothing below can succeed until this returns.
        await bridge?.shutdown()
        bridge = nil
        phase = .starting

        // 2. Re-point. Both halves together: a directory from one vault and a
        //    Keychain account from another is an unreadable vault.
        location = binding.location
        rootStore = binding.rootStore
        relayDeviceStore = binding.relayDeviceStore
        configurationError = nil
        vaults.select(descriptor.id)

        // 3. Open, through the same launch decision as a cold start — so a
        //    vault that has a directory and no key lands on the locked screen
        //    rather than being handed a new key.
        await start()
    }

    /// Register a vault and switch to it. The new one has no key and no
    /// directory, so it arrives at `.firstRun`: create, or pair.
    func addVault(named name: String) async {
        guard let vaults else { return }
        await switchTo(vaults.add(name: name))
    }

    private func open(with root: Data, bundle: Data? = nil) async {
        do {
            bridge = try await openBridge(location.directory, root, appVersion, bundle)
            phase = .unlocked
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }

    /// The relay's id for this device against the open vault, or `nil` for an
    /// unbound sync driver.
    ///
    /// A read rather than stored state: the environment override
    /// `RelayDeviceID.resolve` consults is a launch-time fact, and the stored
    /// half is written by registration, so re-reading is what makes a driver
    /// restarted after registration pick the binding up.
    var relayDeviceID: String? { RelayDeviceID.resolve(store: relayDeviceStore) }

    /// Close the vault, releasing the core's lock on it.
    ///
    /// Lands on ``LockReason/lockedByUser`` rather than `.starting`, and the
    /// difference is the whole method. `.starting` is a *transient* phase —
    /// every other route into it (``start()``, ``switchTo(_:)``,
    /// ``adoptPairing(root:bundle:)``) drives itself out again on the same call — and
    /// `RootView` renders it as a bare `ProgressView` whose
    /// `.task { await session.start() }` will not re-fire, because it is
    /// attached above the phase switch and fires once for the life of the
    /// window. A `lock()` that stopped at `.starting` would therefore leave a
    /// spinner nothing would ever replace.
    ///
    /// Not resolved by calling ``start()`` from here either: that would reopen
    /// the vault it was just asked to close, which is not a lock and would also
    /// make this useless as the release path the tests use for cleanup.
    func lock() async {
        await bridge?.shutdown()
        bridge = nil
        phase = .locked(.lockedByUser)
    }
}

/// Where one vault's data and one vault's key live, resolved together.
///
/// A pair rather than two values, because they are only ever correct together:
/// see `SessionModel.switchTo`.
struct VaultBinding: Sendable {
    let location: VaultLocation
    let rootStore: any VaultRootStore
    /// Where the relay's id for this vault's device lives. Part of the pair
    /// rather than resolved at the call site: it is keyed by the same vault id
    /// as the other two, and a client that presents one vault's device id while
    /// signing with another's key is refused in a way it cannot diagnose.
    let relayDeviceStore: any RelayDeviceIDStore
}
