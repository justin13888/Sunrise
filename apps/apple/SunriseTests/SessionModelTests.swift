import Foundation
import Testing

@testable import Sunrise

/// A key store the test drives, including the failure the real one can have.
final class StubRootStore: VaultRootStore, @unchecked Sendable {
    private let lock = NSLock()
    private var root: Data?
    private var loadError: Error?
    private var storeError: Error?

    init(root: Data? = nil, loadError: Error? = nil, storeError: Error? = nil) {
        self.root = root
        self.loadError = loadError
        self.storeError = storeError
    }

    var stored: Data? {
        lock.withLock { root }
    }

    func load() throws -> Data? {
        try lock.withLock {
            if let loadError { throw loadError }
            return root
        }
    }

    func store(_ root: Data) throws {
        try lock.withLock {
            if let storeError { throw storeError }
            self.root = root
        }
    }

    func clear() throws {
        lock.withLock { root = nil }
    }
}

private struct StubError: Error, LocalizedError {
    let errorDescription: String? = "the Keychain is locked"
}

@MainActor
struct SessionModelTests {
    private func scratchDirectory() -> URL {
        FileManager.default.temporaryDirectory
            .appending(path: "sunrise-tests-\(UUID().uuidString)")
    }

    private func model(
        directory: URL,
        store: StubRootStore
    ) -> SessionModel {
        SessionModel(
            location: VaultLocation(directory: directory),
            rootStore: store,
            appVersion: "test"
        )
    }

    @Test
    func noKeyAndNoVaultIsAFirstRun() async {
        let directory = scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }

        let session = model(directory: directory, store: StubRootStore())
        await session.start()
        #expect(session.phase == .firstRun)
    }

    /// The case the whole state machine exists for. A Keychain that will not
    /// answer must never read as "no key yet": the repair that follows from
    /// that reading destroys the vault.
    @Test
    func aRefusingKeychainLocksRatherThanLookingLikeAFirstRun() async {
        let directory = scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }

        let session = model(directory: directory, store: StubRootStore(loadError: StubError()))
        await session.start()
        #expect(session.phase == .locked(.keychainUnavailable("the Keychain is locked")))
    }

    /// Restored-from-backup: the encrypted data came across, the Keychain item
    /// did not.
    @Test
    func aVaultWithNoKeyLocksAndRefusesToCreateANewOne() async throws {
        let directory = scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        try Data("existing".utf8).write(to: directory.appending(path: "sunrise.sqlite"))

        let store = StubRootStore()
        let session = model(directory: directory, store: store)
        await session.start()
        #expect(session.phase == .locked(.keyMissingForExistingVault))

        await session.createVault()
        #expect(session.phase == .locked(.keyMissingForExistingVault), "creating must be refused")
        #expect(store.stored == nil, "no key may be written over an existing vault")
    }

    @Test
    func creatingAVaultPersistsTheKeyBeforeTheVaultIsUsable() async {
        let directory = scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }

        let store = StubRootStore()
        let session = model(directory: directory, store: store)
        await session.start()
        await session.createVault()

        #expect(session.phase == .unlocked)
        #expect(session.bridge != nil)
        #expect(store.stored?.count == 32)
    }

    /// The proof that the key was persisted, not just held: a second model
    /// over the same store and directory opens without being asked.
    @Test
    func aRestartUnlocksWithoutAskingAgain() async {
        let directory = scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }

        let store = StubRootStore()
        let first = model(directory: directory, store: store)
        await first.start()
        await first.createVault()
        #expect(first.phase == .unlocked)
        await first.lock()

        let second = model(directory: directory, store: store)
        await second.start()
        #expect(second.phase == .unlocked)
        await second.lock()
    }

    /// A key that cannot be stored must not produce a vault. Opening one
    /// anyway would work exactly once and be unreadable after quit.
    @Test
    func aKeyThatCannotBeStoredDoesNotProduceAVault() async {
        let directory = scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }

        let session = model(directory: directory, store: StubRootStore(storeError: StubError()))
        await session.start()
        await session.createVault()

        #expect(session.phase == .failed("the Keychain is locked"))
        #expect(session.bridge == nil)
    }

    /// A path the app cannot even resolve is a failure that retrying reports
    /// again, not a first run against a nonsense directory.
    @Test
    func anUnusableConfigurationStaysAFailureAcrossRetries() async {
        let session = SessionModel(
            location: VaultLocation(directory: URL(filePath: "/dev/null")),
            rootStore: StubRootStore(),
            appVersion: "test",
            configurationError: "no Application Support directory"
        )
        await session.start()
        #expect(session.phase == .failed("no Application Support directory"))
        await session.start()
        #expect(session.phase == .failed("no Application Support directory"))
    }

    /// Locking has to land somewhere the window can get out of.
    ///
    /// It used to land on `.starting`, which `RootView` draws as a bare
    /// `ProgressView` — and the `.task` that calls `start()` is attached above
    /// the phase switch, so it fires once for the life of the window and would
    /// never fire again. A "Lock" menu item would have wedged the app on a
    /// spinner. `.locked(.lockedByUser)` is a screen with a button on it.
    @Test
    func lockingLandsOnAScreenWithAWayOut() async {
        let directory = scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }

        let session = model(directory: directory, store: StubRootStore())
        await session.start()
        await session.createVault()
        #expect(session.phase == .unlocked)

        await session.lock()
        #expect(session.phase == .locked(.lockedByUser))
        #expect(session.bridge == nil, "the core must have let the vault lock go")
        #expect(session.phase != .starting, "a phase nothing would move it out of")

        // What the button on that screen does.
        await session.start()
        #expect(session.phase == .unlocked, "unlocking needs no key and no ceremony")
        await session.lock()
    }

    /// A user-closed vault is not a failure, and the screen must not read like
    /// one — nor offer the repair for a problem the user does not have.
    @Test
    func aUserLockSaysNothingWasLostAndOffersToUnlock() {
        let reason = SessionModel.LockReason.lockedByUser
        #expect(reason.repairTitle == "Unlock")
        #expect(SessionModel.LockReason.keyMissingForExistingVault.repairTitle == "Try again")
        #expect(reason.summary.contains("Nothing was lost"))
        #expect(!reason.summary.lowercased().contains("pair"), "there is no key to recover")
    }

    @Test
    func aVaultThatWillNotOpenIsAFailureNotAFirstRun() async {
        let directory = scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }

        let session = SessionModel(
            location: VaultLocation(directory: directory),
            rootStore: StubRootStore(root: Data(repeating: 9, count: 32)),
            appVersion: "test",
            configurationError: nil,
            openBridge: { _, _, _, _ in throw StubError() }
        )
        await session.start()
        #expect(session.phase == .failed("the Keychain is locked"))
    }
}

/// The three screens `RootView` builds that need the session it is showing.
///
/// They used to default to `SessionModel.active`, a memoized process-wide
/// instance, because `RootView` did not pass one down. That worked and was
/// still wrong: the screen that switches vaults has to act on the session the
/// window is actually rendering, and a global is the one thing that cannot be
/// proved to be that. The global is gone; these are the call sites that
/// replaced it, and this is what stops one of them silently regrowing a
/// default.
@MainActor
struct SessionInjectionTests {
    private func session() -> SessionModel {
        SessionModel(
            location: VaultLocation(directory: URL(filePath: "/dev/null")),
            rootStore: StubRootStore(),
            appVersion: "test"
        )
    }

    @Test
    func onboardingAdoptsAPairedRootIntoTheSessionItWasGiven() {
        let model = session()
        let view = OnboardingView(create: {}, session: model)
        #expect(view.session === model)
    }

    @Test
    func theLockedScreenPairsIntoTheSessionItWasGiven() {
        let model = session()
        let view = LockedView(reason: .keyMissingForExistingVault, retry: {}, session: model)
        #expect(view.session === model)
    }

    /// Settings is the sharpest of the three: it both switches vaults and seals
    /// this vault's root to another Mac, and `session.bridge` is what it seals
    /// with. Sealing from a session other than the open one would hand a second
    /// device the wrong key.
    @Test
    func settingsSwitchesAndSealsWithTheSessionItWasGiven() throws {
        let suite = "sunrise-tests-\(UUID().uuidString)"
        let defaults = try #require(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }

        let model = session()
        let view = AccountViewFixture.make(
            settings: AppSettings(defaults: defaults),
            account: AccountModel(),
            notifications: NotificationPreferences(defaults: defaults),
            deviceID: "device",
            authorization: .authorized,
            scheduledCount: 0,
            signIn: {},
            allowNotifications: {},
            keyboard: KeyboardPreferences(defaults: defaults),
            session: model
        )
        #expect(view.session === model)
    }
}
