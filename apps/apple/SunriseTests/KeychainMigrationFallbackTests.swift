import Foundation
import Testing

@testable import Sunrise

/// The fallback arm, driven by a `Security` call that really fails.
///
/// Every case in `KeychainMigrationTests` runs `.login` → `.login`, where
/// nothing refuses, so until this suite existed the arm that rescues a secret
/// from a failed destination executed in no test at all. The one deterministic refusal this repository
/// can produce is the one the probe already leans on: an ad-hoc-signed Mac with
/// no `keychain-access-groups` entitlement cannot reach the data-protection
/// keychain, so a destination addressed there fails for real.
///
/// Mac-only by necessity, not by choice. On iOS the data-protection keychain is
/// the only keychain and every call to it succeeds, so there is no refusal to
/// build a case out of without a fault-injection seam inside `KeychainItem` —
/// and that seam would be a second implementation of `Security.framework` to
/// get wrong.
#if os(macOS) || targetEnvironment(macCatalyst)
struct KeychainMigrationFallbackTests {
    private let secret = Data(repeating: 7, count: 32)

    /// One `(service, account)` across the two domains — production's exact
    /// shape — with the destination in the keychain this build cannot reach.
    private func crossDomainPair() -> (source: KeychainItem, destination: KeychainItem) {
        let service = "dev.sunrise.Sunrise.tests.\(UUID().uuidString).cross-domain"
        func item(_ domain: KeychainDomain) -> KeychainItem {
            KeychainItem(
                service: service,
                account: "migration",
                accessibility: .afterFirstUnlockThisDeviceOnly,
                domain: domain
            )
        }
        return (item(.login), item(.dataProtection))
    }

    /// A destination that refuses must cost the user a retry next launch, not
    /// the secret. This is the arm decision 3 chose over throwing, and the one
    /// whose value `load` has to consume — a caller that discards it and reads
    /// the destination instead sees nothing and reports a lost vault.
    @Test
    func aDestinationThisBuildCannotReachFallsBackToTheSource() throws {
        let pair = crossDomainPair()
        defer { try? pair.source.delete() }
        try pair.source.write(secret)

        let migration = KeychainMigration(source: pair.source, destination: pair.destination)
        #expect(!migration.sourceAndDestinationAreOneItem, "the Mac has two keychains")
        #expect(try migration.run() == secret, "the fallback arm answers with the source's bytes")
        #expect(try pair.source.read() == secret, "and a refusal must not cost the user the copy")
    }

    /// The same failure seen from where it matters: what a store's `load`
    /// answers. Both repairs land on this one assertion — the composed step
    /// consumes what the migration returned, and reads the other domain before
    /// reporting nothing — and either of them alone produces `secret` here,
    /// where discarding the return value and reading only the destination
    /// produced `nil` and a `.locked(.keyMissingForExistingVault)` session.
    @Test
    func aLoadWhoseDestinationCannotBeReachedStillAnswersWithTheSecret() throws {
        let pair = crossDomainPair()
        defer { try? pair.source.delete() }
        try pair.source.write(secret)

        let migration = KeychainMigration(source: pair.source, destination: pair.destination)
        #expect(try migration.loadMigratingIfNeeded() == secret)
    }
}
#endif
