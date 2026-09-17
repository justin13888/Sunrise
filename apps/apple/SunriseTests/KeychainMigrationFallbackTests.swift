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
/// own domain has refused, and the rescued-bytes `return migrated` isolated
/// from both by giving the destination a service of its own.
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
    ///
    /// Its last assertion is one of the **five** an entitlement turns false: with
    /// a team the destination is reachable, the migration completes, and the
    /// source is deleted. `KeychainMigrationTests`'
    /// `theProbeAnswersWhatThisBuildCanActuallyReach` lists the five and what is
    /// done with them.
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
    /// with `errSecMissingEntitlement`. It does not: the first assertion below
    /// settles that directly. A query answers `errSecItemNotFound`, and the
    /// -34018 refusal is on the three *mutating* calls.
    ///
    /// An earlier revision argued the same conclusion a second way, and that
    /// argument was wrong: it said a hard status on a query would make "the two
    /// cases above" fail. It would not. Both of those go through a fallback arm
    /// that answers with the *source's* bytes whatever the destination raised,
    /// so they stay green either way, and the argument was protected by exactly
    /// the greenness it appealed to. The cases that really would fail are the
    /// three below, which read a `.dataProtection`-addressed item outside any
    /// fallback: `theRescuedBytesAloneAnswerWhenNeitherDomainHoldsTheDestination`,
    /// `aSecretInTheOtherDomainIsFoundByTheSecondRead` and
    /// `aRefusalOnThisDomainDoesNotSpareTheCopyInTheOther`.
    ///
    /// That split is what makes those cases buildable at all: it is why a
    /// cross-domain read can be observed returning bytes, and why a
    /// cross-domain delete can be observed running after its own domain has
    /// already refused.
    ///
    /// One of the **five** an entitlement turns false, and the one that is three
    /// assertions rather than one: every `errSecMissingEntitlement` expectation
    /// below flips. `KeychainMigrationTests`'
    /// `theProbeAnswersWhatThisBuildCanActuallyReach` lists the five and what is
    /// done with them.
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

        // The third mutating call. Decision 19, `KeychainItem.readAcrossDomains`
        // and `docs/07-clients/desktop.md` all name the update alongside the add
        // and the delete; until this line only two of the three were pinned. It
        // is the interesting one of the three, because an update against an
        // item that is not there would answer `errSecItemNotFound` on a keychain
        // this build *can* reach — so the refusal is what separates
        // "unreachable" from "empty".
        let attributes = [kSecValueData as String: Data([1])] as CFDictionary
        #expect(
            SecItemUpdate(query as CFDictionary, attributes) == errSecMissingEntitlement,
            "an update is refused, not answered with not-found"
        )

        #expect(SecItemDelete(query as CFDictionary) == errSecMissingEntitlement)
    }

    /// Two **services**, so `readAcrossDomains()` can answer nothing in either
    /// domain and `return migrated` is the only line left that can supply the
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

    /// The rescued-bytes `return migrated` on its own, with the cross-domain
    /// read taken out of the picture.
    ///
    /// Exactly one case above reaches this line —
    /// `aLoadWhoseDestinationCannotBeReachedStillAnswersWithTheSecret`. The
    /// other calls `run()` directly and never enters the composed step. And the
    /// one that does reach it cannot *isolate* it: it shares one service across
    /// the two domains, so `readAcrossDomains()` finds the source through its
    /// other-domain read and returns before the last line. Turning that
    /// `return migrated` into `return nil` leaves both of them green. A separate
    /// service for the destination makes both of the destination's reads answer
    /// nothing, so the only path to `secret` is the value the fallback arm
    /// rescued.
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
    ///
    /// Its first assertion is one of the **five** an entitlement turns false: with
    /// a team this domain's own delete succeeds and nothing is raised.
    /// `KeychainMigrationTests`'
    /// `theProbeAnswersWhatThisBuildCanActuallyReach` lists the five and what is
    /// done with them.
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

    /// A cross-domain write that stored **nothing** must not claim it stored
    /// something.
    ///
    /// `writtenButOtherDomainRefused` is the one error out of
    /// `writeAcrossDomains` that asserts the bytes are on disk, and
    /// `KeychainCredentialStore.save` acts on that assertion by not rethrowing
    /// it. So the discrimination carries a session: label a write that failed
    /// in its *own* domain as the partial success, and `save` reports a token
    /// as persisted while the Keychain holds none of it — the same class of
    /// defect as the one the case exists to repair, pointing the other way.
    ///
    /// Addressed at the domain this build cannot mutate, so `write(_:)` is
    /// refused and the other-domain delete is never reached. That is the same
    /// refusal `theUnreachableDomainRefusesMutationsAndAnswersReadsAsEmpty`
    /// measures on `SecItemUpdate` and `SecItemAdd`.
    ///
    /// Its throws-expectation is one of the **five** an entitlement turns false:
    /// with a team the write is not refused at all. `KeychainMigrationTests`'
    /// `theProbeAnswersWhatThisBuildCanActuallyReach` lists the five and what is
    /// done with them.
    @Test
    func aWriteRefusedInItsOwnDomainIsNotReportedAsAPartialSuccess() throws {
        let addressed = KeychainItem(
            service: "dev.sunrise.Sunrise.tests.\(UUID().uuidString).own-domain-write",
            account: "credentials",
            accessibility: .afterFirstUnlockThisDeviceOnly,
            domain: .dataProtection
        )

        let raised = #expect(throws: KeychainError.self) {
            try addressed.writeAcrossDomains(secret)
        }
        #expect(
            raised == .unexpected(errSecMissingEntitlement),
            "the write itself refused, so nothing may be claimed to have been written"
        )
        #expect(try addressed.readAcrossDomains() == nil, "and nothing was stored anywhere")
    }
}
#endif

/// The two `LocalizedError` arms the rest of this suite never reads.
///
/// `KeychainError.writtenButOtherDomainRefused` is constructed nowhere else in
/// the suite, and `.migrationUnverified` is thrown and caught by case identity
/// with its message never read — so both user-facing blocks executed in no test
/// until these two cases.
///
/// They are **not** part of the seven lines declared untestable in
/// `docs/07-clients/desktop.md`. Those need a keychain state this machine cannot
/// produce; a `switch` over an enum makes no `Security.framework` call at all,
/// which is why these close here rather than joining the declared set.
///
/// Outside the `#if` above on purpose: the messages are platform-independent, and
/// the Mac-only gate would have left them unexecuted on every iOS run. They sit in
/// this file rather than `KeychainMigrationTests` because that file is exactly on
/// the 520-line `file_length` ceiling `swiftlint --strict` enforces.
struct KeychainErrorMessageTests {
    /// Leads with what *was* saved. Every other message in the enum describes
    /// something that did not happen and this one does not, so a user told only
    /// that a removal failed would reasonably retype a secret already stored.
    /// Asserting the whole string, not merely that it is non-`nil`: a non-`nil`
    /// check passes against an empty string and against a reordered message.
    @Test
    func theRefusedOtherDomainMessageLeadsWithWhatWasSaved() {
        let status = errSecUserCanceled
        let reason = SecCopyErrorMessageString(status, nil) as String? ?? "Keychain error \(status)."
        #expect(
            KeychainError.writtenButOtherDomainRefused(status).errorDescription
                == "This secret was saved, but an older copy of it in your other "
                + "keychain could not be removed: " + reason
        )
    }

    /// Says nothing was deleted, because nothing was. This is the one migration
    /// failure that refuses the load rather than falling back to the source, and a
    /// message that left the reassurance out would read to the user as data loss.
    @Test
    func theUnverifiedMigrationMessageSaysNothingWasDeleted() {
        #expect(
            KeychainError.migrationUnverified.errorDescription
                == "Two different secrets are stored under the same Keychain name, so "
                + "this app cannot tell which one belongs to your vault. "
                + "Nothing has been deleted."
        )
    }
}
