import Foundation
import Security
import Testing

@testable import Sunrise

/// Against the real Keychain, like the vault root's tests and for the same
/// reason: where this id lives is the decision under test, and only the
/// platform can say what it recorded. Each case uses its own account name and
/// removes it, so nothing is left in the developer's login keychain.
struct KeychainRelayDeviceIDStoreTests {
    private static let binding = RelayDeviceBinding(
        id: "dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y",
        scope: RelayDeviceScope(relayURL: "https://relay.example", bearer: RelayScopeFixture.alice)
    )

    private func scratch() -> KeychainRelayDeviceIDStore {
        KeychainRelayDeviceIDStore(vaultName: "tests-\(UUID().uuidString)")
    }

    @Test
    func aBindingSurvivesBeingRecordedAndReadBack() throws {
        let store = scratch()
        defer { try? store.clear() }

        #expect(try store.load() == nil, "an unregistered device has no id")
        try store.store(Self.binding)
        #expect(try store.load() == Self.binding, "the id comes back with the scope it was minted in")
        try store.clear()
        #expect(try store.load() == nil)
    }

    /// Whitespace off a copy-paste must not become part of the id: the header
    /// it lands in names a relay row exactly or names nothing.
    @Test
    func surroundingWhitespaceIsNotPartOfTheID() throws {
        let store = scratch()
        defer { try? store.clear() }

        let padded = "  dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y\n"
        try store.store(RelayDeviceBinding(id: padded, scope: Self.binding.scope))
        #expect(try store.load()?.id == "dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y")
    }

    /// An empty id would put an `X-Sunrise-Device` on the wire naming no row,
    /// and the relay answers that as a bad *bearer* so a caller cannot
    /// enumerate an account's devices. This is the only place it can be told
    /// apart from a real failure, so it is refused here.
    @Test
    func anEmptyIDIsRefusedRatherThanRecorded() throws {
        let store = scratch()
        defer { try? store.clear() }

        #expect(throws: RelayDeviceIDError.empty) {
            try store.store(RelayDeviceBinding(id: "   ", scope: Self.binding.scope))
        }
        #expect(try store.load() == nil)
    }

    /// A bare id with no scope recorded cannot be shown to name a row on the
    /// relay sync now reaches, so it reads as unregistered and the device
    /// registers again rather than presenting it.
    @Test
    func aBareIDWithNoScopeReadsAsUnregistered() throws {
        let vaultName = "tests-\(UUID().uuidString)"
        let item = KeychainItem(
            service: KeychainRelayDeviceIDStore.service,
            account: vaultName,
            accessibility: KeychainRelayDeviceIDStore.accessibility,
            domain: KeychainDomain.current
        )
        defer { try? item.deleteAcrossDomains() }
        try item.write(Data("dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y".utf8))

        #expect(try KeychainRelayDeviceIDStore(vaultName: vaultName).load() == nil)
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

    /// The guard the Keychain migration needs on every store that runs one.
    /// `load` migrates this item before it raises its class, and on every
    /// configuration this repository can build the source and the destination
    /// are two names for one stored item — which a migration that did not
    /// notice would verify against itself and then delete. Read twice, because
    /// the failure only shows on the launch after.
    @Test
    func anIDInTheLoginKeychainSurvivesTheMigrationOnEveryLoad() throws {
        let vaultName = "tests-\(UUID().uuidString)"
        let asAnOlderBuildWroteIt = KeychainItem(
            service: KeychainRelayDeviceIDStore.service,
            account: vaultName,
            accessibility: KeychainRelayDeviceIDStore.accessibility,
            domain: .login
        )
        defer { try? asAnOlderBuildWroteIt.delete() }
        try asAnOlderBuildWroteIt.write(JSONEncoder().encode(Self.binding))

        let store = KeychainRelayDeviceIDStore(vaultName: vaultName)
        #expect(try store.load() == Self.binding)
        #expect(try store.load() == Self.binding)
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
    private let scope = RelayScopeFixture.scope

    @Test
    func theEnvironmentOverrideBeatsTheStoredID() {
        let store = RelayScopeFixture.store(id: "dev_stored")
        #expect(
            RelayDeviceID.resolve(
                store: store,
                scope: scope,
                environment: [RelayDeviceID.environmentKey: "dev_override"]
            ) == "dev_override"
        )
    }

    @Test
    func theStoredIDIsUsedWhenNothingOverridesIt() {
        let store = RelayScopeFixture.store(id: "dev_stored")
        #expect(RelayDeviceID.resolve(store: store, scope: scope, environment: [:]) == "dev_stored")
    }

    /// An unregistered device resolves to nothing rather than to an empty
    /// string, which is what keeps `SyncPlan` from putting a header naming no
    /// row on the wire.
    @Test
    func anUnregisteredDeviceResolvesToNothing() {
        #expect(
            RelayDeviceID.resolve(store: InMemoryRelayDeviceIDStore(), scope: scope, environment: [:]) == nil
        )
    }

    /// A blank override is not an override. Exporting the variable empty is
    /// how a shell says "unset", and reading it as a binding would un-bind a
    /// device that has a perfectly good stored id.
    @Test
    func aBlankOverrideFallsBackToTheStoredID() {
        let store = RelayScopeFixture.store(id: "dev_stored")
        #expect(
            RelayDeviceID.resolve(
                store: store,
                scope: scope,
                environment: [RelayDeviceID.environmentKey: "   "]
            ) == "dev_stored"
        )
    }

    /// A store that cannot answer must not be read as "unregistered" in a way
    /// that throws; it degrades to an unbound driver, which is the same state
    /// an unregistered device is in.
    @Test
    func aStoreThatThrowsResolvesToNothing() {
        let failing = FailingRelayDeviceIDStore()
        #expect(RelayDeviceID.resolve(store: failing, scope: scope, environment: [:]) == nil)
    }

    /// #183's review defect: the relay looks the id up under the account on
    /// the relay that minted it, so after the relay URL changes the stored id
    /// names no row, and the relay answers every request as a bad bearer.
    @Test
    func anIDMintedOnAnotherRelayIsNotPresented() {
        let store = RelayScopeFixture.store(id: "dev_stored")
        let moved = RelayDeviceScope(relayURL: "https://other.example", bearer: RelayScopeFixture.alice)
        #expect(RelayDeviceID.resolve(store: store, scope: moved, environment: [:]) == nil)
    }

    /// The same defect after signing in to another account on the same relay.
    @Test
    func anIDMintedForAnotherAccountIsNotPresented() {
        let store = RelayScopeFixture.store(id: "dev_stored")
        let other = RelayDeviceScope(relayURL: "https://relay.example", bearer: RelayScopeFixture.bob)
        #expect(RelayDeviceID.resolve(store: store, scope: other, environment: [:]) == nil)
    }

    /// Signed out, the driver presents no bearer, so no account: an id minted
    /// under one is not presented either.
    @Test
    func anIDMintedUnderABearerIsNotPresentedSignedOut() {
        let store = RelayScopeFixture.store(id: "dev_stored")
        let signedOut = RelayDeviceScope(relayURL: "https://relay.example", bearer: nil)
        #expect(RelayDeviceID.resolve(store: store, scope: signedOut, environment: [:]) == nil)
    }
}

/// Which relay and which account a scope names.
struct RelayDeviceScopeTests {
    /// A renewed token is the same subject, and must not cost the binding.
    @Test
    func aRenewedTokenForTheSameSubjectIsTheSameAccount() {
        let renewed = RelayScopeFixture.jwt(iss: "https://id.example", sub: "alice", extra: #","exp":2"#)
        #expect(
            RelayDeviceScope(relayURL: "https://relay.example", bearer: renewed)
                == RelayScopeFixture.scope
        )
    }

    @Test
    func theAccountIsTheIssuerAndSubject() {
        #expect(RelayDeviceScope.account(ofBearer: RelayScopeFixture.alice) == "https://id.example\u{0}alice")
        let otherIssuer = RelayScopeFixture.jwt(iss: "https://other-id.example", sub: "alice")
        #expect(RelayDeviceScope.account(ofBearer: otherIssuer) != RelayDeviceScope.account(
            ofBearer: RelayScopeFixture.alice
        ))
    }

    /// An opaque token carries no claims to read; the scope then rests on the
    /// relay URL alone rather than failing.
    @Test
    func anOpaqueBearerHasNoReadableAccount() {
        #expect(RelayDeviceScope.account(ofBearer: "opaque-token") == nil)
        #expect(RelayDeviceScope.account(ofBearer: "a.!!!.c") == nil)
    }

    /// A trailing slash or stray whitespace in Settings is not another relay.
    @Test
    func theRelayURLIsComparedWithoutTrailingSlashesOrWhitespace() {
        #expect(
            RelayDeviceScope(relayURL: " https://relay.example/ \n", bearer: RelayScopeFixture.alice)
                == RelayScopeFixture.scope
        )
    }
}

/// Bearers and a scope the tests above share.
enum RelayScopeFixture {
    static let alice = jwt(iss: "https://id.example", sub: "alice")
    static let bob = jwt(iss: "https://id.example", sub: "bob")
    static let scope = RelayDeviceScope(relayURL: "https://relay.example", bearer: alice)

    /// An unsigned JWT: only the payload's claims matter to the scope.
    static func jwt(iss: String, sub: String, extra: String = "") -> String {
        let payload = Data(#"{"iss":"\#(iss)","sub":"\#(sub)"\#(extra)}"#.utf8)
            .base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
        return "eyJhbGciOiJub25lIn0.\(payload).sig"
    }

    static func store(id: String) -> InMemoryRelayDeviceIDStore {
        InMemoryRelayDeviceIDStore(binding: RelayDeviceBinding(id: id, scope: scope))
    }
}

/// The two paths that now write the relay id (#183), over closures standing in
/// for the relay, so the order each one depends on is pinned without a network.
@MainActor
struct RelayDeviceRegistrationTests {
    private static let id = "01J8ZQ7X9K3M5N7P9R1T3V5W7Y"
    private static let code = "abandon ability able about"
    private static let scope = RelayScopeFixture.scope

    private struct RelayRefused: Error {}

    private static func outcome(code: String?) -> AccountBootstrap {
        AccountBootstrap(
            identityId: "acct",
            email: "alice@example.com",
            deviceId: id,
            recoveryCode: code
        )
    }

    /// The defect itself: the id the relay returned used to be dropped with
    /// everything but the code.
    @Test
    func publishingRecordsTheRelayIDAndReturnsTheCode() async throws {
        let store = InMemoryRelayDeviceIDStore()
        let code = try await RelayDeviceRegistration.publish(store: store, scope: Self.scope) {
            Self.outcome(code: Self.code)
        }
        #expect(code == Self.code)
        #expect(try store.load() == RelayDeviceBinding(id: Self.id, scope: Self.scope))
    }

    /// A paired device publishing has no code to show and an id to keep all
    /// the same.
    @Test
    func aDeviceWithNoCodeStillRecordsItsID() async throws {
        let store = InMemoryRelayDeviceIDStore()
        let code = try await RelayDeviceRegistration.publish(store: store, scope: Self.scope) {
            Self.outcome(code: nil)
        }
        #expect(code == nil)
        #expect(try store.load() == RelayDeviceBinding(id: Self.id, scope: Self.scope))
    }

    /// An id recorded for a registration the relay refused names no row, and
    /// the relay would answer it as a bad bearer.
    @Test
    func aRefusedPublicationRecordsNothing() async throws {
        let store = InMemoryRelayDeviceIDStore()
        await #expect(throws: RelayRefused.self) {
            _ = try await RelayDeviceRegistration.publish(store: store, scope: Self.scope) {
                throw RelayRefused()
            }
        }
        #expect(try store.load() == nil)
    }

    /// The relay holds the blob by now, and the code is the only thing that
    /// opens it. A Keychain that will not take the id must not cost the user
    /// the code: a retry would seal a new seed the relay refuses.
    @Test
    func aStoreThatRefusesTheIDDoesNotCostTheCode() async throws {
        let refusing = FailingRelayDeviceIDStore()
        let code = try await RelayDeviceRegistration.publish(store: refusing, scope: Self.scope) {
            Self.outcome(code: Self.code)
        }
        #expect(code == Self.code)
    }

    /// A device already bound is not registered again: that would be a
    /// second relay row for the same key in the list a user revokes from.
    @Test
    func aStoredIDIsUsedWithoutRegistering() async throws {
        let store = RelayScopeFixture.store(id: "dev_stored")
        let id = try await RelayDeviceRegistration.bind(store: store, scope: Self.scope, environment: [:]) {
            Issue.record("a bound device must not register again")
            return Self.id
        }
        #expect(id == "dev_stored")
    }

    /// The override means *this device registered somewhere else*.
    @Test
    func theOverrideIsUsedWithoutRegistering() async throws {
        let store = InMemoryRelayDeviceIDStore()
        let id = try await RelayDeviceRegistration.bind(
            store: store,
            scope: Self.scope,
            environment: [RelayDeviceID.environmentKey: "dev_override"]
        ) {
            Issue.record("an overridden device must not register")
            return Self.id
        }
        #expect(id == "dev_override")
        #expect(try store.load() == nil, "the override is not copied into the store")
    }

    /// The paired device's path: nothing stored, so it registers and keeps
    /// the id — which is what `SessionModel.relayDeviceID(relayURL:bearer:)`
    /// then reads.
    @Test
    func anUnboundDeviceRegistersAndRecordsTheID() async throws {
        let store = InMemoryRelayDeviceIDStore()
        let id = try await RelayDeviceRegistration.bind(store: store, scope: Self.scope, environment: [:]) {
            "  \(Self.id)\n"
        }
        #expect(id == Self.id)
        #expect(try store.load() == RelayDeviceBinding(id: Self.id, scope: Self.scope))
        #expect(RelayDeviceID.resolve(store: store, scope: Self.scope, environment: [:]) == Self.id)
    }

    /// #183's review defect, repaired: an id minted on another relay or for
    /// another account is replaced by registering in the scope sync now runs
    /// in, instead of being presented where it names no row.
    @Test
    func anIDFromAnotherScopeIsReplacedByRegisteringAgain() async throws {
        let store = RelayScopeFixture.store(id: "dev_stale")
        let other = RelayDeviceScope(relayURL: "https://relay.example", bearer: RelayScopeFixture.bob)
        let id = try await RelayDeviceRegistration.bind(store: store, scope: other, environment: [:]) {
            Self.id
        }
        #expect(id == Self.id)
        #expect(try store.load() == RelayDeviceBinding(id: Self.id, scope: other))
        #expect(RelayDeviceID.resolve(store: store, scope: other, environment: [:]) == Self.id)
    }

    /// The ceremony records the id as valid where it published, and nowhere
    /// else: another relay does not inherit it.
    @Test
    func aPublishedIDIsValidOnlyWhereItWasPublished() async throws {
        let store = InMemoryRelayDeviceIDStore()
        _ = try await RelayDeviceRegistration.publish(store: store, scope: Self.scope) {
            Self.outcome(code: Self.code)
        }
        let elsewhere = RelayDeviceScope(relayURL: "https://other.example", bearer: RelayScopeFixture.alice)
        #expect(RelayDeviceID.resolve(store: store, scope: Self.scope, environment: [:]) == Self.id)
        #expect(RelayDeviceID.resolve(store: store, scope: elsewhere, environment: [:]) == nil)
    }

    @Test
    func aRefusedRegistrationRecordsNothing() async throws {
        let store = InMemoryRelayDeviceIDStore()
        await #expect(throws: RelayRefused.self) {
            _ = try await RelayDeviceRegistration.bind(store: store, scope: Self.scope, environment: [:]) {
                throw RelayRefused()
            }
        }
        #expect(try store.load() == nil)
    }

    /// An empty id would put a header naming no row on the wire.
    @Test
    func anEmptyRegisteredIDIsRefused() async throws {
        let store = InMemoryRelayDeviceIDStore()
        await #expect(throws: RelayDeviceIDError.empty) {
            _ = try await RelayDeviceRegistration.bind(store: store, scope: Self.scope, environment: [:]) {
                "  "
            }
        }
        #expect(try store.load() == nil)
    }

    /// A Keychain that will not take the id leaves nothing stored, so the next
    /// sync start registers again rather than the device staying unbound.
    @Test
    func aStoreThatRefusesTheIDStillReturnsIt() async throws {
        let id = try await RelayDeviceRegistration.bind(
            store: FailingRelayDeviceIDStore(),
            scope: Self.scope,
            environment: [:]
        ) { Self.id }
        #expect(id == Self.id)
    }
}

private struct FailingRelayDeviceIDStore: RelayDeviceIDStore {
    func load() throws -> RelayDeviceBinding? { throw RelayDeviceIDError.empty }
    func store(_ binding: RelayDeviceBinding) throws { throw RelayDeviceIDError.empty }
    func clear() throws {}
}
