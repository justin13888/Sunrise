import Foundation
import Security
import Testing

@testable import Sunrise

/// The fallback arm and the cross-domain operations, driven by `Security` calls
/// that really fail.
///
/// Every case in `KeychainMigrationTests` runs `.login` → `.login`, where
/// nothing refuses, so until this suite existed the arm that rescues a secret
/// from a failed destination executed in no test at all. The one deterministic
/// refusal this repository can produce is the one the probe already leans on:
/// an ad-hoc-signed Mac with no `keychain-access-groups` entitlement cannot
/// reach the data-protection keychain, so a destination addressed there fails
/// for real.
///
/// The refusal is narrower than it looks, and
/// `theUnreachableDomainRefusesMutationsAndAnswersReadsAsEmpty` pins where it
/// actually falls: on `SecItemAdd`, `SecItemUpdate` and `SecItemDelete`, while
/// a query answers `errSecItemNotFound` like any empty keychain. That is what
/// makes the second half of this suite buildable — a cross-domain read
/// observed returning bytes, a cross-domain delete observed running after its
/// own domain has refused, and the `?? migrated` term isolated from both by
/// giving the destination a service of its own.
///
/// Mac-only by necessity, not by choice. On iOS the data-protection keychain is
/// the only keychain and every call to it succeeds, so there is no refusal to
/// build a case out of without a fault-injection seam inside `KeychainItem` —
/// and that seam would be a second implementation of `Security.framework` to
/// get wrong. `aCrossDomainWriteKeepsWhatItJustWrote` is in the other file for
/// that reason: the property it pins is one only iOS can break.
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

    /// What this build's two keychains actually answer, which is the fact every
    /// other case here rests on and the one the source used to state wrongly.
    ///
    /// The claim was that an unsigned Mac answers a `.dataProtection` *query*
    /// with `errSecMissingEntitlement`. It does not, and it never could have —
    /// if a query returned a hard status, the first bare `try` in
    /// `readAcrossDomains()` would throw and the two cases above would fail. A
    /// query answers `errSecItemNotFound`; the -34018 refusal is on the three
    /// *mutating* calls. That split is what makes the cases below buildable at
    /// all: it is why a cross-domain read can be observed returning bytes, and
    /// why a cross-domain delete can be observed running after its own domain
    /// has already refused.
    @Test
    func theUnreachableDomainRefusesMutationsAndAnswersReadsAsEmpty() {
        let service = "dev.sunrise.Sunrise.tests.\(UUID().uuidString).statuses"
        var query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: "probe"
        ]
        KeychainDomain.dataProtection.apply(to: &query)

        var read = query
        read[kSecReturnData as String] = true
        read[kSecMatchLimit as String] = kSecMatchLimitOne
        #expect(
            SecItemCopyMatching(read as CFDictionary, nil) == errSecItemNotFound,
            "a query is answered, not refused — the `try?` in readAcrossDomains is not for this"
        )

        var insert = query
        insert[kSecAttrAccessible as String] = KeychainAccessibility
            .afterFirstUnlockThisDeviceOnly.attribute
        insert[kSecValueData as String] = Data([0])
        #expect(SecItemAdd(insert as CFDictionary, nil) == errSecMissingEntitlement)
        #expect(SecItemDelete(query as CFDictionary) == errSecMissingEntitlement)
    }

    /// Two **services**, so `readAcrossDomains()` can answer nothing in either
    /// domain and `?? migrated` is the only term left that can supply the
    /// secret.
    private func twoServicePair() -> (source: KeychainItem, destination: KeychainItem) {
        let run = UUID().uuidString
        func item(_ role: String, _ domain: KeychainDomain) -> KeychainItem {
            KeychainItem(
                service: "dev.sunrise.Sunrise.tests.\(run).\(role)",
                account: "migration",
                accessibility: .afterFirstUnlockThisDeviceOnly,
                domain: domain
            )
        }
        return (item("source", .login), item("destination", .dataProtection))
    }

    /// The `?? migrated` term on its own, with the cross-domain read taken out
    /// of the picture.
    ///
    /// The two cases above reach this line, but they cannot *isolate* it: they
    /// share one service across the two domains, so `readAcrossDomains()` finds
    /// the source through its other-domain read and answers before the `??` is
    /// ever consulted. Deleting `?? migrated` leaves them both green. A separate
    /// service for the destination makes both of its reads answer nothing, so
    /// the only path to `secret` is the value the fallback arm rescued.
    ///
    /// This is the case an earlier note said no buildable configuration could
    /// produce — on the grounds that it needs a destination that fails writes
    /// while answering reads cleanly. This repository's destination is exactly
    /// that, and always was: the refusal is on the write, not the read.
    @Test
    func theRescuedBytesAloneAnswerWhenNeitherDomainHoldsTheDestination() throws {
        let pair = twoServicePair()
        defer { try? pair.source.delete() }
        try pair.source.write(secret)

        let migration = KeychainMigration(source: pair.source, destination: pair.destination)
        #expect(
            try pair.destination.readAcrossDomains() == nil,
            "neither domain may hold the destination, or this case proves nothing"
        )
        #expect(try migration.loadMigratingIfNeeded() == secret)
    }

    /// The other-domain read returning **bytes** — the half of
    /// `readAcrossDomains()` that turns a `nil` into a secret, asserted through
    /// the method itself rather than through a migration that also has a
    /// fallback arm to answer with.
    @Test
    func aSecretInTheOtherDomainIsFoundByTheSecondRead() throws {
        let service = "dev.sunrise.Sunrise.tests.\(UUID().uuidString).other-domain"
        func item(_ domain: KeychainDomain) -> KeychainItem {
            KeychainItem(
                service: service,
                account: "twin",
                accessibility: .afterFirstUnlockThisDeviceOnly,
                domain: domain
            )
        }
        let twin = item(.login)
        defer { try? twin.delete() }
        try twin.write(secret)

        let addressed = item(.dataProtection)
        #expect(try addressed.read() == nil, "this build cannot see the domain it is addressed to")
        #expect(try addressed.readAcrossDomains() == secret)
    }

    /// The cross-domain **delete**, which until now nothing constrained: every
    /// `clear()` case passed identically with the second delete removed.
    ///
    /// Addressed at the domain this build cannot mutate, which is both the only
    /// way to observe the other-domain half and the case that matters. Its own
    /// delete is refused, and an ordering that took that refusal as a reason to
    /// stop left the one copy a `load` can still find sitting in the keychain —
    /// a live refresh token behind a Sign out the user was told had worked.
    /// Both are attempted; the refusal is still raised afterwards.
    @Test
    func aRefusalOnThisDomainDoesNotSpareTheCopyInTheOther() throws {
        let service = "dev.sunrise.Sunrise.tests.\(UUID().uuidString).cross-delete"
        func item(_ domain: KeychainDomain) -> KeychainItem {
            KeychainItem(
                service: service,
                account: "twin",
                accessibility: .afterFirstUnlockThisDeviceOnly,
                domain: domain
            )
        }
        let twin = item(.login)
        defer { try? twin.delete() }
        try twin.write(secret)

        let addressed = item(.dataProtection)
        #expect(throws: KeychainError.self) { try addressed.deleteAcrossDomains() }
        #expect(try twin.read() == nil, "the reachable copy must be gone even so")
        #expect(try addressed.readAcrossDomains() == nil, "and a load must no longer find it")
    }
}
#endif
