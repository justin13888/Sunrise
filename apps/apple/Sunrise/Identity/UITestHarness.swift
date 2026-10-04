#if DEBUG
import Foundation

/// The one hook the app offers a UI test.
///
/// A UI test drives the real window, which means the real `SessionModel` — and
/// that must not open the developer's vault or write to their login Keychain.
/// The launch argument redirects both to a scratch directory the test owns.
///
/// `#if DEBUG` and an explicit launch argument, so nothing in a release build
/// can reach it and nothing in a normal run can trip over it.
enum UITestHarness {
    /// The launch argument a UI test passes, followed by a directory path.
    static let flag = "-sunrise-ui-test-vault"

    /// The launch argument a UI test passes to *keep* the recovery ceremony.
    ///
    /// The ceremony is a sheet over the whole window, presented the moment a
    /// vault is created — and every UI test in both suites starts by creating
    /// one, because the scratch directory is empty and the key store dies with
    /// the process. Left on, it covers the app before the first assertion and
    /// every suite fails at once, which is exactly what happened the first time
    /// this shipped.
    ///
    /// Opt **in** rather than opt out, so a test that says nothing gets the
    /// window it is trying to drive, and the one suite that wants the ceremony
    /// asks for it by name and is the only place it can flake.
    static let recoveryFlag = "-sunrise-ui-test-recovery"

    /// The launch argument a UI test passes to get the **multi-vault** session.
    ///
    /// The single-vault session every other suite runs has no registry, and
    /// `AccountView` hides its whole Vaults section without one — the vault
    /// picker, **Add a vault…** and **Add a device…** with it. So the harness's
    /// default configuration is exactly the one in which those three controls
    /// cannot be reached, and `SunriseiOSUITests/AccountUITests` asks for this
    /// one by name to reach them (#287).
    ///
    /// Opt **in**, for the same reason as ``recoveryFlag``: a suite that says
    /// nothing keeps the configuration it was written against.
    static let multiVaultFlag = "-sunrise-ui-test-multi-vault"

    /// The launch argument a UI test passes to make every quick-capture
    /// commit **fail**.
    ///
    /// `quick-capture.failure` is the sheet's durable refusal label, and a UI
    /// test that asserts it is *absent* proves nothing unless some test also
    /// proves it is drawn when a commit is refused. Nothing a UI test can do
    /// to the scratch vault makes the core refuse a one-line capture, so the
    /// refusal is injected at ``AppSurfaces/commitCapture(_:)`` — the seam the
    /// sheet commits through — as ``RefusedByUITestHarness`` (#295).
    ///
    /// Opt **in**, for the same reason as ``recoveryFlag``. Only the sheet's
    /// commit is failed: the inline capture bar writes through its own model
    /// and is untouched, so a suite under this flag can still create tasks.
    static let failCaptureFlag = "-sunrise-ui-test-fail-capture"

    /// The `UserDefaults` suite the multi-vault registry is kept in.
    ///
    /// Never `.standard`: the registry persists its list, and a list a previous
    /// test left behind would put vaults on screen this launch never added.
    static let registrySuite = "dev.sunrise.ui-test.vaults"

    /// Whether this process should present the recovery ceremony.
    ///
    /// `true` for any launch that is not a UI test, which is every real one.
    static func presentsRecoveryCeremony(
        arguments: [String] = ProcessInfo.processInfo.arguments
    ) -> Bool {
        scratchVault(arguments: arguments) == nil || arguments.contains(recoveryFlag)
    }

    /// Whether this process should run the multi-vault session.
    ///
    /// `false` without a scratch vault, whatever else was passed: the flag
    /// changes how a UI test's session is built, never whether one is.
    static func usesMultiVault(
        arguments: [String] = ProcessInfo.processInfo.arguments
    ) -> Bool {
        scratchVault(arguments: arguments) != nil && arguments.contains(multiVaultFlag)
    }

    /// Whether this process should refuse every quick-capture commit.
    ///
    /// `false` without a scratch vault, whatever else was passed, exactly as
    /// ``usesMultiVault(arguments:)`` is: no launch that can reach a real vault
    /// can have its captures thrown away by a stray argument.
    static func failsQuickCapture(
        arguments: [String] = ProcessInfo.processInfo.arguments
    ) -> Bool {
        scratchVault(arguments: arguments) != nil && arguments.contains(failCaptureFlag)
    }

    /// Throw ``RefusedByUITestHarness`` when ``failsQuickCapture(arguments:)``.
    ///
    /// Called by ``AppSurfaces/commitCapture(_:)`` after its no-open-vault
    /// guard and before the core is reached, so the sheet sees the refusal
    /// exactly as it would see one the core returned.
    static func refuseQuickCaptureIfAsked(
        arguments: [String] = ProcessInfo.processInfo.arguments
    ) throws {
        if failsQuickCapture(arguments: arguments) { throw RefusedByUITestHarness() }
    }

    /// The session a UI-test launch runs, or `nil` for every other launch.
    ///
    /// Both configurations keep everything under the scratch directory and
    /// every key in memory, so neither can open the developer's vault or write
    /// to their login Keychain:
    ///
    /// - **Single vault**, the default: the scratch directory *is* the vault.
    /// - **Multi-vault**, under ``multiVaultFlag``: a real ``VaultRegistry``
    ///   over ``registrySuite``, emptied here so every launch starts from the
    ///   one first vault a fresh install has, and each vault in it resolved to
    ///   `scratch/<id>` with key stores that outlive a switch — switching back
    ///   to a vault must find its key, exactly as the Keychain would.
    @MainActor
    static func session(
        appVersion: String,
        arguments: [String] = ProcessInfo.processInfo.arguments
    ) -> SessionModel? {
        guard let scratch = scratchVault(arguments: arguments) else { return nil }
        guard usesMultiVault(arguments: arguments) else {
            return SessionModel(
                location: VaultLocation(directory: scratch),
                rootStore: InMemoryVaultRootStore(),
                appVersion: appVersion
            )
        }
        UserDefaults.standard.removePersistentDomain(forName: registrySuite)
        guard let defaults = UserDefaults(suiteName: registrySuite) else {
            preconditionFailure("\(registrySuite) is neither the app's domain nor the global one")
        }
        let stores = ScratchVaultStores()
        return SessionModel(
            vaults: VaultRegistry(defaults: defaults),
            appVersion: appVersion,
            resolve: { descriptor in
                VaultBinding(
                    location: VaultLocation(directory: scratch.appending(path: descriptor.id)),
                    rootStore: stores.rootStore(for: descriptor.id),
                    relayDeviceStore: stores.relayDeviceStore(for: descriptor.id)
                )
            }
        )
    }

    /// The scratch vault directory this process was launched with, if any.
    static func scratchVault(
        arguments: [String] = ProcessInfo.processInfo.arguments
    ) -> URL? {
        guard let index = arguments.firstIndex(of: flag),
              arguments.index(after: index) < arguments.endIndex else { return nil }
        return URL(filePath: arguments[arguments.index(after: index)])
    }
}

/// The multi-vault harness's key stores, one pair per vault id.
///
/// Held for the life of the process rather than made afresh per resolve:
/// ``SessionModel/switchTo(_:)`` resolves a vault every time it is opened, and
/// a fresh in-memory store would present a vault this process created as one
/// whose key is missing.
final class ScratchVaultStores: @unchecked Sendable {
    private let lock = NSLock()
    private var roots: [String: InMemoryVaultRootStore] = [:]
    private var relayDevices: [String: InMemoryRelayDeviceIDStore] = [:]

    func rootStore(for id: String) -> InMemoryVaultRootStore {
        lock.lock()
        defer { lock.unlock() }
        if let store = roots[id] { return store }
        let store = InMemoryVaultRootStore()
        roots[id] = store
        return store
    }

    func relayDeviceStore(for id: String) -> InMemoryRelayDeviceIDStore {
        lock.lock()
        defer { lock.unlock() }
        if let store = relayDevices[id] { return store }
        let store = InMemoryRelayDeviceIDStore()
        relayDevices[id] = store
        return store
    }
}

/// A key store that lives and dies with the process.
///
/// Deliberately not persisted: a UI test that left a key behind would be
/// indistinguishable, on the next run, from a developer's real vault.
final class InMemoryVaultRootStore: VaultRootStore, @unchecked Sendable {
    private let lock = NSLock()
    private var root: Data?

    func load() throws -> Data? {
        lock.lock()
        defer { lock.unlock() }
        return root
    }

    func store(_ root: Data) throws {
        lock.lock()
        defer { lock.unlock() }
        self.root = root
    }

    func clear() throws {
        lock.lock()
        defer { lock.unlock() }
        root = nil
    }
}

/// The refusal ``UITestHarness/failCaptureFlag`` injects into a quick capture.
///
/// Its own type rather than a `CaptureError` case, so the production error
/// enum carries no case a release build could never produce.
struct RefusedByUITestHarness: LocalizedError, Equatable {
    var errorDescription: String? { "The UI-test harness refused this capture." }
}
#endif
