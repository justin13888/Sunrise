import Foundation
import Security
import Testing

@testable import Sunrise

/// #284: a Keychain that was reached and refused is not a device that never
/// registered. The id may be sitting in the store, so `resolve` throws, and
/// `bind` registers nothing on top of it. Beside `RelayDeviceStoreTests`
/// rather than inside it, which sits on the `file_length` ceiling, and for the
/// same reason `AccountRefusalTests` is beside the credential's tests: these
/// cases are about what a refusal means, not about where the id lives.
struct RelayDeviceRefusalTests {
    private static let id = "01J8ZQ7X9K3M5N7P9R1T3V5W7Y"
    private static let scope = RelayScopeFixture.scope

    /// The one refusal read as no id: both copies were read and disagree, so
    /// nothing sits unread, and the registration that follows is what
    /// collapses them. Throwing here would leave that state with no remedy.
    @Test
    func twoDisagreeingCopiesResolveToNothing() throws {
        let store = FailingRelayDeviceIDStore(loadRefusal: .migrationUnverified)
        #expect(try RelayDeviceID.resolve(store: store, scope: Self.scope, environment: [:]) == nil)
    }

    /// Every other refusal is passed through, whichever keychain raised it.
    @Test(arguments: [
        KeychainError.unexpected(errSecInteractionNotAllowed),
        .unexpected(errSecUserCanceled),
        .accessibilityNotRaised(errSecInteractionNotAllowed),
        .otherDomainUnreadable(errSecInteractionNotAllowed),
        .malformedItem
    ])
    func everyOtherRefusalIsThrown(refusal: KeychainError) {
        let store = FailingRelayDeviceIDStore(loadRefusal: refusal)
        #expect(throws: refusal) {
            try RelayDeviceID.resolve(store: store, scope: Self.scope, environment: [:])
        }
    }

    /// The override never reaches the store, so a refusing store cannot take
    /// away an id somebody set by hand.
    @Test
    func theOverrideWinsOverARefusingStore() throws {
        #expect(
            try RelayDeviceID.resolve(
                store: FailingRelayDeviceIDStore(),
                scope: Self.scope,
                environment: [RelayDeviceID.environmentKey: "dev_override"]
            ) == "dev_override"
        )
    }

    /// A Keychain that would not *read* may be holding the id this device is
    /// bound by. Registering on top of it is a fresh relay row per sync start
    /// for as long as the refusal lasts, so `bind` throws first.
    @Test
    func aStoreThatRefusesTheReadDoesNotRegister() async throws {
        await #expect(throws: FailingRelayDeviceIDStore.refusal) {
            _ = try await RelayDeviceRegistration.bind(
                store: FailingRelayDeviceIDStore(),
                scope: Self.scope,
                environment: [:]
            ) {
                Issue.record("a refused read must not register")
                return Self.id
            }
        }
    }

    /// Two disagreeing copies are the refusal registration repairs: the
    /// store's cross-domain write replaces both, and nothing else would. The
    /// store answers no id, so the one `bind` returns is the one it registered.
    @Test
    func twoDisagreeingCopiesRegisterAgain() async throws {
        let id = try await RelayDeviceRegistration.bind(
            store: FailingRelayDeviceIDStore(loadRefusal: .migrationUnverified),
            scope: Self.scope,
            environment: [:]
        ) { Self.id }
        #expect(id == Self.id)
    }
}

/// A store whose writes the Keychain refuses, and whose reads it refuses too
/// unless `loadRefusal` is `nil`, which answers "never registered".
struct FailingRelayDeviceIDStore: RelayDeviceIDStore {
    /// What a locked keychain or a dismissed access prompt answers.
    static let refusal = KeychainError.unexpected(errSecInteractionNotAllowed)

    var loadRefusal: KeychainError? = Self.refusal

    func load() throws -> RelayDeviceBinding? {
        if let loadRefusal { throw loadRefusal }
        return nil
    }

    func store(_ binding: RelayDeviceBinding) throws { throw Self.refusal }
    func clear() throws {}
}
