import Foundation
import Security
import Testing

@testable import Sunrise

/// Against the real Keychain, like the vault root's tests and for the same
/// reason: where this id lives is the decision under test, and only the
/// platform can say what it recorded. Each case uses its own account name and
/// removes it, so nothing is left in the developer's login keychain.
struct KeychainRelayDeviceIDStoreTests {
    private func scratch() -> KeychainRelayDeviceIDStore {
        KeychainRelayDeviceIDStore(vaultName: "tests-\(UUID().uuidString)")
    }

    @Test
    func anIDSurvivesBeingRecordedAndReadBack() throws {
        let store = scratch()
        defer { try? store.clear() }

        #expect(try store.load() == nil, "an unregistered device has no id")
        try store.store("dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y")
        #expect(try store.load() == "dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y")
        try store.clear()
        #expect(try store.load() == nil)
    }

    /// Whitespace off a copy-paste must not become part of the id: the header
    /// it lands in names a relay row exactly or names nothing.
    @Test
    func surroundingWhitespaceIsNotPartOfTheID() throws {
        let store = scratch()
        defer { try? store.clear() }

        try store.store("  dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y\n")
        #expect(try store.load() == "dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y")
    }

    /// An empty id would put an `X-Sunrise-Device` on the wire naming no row,
    /// and the relay answers that as a bad *bearer* so a caller cannot
    /// enumerate an account's devices. This is the only place it can be told
    /// apart from a real failure, so it is refused here.
    @Test
    func anEmptyIDIsRefusedRatherThanRecorded() throws {
        let store = scratch()
        defer { try? store.clear() }

        #expect(throws: RelayDeviceIDError.empty) { try store.store("   ") }
        #expect(try store.load() == nil)
    }

    /// The decision this issue took: the id is kept in the vault root's
    /// protection class, so the id and the signing key it names are present or
    /// absent together. Split them and the client signs with a key the named
    /// row does not hold, which the relay reports as a bad bearer.
    @Test
    func theIDDeclaresTheVaultRootsClass() {
        #expect(KeychainRelayDeviceIDStore.accessibility == KeychainVaultRootStore.accessibility)
        #expect(KeychainRelayDeviceIDStore.accessibility == .afterFirstUnlockThisDeviceOnly)
    }

    /// Its own service, so a user who deletes one Sunrise item in Keychain
    /// Access does not silently take the others with it.
    @Test
    func theIDLivesUnderItsOwnService() {
        #expect(KeychainRelayDeviceIDStore.service != KeychainVaultRootStore.service)
        #expect(KeychainRelayDeviceIDStore.service != KeychainCredentialStore.service)
    }
}

struct RelayDeviceIDResolutionTests {
    /// The CLI's precedence, spelled the same way: an explicit
    /// `SUNRISE_SYNC_DEVICE_ID` beats what is on disk, because it exists for
    /// the case where registration happened somewhere else.
    @Test
    func theEnvironmentOverrideBeatsTheStoredID() {
        let store = InMemoryRelayDeviceIDStore(id: "dev_stored")
        #expect(
            RelayDeviceID.resolve(
                store: store,
                environment: [RelayDeviceID.environmentKey: "dev_override"]
            ) == "dev_override"
        )
    }

    @Test
    func theStoredIDIsUsedWhenNothingOverridesIt() {
        let store = InMemoryRelayDeviceIDStore(id: "dev_stored")
        #expect(RelayDeviceID.resolve(store: store, environment: [:]) == "dev_stored")
    }

    /// An unregistered device resolves to nothing rather than to an empty
    /// string, which is what keeps `SyncPlan` from putting a header naming no
    /// row on the wire.
    @Test
    func anUnregisteredDeviceResolvesToNothing() {
        #expect(RelayDeviceID.resolve(store: InMemoryRelayDeviceIDStore(), environment: [:]) == nil)
    }

    /// A blank override is not an override. Exporting the variable empty is
    /// how a shell says "unset", and reading it as a binding would un-bind a
    /// device that has a perfectly good stored id.
    @Test
    func aBlankOverrideFallsBackToTheStoredID() {
        let store = InMemoryRelayDeviceIDStore(id: "dev_stored")
        #expect(
            RelayDeviceID.resolve(
                store: store,
                environment: [RelayDeviceID.environmentKey: "   "]
            ) == "dev_stored"
        )
    }

    /// A store that cannot answer must not be read as "unregistered" in a way
    /// that throws; it degrades to an unbound driver, which is the same state
    /// an unregistered device is in.
    @Test
    func aStoreThatThrowsResolvesToNothing() {
        #expect(RelayDeviceID.resolve(store: FailingRelayDeviceIDStore(), environment: [:]) == nil)
    }
}

private struct FailingRelayDeviceIDStore: RelayDeviceIDStore {
    func load() throws -> String? { throw RelayDeviceIDError.empty }
    func store(_ id: String) throws { throw RelayDeviceIDError.empty }
    func clear() throws {}
}
