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

    @Test
    func aVaultThatWillNotOpenIsAFailureNotAFirstRun() async {
        let directory = scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }

        let session = SessionModel(
            location: VaultLocation(directory: directory),
            rootStore: StubRootStore(root: Data(repeating: 9, count: 32)),
            appVersion: "test",
            configurationError: nil,
            openBridge: { _, _, _ in throw StubError() }
        )
        await session.start()
        #expect(session.phase == .failed("the Keychain is locked"))
    }
}
