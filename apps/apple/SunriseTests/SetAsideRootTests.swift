import Foundation
import Testing

@testable import Sunrise

/// The root a restore replaces (#1222): a vault that would not open is moved
/// aside and a new root stored, so the root it was written under is kept under
/// the set-aside directory's name, and the vault moved aside still opens.
@MainActor
struct SetAsideRootTests {
    private static let oldRoot = Data(repeating: 7, count: 32)

    /// Refuses the first open as a vault no build upgrades, then opens.
    private final class TooOldOnce: @unchecked Sendable {
        private let lock = NSLock()
        private var refused = false

        func open(_ dir: URL, _ root: Data, _ version: String, _ bundle: Data?) async throws -> CoreBridge {
            let refuse = lock.withLock {
                defer { refused = true }
                return !refused
            }
            if refuse { throw BindingError.VaultTooOld(message: "too old") }
            return try await CoreBridge.open(
                directory: dir, vaultRoot: root, appVersion: version, pairedBundle: bundle
            )
        }
    }

    private let request = RecoveryRequest(
        relayURL: "https://relay.example", bearer: "b", code: "c", nickname: "n"
    )

    /// A closed vault under ``oldRoot``, alone in a parent removed after.
    private func vault() async throws -> URL {
        let dir = FileManager.default.temporaryDirectory
            .appending(path: "sunrise-tests-\(UUID().uuidString)/vault")
        let made = try await CoreBridge.open(directory: dir, vaultRoot: Self.oldRoot, appVersion: "test")
        await made.shutdown()
        return dir
    }

    /// The directories the restore moved `directory` aside to.
    private func setAside(beside directory: URL) throws -> [URL] {
        let parent = directory.deletingLastPathComponent()
        return try FileManager.default.contentsOfDirectory(atPath: parent.path(percentEncoded: false))
            .filter { $0.hasPrefix("\(directory.lastPathComponent).set-aside-") }
            .map { parent.appending(path: $0) }
    }

    /// A session at the repair screen of a vault that would not open.
    private func failedSession(_ directory: URL, store: StubRootStore) async throws -> SessionModel {
        let relayIDs = InMemoryRelayDeviceIDStore()
        try relayIDs.store(RelayDeviceBinding(
            id: "01RELAY",
            scope: RelayDeviceScope(relayURL: "https://relay.example", bearer: "b")
        ))
        let opener = TooOldOnce()
        let session = SessionModel(
            location: VaultLocation(directory: directory),
            rootStore: store,
            appVersion: "test",
            relayDeviceStore: relayIDs,
            openBridge: { try await opener.open($0, $1, $2, $3) }
        )
        await session.start()
        #expect(session.canRestoreFromRecoveryCode)
        return session
    }

    @Test
    func aRestoreOverAVaultThatWouldNotOpenKeepsTheRootThatOpensIt() async throws {
        let directory = try await vault()
        defer { try? FileManager.default.removeItem(at: directory.deletingLastPathComponent()) }
        let store = StubRootStore(root: Self.oldRoot)
        let session = try await failedSession(directory, store: store)
        try await session.restoreAccount(
            request,
            onStep: { _ in },
            recover: { dir, root, version, _, _ in
                try await CoreBridge.open(directory: dir, vaultRoot: root, appVersion: version)
            }
        )
        #expect(session.phase == .unlocked)
        await session.lock()

        let newRoot = try #require(store.stored)
        #expect(newRoot != Self.oldRoot, "the restore stored a root of its own")
        let aside = try #require(try setAside(beside: directory).first)
        let kept = store.forSetAside(aside.lastPathComponent)
        #expect(try kept.load() == Self.oldRoot)
        // The proof that matters: the set-aside vault opens under what was kept.
        let opened = try await CoreBridge.open(
            directory: aside, vaultRoot: try #require(try kept.load()), appVersion: "test"
        )
        await opened.shutdown()
    }

    /// The core reuses a set-aside name once its directory has gone, so a
    /// user who renamed `vault.set-aside-1` frees that name while its root is
    /// still kept under it. The next restore keeps its root under a fresh
    /// account, and the earlier one is left alone.
    @Test
    func aRestoreOverAReusedSetAsideNameKeepsTheEarlierRoot() async throws {
        let earlierRoot = Data(repeating: 9, count: 32)
        let directory = try await vault()
        defer { try? FileManager.default.removeItem(at: directory.deletingLastPathComponent()) }
        let store = StubRootStore(root: Self.oldRoot)
        let reused = "\(directory.lastPathComponent).set-aside-1"
        try store.forSetAside(reused).store(earlierRoot)
        let session = try await failedSession(directory, store: store)
        try await session.restoreAccount(
            request,
            onStep: { _ in },
            recover: { dir, root, version, _, _ in
                try await CoreBridge.open(directory: dir, vaultRoot: root, appVersion: version)
            }
        )
        await session.lock()

        let aside = try #require(try setAside(beside: directory).first)
        #expect(aside.lastPathComponent == reused, "the core reused the free name")
        #expect(try store.forSetAside(reused).load() == earlierRoot)
        let fresh = store.setAsideNames.filter { $0 != reused }
        try #require(fresh.count == 1)
        #expect(fresh[0].hasPrefix("\(reused)~"))
        let kept = try #require(try store.forSetAside(fresh[0]).load())
        #expect(kept == Self.oldRoot)
        let opened = try await CoreBridge.open(directory: aside, vaultRoot: kept, appVersion: "test")
        await opened.shutdown()
    }

    /// A restore that wrote nothing puts the vault back, where the root in
    /// use still opens it, and keeps no copy for a directory that is gone.
    @Test
    func aRefusedRestorePutsTheVaultBackAndKeepsNoCopy() async throws {
        let directory = try await vault()
        defer { try? FileManager.default.removeItem(at: directory.deletingLastPathComponent()) }
        let store = StubRootStore(root: Self.oldRoot)
        let session = try await failedSession(directory, store: store)
        var asideName: String?
        await #expect(throws: BindingError.self) {
            try await session.restoreAccount(
                self.request,
                onStep: { _ in },
                recover: { dir, _, _, _, _ in
                    asideName = try self.setAside(beside: dir).first?.lastPathComponent
                    throw BindingError.RecoveryCode(message: "recovery code: wrong")
                }
            )
        }

        #expect(store.stored == Self.oldRoot)
        #expect(try setAside(beside: directory).isEmpty, "the vault is back where it was")
        let name = try #require(asideName)
        #expect(try store.forSetAside(name).load() == nil)
    }

    /// A root the Keychain will not hand over may be the one the vault was
    /// written under, so the restore refuses before it writes anything.
    @Test
    func aRootThatCannotBeReadRefusesTheRestore() async throws {
        struct Locked: Error {}
        let directory = try await vault()
        defer { try? FileManager.default.removeItem(at: directory.deletingLastPathComponent()) }
        let store = StubRootStore(root: Self.oldRoot)
        let session = try await failedSession(directory, store: store)
        store.refuseLoads(Locked())

        var recovered = false
        await #expect(throws: Locked.self) {
            try await session.restoreAccount(
                self.request,
                onStep: { _ in },
                recover: { _, _, _, _, _ in
                    recovered = true
                    throw BindingError.RecoveryCode(message: "unreached")
                }
            )
        }

        #expect(!recovered)
        #expect(store.stored == Self.oldRoot)
        #expect(try setAside(beside: directory).isEmpty, "the vault is back where it was")
    }

    /// A set-aside name whose kept root cannot be read may hold the only key
    /// to an earlier directory, so the restore refuses rather than store over
    /// it, and puts the vault back.
    @Test
    func aSetAsideNameThatCannotBeReadRefusesTheRestore() async throws {
        struct Locked: Error {}
        let earlierRoot = Data(repeating: 9, count: 32)
        let directory = try await vault()
        defer { try? FileManager.default.removeItem(at: directory.deletingLastPathComponent()) }
        let store = StubRootStore(root: Self.oldRoot)
        let reused = "\(directory.lastPathComponent).set-aside-1"
        let earlier = try #require(store.forSetAside(reused) as? StubRootStore)
        try earlier.store(earlierRoot)
        earlier.refuseLoads(Locked())
        let session = try await failedSession(directory, store: store)

        var recovered = false
        await #expect(throws: Locked.self) {
            try await session.restoreAccount(
                self.request,
                onStep: { _ in },
                recover: { _, _, _, _, _ in
                    recovered = true
                    throw BindingError.RecoveryCode(message: "unreached")
                }
            )
        }

        #expect(!recovered)
        #expect(store.stored == Self.oldRoot)
        #expect(try setAside(beside: directory).isEmpty, "the vault is back where it was")
        #expect(earlier.stored == earlierRoot, "the earlier root is untouched")
        #expect(store.setAsideNames == [reused], "no fresh account was written")
    }

    /// The key-missing route has no root to lose, and keeps nothing.
    @Test
    func aRestoreOverAVaultWhoseKeyIsMissingKeepsNoRoot() async throws {
        let directory = try await vault()
        defer { try? FileManager.default.removeItem(at: directory.deletingLastPathComponent()) }
        let store = StubRootStore()
        let session = SessionModel(
            location: VaultLocation(directory: directory),
            rootStore: store,
            appVersion: "test"
        )
        await session.start()
        #expect(session.phase == .locked(.keyMissingForExistingVault))

        try await session.restoreAccount(
            request,
            onStep: { _ in },
            recover: { dir, root, version, _, _ in
                try await CoreBridge.open(directory: dir, vaultRoot: root, appVersion: version)
            }
        )
        await session.lock()

        #expect(store.stored?.count == 32)
        let aside = try #require(try setAside(beside: directory).first)
        #expect(try store.forSetAside(aside.lastPathComponent).load() == nil)
    }
}
