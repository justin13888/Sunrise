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

    /// The launch argument a UI test passes, followed by a relay's base URL,
    /// to restore an account from its recovery code against that relay (#458).
    ///
    /// The relay is the test runner's, not this process's: an app cannot host
    /// `sunrise-server`, and an iOS test runner cannot start a process, so
    /// whoever runs the suite starts one and names it in the runner's
    /// environment (``RelaySeed``). It must run the self-host `NullVerifier`,
    /// the one configuration exempt from the recovery step-up, which is what
    /// lets ``stepUpBearer(arguments:)`` stand in for the browser sign-in.
    ///
    /// Under this flag the session is the single-vault one, with its vault at
    /// `scratch/vault`, its settings at `scratch/settings` pointing at the
    /// relay, and the relay seeded at launch with an account holding one task
    /// (``RelaySeed/found(relayURL:in:appVersion:)``). Opt **in**, for the
    /// same reason as ``recoveryFlag``, and ignored without a scratch vault.
    static let relayFlag = "-sunrise-ui-test-relay"

    /// The `UserDefaults` suite the relay session's settings read, emptied at
    /// launch. Never `.standard`: `AppSettings` moves what it finds there into
    /// its directory and deletes it, and those are the developer's values.
    static let relaySettingsSuite = "dev.sunrise.ui-test.relay-settings"

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

    /// The relay this process was launched against, or `nil`.
    ///
    /// `nil` without a scratch vault, whatever else was passed, exactly as
    /// ``usesMultiVault(arguments:)`` is: a stray argument on a real launch
    /// can neither point the developer's vault at a relay nor skip its
    /// sign-in.
    static func relay(
        arguments: [String] = ProcessInfo.processInfo.arguments
    ) -> String? {
        guard scratchVault(arguments: arguments) != nil,
              let index = arguments.firstIndex(of: relayFlag),
              arguments.index(after: index) < arguments.endIndex else { return nil }
        let url = arguments[arguments.index(after: index)].trimmed
        return url.isEmpty ? nil : url
    }

    /// The bearer a restore presents in place of the browser step-up, or `nil`
    /// for every launch not under ``relayFlag``.
    ///
    /// A self-host relay's `NullVerifier` accepts any bearer and exempts the
    /// recovery blob from the step-up, so a fixed one is the whole of the
    /// sign-in there, and it is the only relay this flag is for.
    static func stepUpBearer(
        arguments: [String] = ProcessInfo.processInfo.arguments
    ) -> String? {
        relay(arguments: arguments) == nil ? nil : RelaySeed.bearer
    }

    /// The session for ``relayFlag``: one vault, its settings in the scratch
    /// directory pointed at `relayURL`, and the relay seeded in the
    /// background.
    @MainActor
    private static func relaySession(scratch: URL, relayURL: String, appVersion: String) -> SessionModel {
        UserDefaults.standard.removePersistentDomain(forName: relaySettingsSuite)
        guard let defaults = UserDefaults(suiteName: relaySettingsSuite) else {
            preconditionFailure("\(relaySettingsSuite) is neither the app's domain nor the global one")
        }
        let settingsDirectory = scratch.appending(path: "settings")
        try? FileManager.default.createDirectory(at: settingsDirectory, withIntermediateDirectories: true)
        let settings = AppSettings(defaults: defaults, directory: settingsDirectory)
        settings.relayURL = relayURL
        settings.accountEmail = RelaySeed.email
        _Concurrency.Task.detached {
            await RelaySeed.seed(relayURL: relayURL, scratch: scratch, appVersion: appVersion)
        }
        return SessionModel(
            location: VaultLocation(directory: scratch.appending(path: "vault")),
            rootStore: InMemoryVaultRootStore(),
            appVersion: appVersion,
            settingsDefaults: defaults,
            settingsDirectory: settingsDirectory
        )
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
        if let relayURL = relay(arguments: arguments) {
            return relaySession(scratch: scratch, relayURL: relayURL, appVersion: appVersion)
        }
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

/// The account ``UITestHarness/relayFlag`` founds on the relay, and how the
/// test runner learns its recovery code (#458).
///
/// Founded through the app's own seam, the sequence
/// `crates/sunrise-e2e/tests/apple_recovers_an_account.rs` drives from Rust:
/// open a vault, publish it, write one task, push it, close it. The founding
/// vault is `scratch/founder`, beside the session's `scratch/vault`, and is
/// shut down before the code is handed over, so the restore that follows
/// finds the account exactly as a user who lost that device would.
///
/// The code is drawn by the core, not fixed: it is written to ``codeFile`` in
/// the scratch directory, which the test runner created and reads back. A
/// founding that failed writes why to ``failureFile`` instead, so the test
/// names the cause rather than timing out on a file that never came.
enum RelaySeed {
    /// The bearer every call presents. `NullVerifier` reads none of it.
    static let bearer = "self-host"
    /// The address the account is registered under: the route refuses an
    /// empty one, and `NullVerifier` emits no `email` claim to fall back on.
    static let email = "ui-test@example.com"
    /// The task the founding device writes, which a restore must read back.
    /// `SunriseUITests` and `SunriseiOSUITests` assert this literal.
    static let taskTitle = "Renew passport"
    /// Where in the scratch directory the recovery code is written.
    static let codeFile = "seeded-recovery-code"
    /// Where in the scratch directory a failed founding says why.
    static let failureFile = "seed-failure"

    /// Why the founding device did not leave a restorable account behind.
    enum SeedError: LocalizedError, Equatable {
        case notPushed(outboxPending: UInt32)
        case noRecoveryCode

        var errorDescription: String? {
            switch self {
            case let .notPushed(pending):
                "the founding device did not push its task (\(pending) ops still in its outbox)"
            case .noRecoveryCode:
                "the relay founded the account without a recovery code"
            }
        }
    }

    /// Found the account and write the code, or the failure, into `scratch`.
    static func seed(relayURL: String, scratch: URL, appVersion: String) async {
        do {
            let code = try await found(
                relayURL: relayURL,
                in: scratch.appending(path: "founder"),
                appVersion: appVersion
            )
            try Data(code.utf8).write(to: scratch.appending(path: codeFile), options: .atomic)
        } catch {
            let reason = "\(error.localizedDescription) (\(error))"
            try? Data(reason.utf8).write(to: scratch.appending(path: failureFile), options: .atomic)
        }
    }

    /// Found an account on `relayURL` from a vault at `directory`, write
    /// ``taskTitle``, push it, close the vault, and return the recovery code.
    static func found(relayURL: String, in directory: URL, appVersion: String) async throws -> String {
        let bridge = try await CoreBridge.open(
            directory: directory,
            vaultRoot: VaultRoot.generate(),
            appVersion: appVersion
        )
        do {
            let account = try await bridge.bootstrapAccount(
                relayURL: relayURL,
                bearer: bearer,
                email: email,
                nickname: "UI-test founding device",
                termsAcceptedAtMs: bridge.nowMs()
            )
            let preview = try await bridge.previewCapture(taskTitle, timeZone: TimeZone.current.identifier)
            _ = try await bridge.submit(.createTask(draft: preview.draft))
            let pushed = try await bridge.syncOnce(
                url: relayURL,
                bearer: bearer,
                relayDeviceID: account.deviceId,
                budgetMs: 30000
            )
            guard pushed.completed, pushed.outboxPending == 0 else {
                throw SeedError.notPushed(outboxPending: pushed.outboxPending)
            }
            guard let code = account.recoveryCode else { throw SeedError.noRecoveryCode }
            await bridge.shutdown()
            return code
        } catch {
            await bridge.shutdown()
            throw error
        }
    }
}
#endif
