import Foundation
import Testing

@testable import Sunrise

/// Switching between vaults, and taking a vault root from a pairing.
///
/// These run against **real** cores, and that is the whole design of the file.
/// `crates/sunrise-core/src/vault_lock.rs` keeps a process-local registry of
/// canonicalized vault paths and refuses a second `Core::open` on one that is
/// held — so a switcher that opened the new vault before closing the old one
/// would not fail to compile, and would not fail on a stub. It would fail
/// here, thirteen retries later, with a message naming this test process as
/// the holder. A green run *is* the ordering proof.
@MainActor
struct VaultSwitchingTests {
    private func scratchDirectory() -> URL {
        FileManager.default.temporaryDirectory
            .appending(path: "sunrise-tests-\(UUID().uuidString)")
    }

    private func scratchDefaults() throws -> UserDefaults {
        try #require(UserDefaults(suiteName: "sunrise-tests-\(UUID().uuidString)"))
    }

    /// Two vaults, each with its own directory and its own key store — which
    /// is what `KeychainVaultRootStore(vaultName:)` gives the real app once
    /// something finally passes it a name.
    private struct Fixture {
        let session: SessionModel
        let registry: VaultRegistry
        let second: VaultDescriptor
        let firstStore: StubRootStore
        let secondStore: StubRootStore
        let directories: [URL]
    }

    private func fixture() throws -> Fixture {
        let registry = VaultRegistry(defaults: try scratchDefaults())
        let second = registry.add(name: "Work")
        let firstDirectory = scratchDirectory()
        let secondDirectory = scratchDirectory()
        let firstStore = StubRootStore()
        let secondStore = StubRootStore()

        let session = SessionModel(
            vaults: registry,
            appVersion: "test",
            resolve: { descriptor in
                descriptor.id == VaultRegistry.firstVaultID
                    ? VaultBinding(
                        location: VaultLocation(directory: firstDirectory),
                        rootStore: firstStore
                    )
                    : VaultBinding(
                        location: VaultLocation(directory: secondDirectory),
                        rootStore: secondStore
                    )
            }
        )
        return Fixture(
            session: session,
            registry: registry,
            second: second,
            firstStore: firstStore,
            secondStore: secondStore,
            directories: [firstDirectory, secondDirectory]
        )
    }

    private func clean(_ fixture: Fixture) async {
        await fixture.session.lock()
        for directory in fixture.directories {
            try? FileManager.default.removeItem(at: directory)
        }
    }

    /// The second vault cannot open while the first still holds the lock, and
    /// nothing in Swift would have told us. This is the test.
    @Test
    func theOpenVaultIsClosedBeforeTheNextOneIsOpened() async throws {
        let fixture = try fixture()
        let session = fixture.session

        await session.start()
        await session.createVault()
        #expect(session.phase == .unlocked)

        await session.switchTo(fixture.second)
        #expect(session.phase == .firstRun, "a vault with no key and no data is a first run")
        await session.createVault()
        #expect(
            session.phase == .unlocked,
            "the second core could not have opened if the first still held the vault lock"
        )
        await clean(fixture)
    }

    /// Per-vault Keychain accounts, which is the point of the `vaultName`
    /// parameter that has existed unused since the store was written. One
    /// shared account would mean the second vault overwrote the first vault's
    /// key, and the first vault would never open again.
    @Test
    func eachVaultKeepsItsOwnKey() async throws {
        let fixture = try fixture()
        let session = fixture.session

        await session.start()
        await session.createVault()
        let firstKey = fixture.firstStore.stored

        await session.switchTo(fixture.second)
        await session.createVault()

        #expect(fixture.firstStore.stored == firstKey, "the first key was not written over")
        #expect(fixture.secondStore.stored != nil)
        #expect(fixture.firstStore.stored != fixture.secondStore.stored)
        await clean(fixture)
    }

    /// Coming back has to work, and has to need no ceremony: the key is where
    /// it was, so the vault simply opens.
    @Test
    func switchingBackReopensTheFirstVaultWithoutAskingAnything() async throws {
        let fixture = try fixture()
        let session = fixture.session
        let first = try #require(fixture.registry.vaults.first)

        await session.start()
        await session.createVault()
        await session.switchTo(fixture.second)
        await session.createVault()

        await session.switchTo(first)
        #expect(session.phase == .unlocked)
        #expect(fixture.registry.selectedID == first.id)
        await clean(fixture)
    }

    /// The switcher and the open core must not be able to disagree about which
    /// vault this is.
    @Test
    func theRegistryFollowsWhatIsActuallyOpen() async throws {
        let fixture = try fixture()
        let session = fixture.session

        await session.start()
        await session.createVault()
        #expect(fixture.registry.selectedID == VaultRegistry.firstVaultID)

        await session.switchTo(fixture.second)
        #expect(fixture.registry.selectedID == fixture.second.id)
        #expect(session.vaults?.selected.name == "Work")
        await clean(fixture)
    }

    @Test
    func addingAVaultRegistersItAndOpensItAsAFirstRun() async throws {
        let fixture = try fixture()
        let session = fixture.session

        await session.start()
        await session.createVault()

        await session.addVault(named: "Personal")
        #expect(fixture.registry.vaults.count == 3)
        #expect(session.phase == .firstRun, "a brand new vault has nothing in it yet")
        await clean(fixture)
    }

    /// The counterpart to `createVault`, and the reason
    /// `LockReason.keyMissingForExistingVault` is allowed to tell the user to
    /// go and pair: a root that came from another device is the one root that
    /// may legitimately be written where a vault already is.
    @Test
    func aPairedRootIsStoredBeforeItIsUsed() async throws {
        let directory = scratchDirectory()
        let store = StubRootStore()
        defer { try? FileManager.default.removeItem(at: directory) }

        let session = SessionModel(
            location: VaultLocation(directory: directory),
            rootStore: store,
            appVersion: "test"
        )
        await session.start()
        #expect(session.phase == .firstRun)

        let root = try VaultRoot.generate()
        await session.adoptPairing(root: root, bundle: try PairingFixture.payload())

        #expect(session.phase == .unlocked)
        #expect(store.stored == root, "a root that was never persisted is unreadable next launch")
        await session.lock()
    }

    @Test
    func aRootThatIsNotAVaultKeyIsRefusedWithoutBeingStored() async throws {
        let directory = scratchDirectory()
        let store = StubRootStore()
        defer { try? FileManager.default.removeItem(at: directory) }

        let session = SessionModel(
            location: VaultLocation(directory: directory),
            rootStore: store,
            appVersion: "test"
        )
        await session.start()
        await session.adoptPairing(
            root: Data(repeating: 3, count: 16),
            bundle: try PairingFixture.payload()
        )

        #expect(store.stored == nil)
        if case .failed = session.phase {} else {
            Issue.record("expected a failure, got \(session.phase)")
        }
    }

    /// An open vault already has a working key and a live core holding the
    /// lock. Writing another one over it is the destructive operation this
    /// whole state machine exists to refuse.
    @Test
    func adoptingIsRefusedWhileAVaultIsOpen() async throws {
        let directory = scratchDirectory()
        let store = StubRootStore()
        defer { try? FileManager.default.removeItem(at: directory) }

        let session = SessionModel(
            location: VaultLocation(directory: directory),
            rootStore: store,
            appVersion: "test"
        )
        await session.start()
        await session.createVault()
        let key = store.stored

        await session.adoptPairing(
            root: try VaultRoot.generate(),
            bundle: try PairingFixture.payload()
        )
        #expect(session.phase == .unlocked)
        #expect(store.stored == key, "the working key was left alone")
        await session.lock()
    }
}
