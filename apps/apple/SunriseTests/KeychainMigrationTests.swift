import Foundation
import Security
import Testing

@testable import Sunrise

/// Against the real Keychain, like the vault root's tests and for the same
/// reason: the claim under test is what the platform *did*, and only the
/// platform can answer it. Each case uses its own pair of scratch services and
/// removes both, so nothing is left in the developer's login keychain.
///
/// Every case here runs `.login` → `.login` between two different services.
/// That is deliberate and it is the limit of what this machine can prove: no
/// build this repository can make reaches the data-protection keychain, so a
/// genuinely *cross-domain* migration cannot be exercised. What these cases do
/// prove is the part that is not about domains at all — the resume rule, the
/// order of write-before-delete, and the terminal state after an interruption
/// at each of the five steps.
struct KeychainMigrationTests {
    /// Thrown from the step hook to kill a migration the way a crash would,
    /// leaving whatever the Keychain has actually been told so far.
    private struct Interrupted: Error {}

    private struct Pair {
        let source: KeychainItem
        let destination: KeychainItem

        var migration: KeychainMigration {
            KeychainMigration(source: source, destination: destination)
        }

        func removeBoth() {
            try? source.delete()
            try? destination.delete()
        }
    }

    private func scratchPair() -> Pair {
        let run = UUID().uuidString
        func item(_ role: String) -> KeychainItem {
            KeychainItem(
                service: "dev.sunrise.Sunrise.tests.\(run).\(role)",
                account: "migration",
                accessibility: .afterFirstUnlockThisDeviceOnly,
                domain: .login
            )
        }
        return Pair(source: item("source"), destination: item("destination"))
    }

    private let secret = Data(repeating: 9, count: 32)

    /// First run. Neither side holding anything is not an error — `read()`
    /// records why turning it into one would generate a new root and orphan
    /// the existing vault.
    @Test
    func nothingAnywhereIsNotAnError() throws {
        let pair = scratchPair()
        defer { pair.removeBoth() }

        #expect(try pair.migration.run() == nil)
        #expect(try pair.source.read() == nil)
        #expect(try pair.destination.read() == nil)
    }

    /// The uninterrupted path, end to end.
    @Test
    func aSourceOnlyItemIsMovedAndTheSourceRemoved() throws {
        let pair = scratchPair()
        defer { pair.removeBoth() }
        try pair.source.write(secret)

        #expect(try pair.migration.run() == secret)
        #expect(try pair.destination.read() == secret)
        #expect(try pair.source.read() == nil, "step 4 must remove the old copy")
    }

    /// Step 1. Nothing has been written, so the source is the only copy and
    /// the next launch restarts from the top.
    @Test
    func anInterruptionBeforeReadingTheSourceResumesFromTheTop() throws {
        let pair = scratchPair()
        defer { pair.removeBoth() }
        try pair.source.write(secret)

        #expect(throws: Interrupted.self) {
            try pair.migration.run { step in
                if step == .readSource { throw Interrupted() }
            }
        }
        #expect(try pair.source.read() == secret, "a readable copy must exist at every instant")
        #expect(try pair.destination.read() == nil)

        #expect(try pair.migration.run() == secret)
        #expect(try pair.destination.read() == secret)
        #expect(try pair.source.read() == nil)
    }

    /// Step 2. The same partial state as step 1 — which is the point of
    /// writing the destination before deleting the source rather than after.
    @Test
    func anInterruptionBeforeWritingTheDestinationResumesFromTheTop() throws {
        let pair = scratchPair()
        defer { pair.removeBoth() }
        try pair.source.write(secret)

        #expect(throws: Interrupted.self) {
            try pair.migration.run { step in
                if step == .writeDestination { throw Interrupted() }
            }
        }
        #expect(try pair.source.read() == secret)
        #expect(try pair.destination.read() == nil, "nothing may be written before the source is read")

        #expect(try pair.migration.run() == secret)
        #expect(try pair.destination.read() == secret)
        #expect(try pair.source.read() == nil)
    }

    /// Step 3. Both sides now hold the secret and the destination has not been
    /// checked yet; the next launch finds the destination, verifies it against
    /// the surviving source, and removes the source.
    @Test
    func anInterruptionBeforeVerifyingLeavesBothAndFinishesNextTime() throws {
        let pair = scratchPair()
        defer { pair.removeBoth() }
        try pair.source.write(secret)

        #expect(throws: Interrupted.self) {
            try pair.migration.run { step in
                if step == .verify { throw Interrupted() }
            }
        }
        #expect(try pair.source.read() == secret)
        #expect(try pair.destination.read() == secret, "step 2 must have completed")

        #expect(try pair.migration.run() == secret)
        #expect(try pair.destination.read() == secret)
        #expect(try pair.source.read() == nil)
    }

    /// Step 4. Verified but not yet cleaned up. Two readable copies is the
    /// safe end of the trade, and the next launch closes it.
    @Test
    func anInterruptionBeforeDeletingTheSourceFinishesNextTime() throws {
        let pair = scratchPair()
        defer { pair.removeBoth() }
        try pair.source.write(secret)

        #expect(throws: Interrupted.self) {
            try pair.migration.run { step in
                if step == .deleteSource { throw Interrupted() }
            }
        }
        #expect(try pair.source.read() == secret)
        #expect(try pair.destination.read() == secret)

        #expect(try pair.migration.run() == secret)
        #expect(try pair.destination.read() == secret)
        #expect(try pair.source.read() == nil)
    }

    /// The one failure that refuses rather than falls back. Two different
    /// secrets claiming one `(service, account)` cannot be told apart from
    /// here, and deleting either would be a guess about which one sealed the
    /// vault.
    @Test
    func aDestinationHoldingDifferentBytesIsRefusedAndNothingIsDeleted() throws {
        let pair = scratchPair()
        defer { pair.removeBoth() }
        try pair.source.write(secret)
        let stranger = Data(repeating: 4, count: 32)
        try pair.destination.write(stranger)

        #expect(throws: KeychainError.migrationUnverified) { try pair.migration.run() }
        #expect(try pair.source.read() == secret, "a refusal must not cost the user either copy")
        #expect(try pair.destination.read() == stranger)
    }

    /// Step 0's "done" answer: a completed migration, read on every subsequent
    /// launch, must not go looking for work.
    @Test
    func aDestinationOnlyItemIsAnsweredWithNothingMoved() throws {
        let pair = scratchPair()
        defer { pair.removeBoth() }
        try pair.destination.write(secret)

        #expect(try pair.migration.run() == secret)
        #expect(try pair.destination.read() == secret)
        #expect(try pair.source.read() == nil)
    }

    /// The case that makes the whole thing safe on the platforms this
    /// repository can actually build, and the one whose absence would be
    /// catastrophic: when the source and the destination are two names for one
    /// stored item, the migration must do nothing at all. Without this it
    /// would write the item over itself, verify it against itself, and then
    /// delete the only copy there is.
    @Test
    func twoNamesForOneItemAreLeftAlone() throws {
        let item = KeychainItem(
            service: "dev.sunrise.Sunrise.tests.\(UUID().uuidString)",
            account: "migration",
            accessibility: .afterFirstUnlockThisDeviceOnly,
            domain: .login
        )
        defer { try? item.delete() }
        try item.write(secret)

        let migration = KeychainMigration(source: item, destination: item)
        #expect(migration.sourceAndDestinationAreOneItem)
        #expect(try migration.run() == secret)
        #expect(try item.read() == secret, "the migration must not delete the item it was given twice")
    }

    /// The same question asked the way production asks it: one `(service,
    /// account)`, the two domains. On macOS those are two keychains and the
    /// migration is real work; everywhere else there is one keychain and it
    /// must be a no-op.
    @Test
    func oneNameInTwoDomainsIsOneItemOnlyWhereThePlatformHasOneKeychain() {
        func item(_ domain: KeychainDomain) -> KeychainItem {
            KeychainItem(
                service: "dev.sunrise.Sunrise.tests.domains",
                account: "migration",
                accessibility: .afterFirstUnlockThisDeviceOnly,
                domain: domain
            )
        }
        let migration = KeychainMigration(source: item(.login), destination: item(.dataProtection))
        #expect(migration.sourceAndDestinationAreOneItem == !KeychainDomain.domainsAreDistinctStores)
    }
}

/// The two lines that choose where a secret is stored. Nothing below proves a
/// cross-domain migration works — no build this machine can make reaches the
/// data-protection keychain — but the probe's own answer is checkable, and so
/// is the promise that it leaves nothing behind.
///
/// Serialized because `theProbeDeletesWhateverItWrote` looks at a service the
/// other two cases here write to: two probes in flight at once would let one
/// case see the other's byte in the window between its add and its delete.
@Suite(.serialized)
struct KeychainDomainTests {
    /// `.login` must add **nothing**, not `kSecUseDataProtectionKeychain:
    /// false`. Writing `false` on iOS is a claim the platform does not honour,
    /// and on macOS it is a second spelling of the default.
    @Test
    func theLoginDomainAddsNothingToAQuery() {
        var query: [String: Any] = [kSecClass as String: kSecClassGenericPassword]
        KeychainDomain.login.apply(to: &query)
        #expect(query.count == 1)
        #expect(!query.keys.contains(kSecUseDataProtectionKeychain as String))
    }

    @Test
    func theDataProtectionDomainAsksForItByName() {
        var query: [String: Any] = [kSecClass as String: kSecClassGenericPassword]
        KeychainDomain.dataProtection.apply(to: &query)
        #expect(query[kSecUseDataProtectionKeychain as String] as? Bool == true)
    }

    /// The probe writes a byte of its own to find out what this binary can
    /// reach. Leaving it behind would put a Sunrise item a user did not ask
    /// for in their Keychain on every cold launch.
    @Test
    func theProbeDeletesWhateverItWrote() {
        // Force the memoised probe to have *finished*, so the only one that can
        // be in flight while this looks is the one on the next line. A
        // `static let` blocks every caller until its initialiser returns, which
        // is exactly the barrier needed here.
        _ = KeychainDomain.current
        _ = KeychainDomain.probe()

        for domain in [KeychainDomain.login, .dataProtection] {
            var query: [String: Any] = [
                kSecClass as String: kSecClassGenericPassword,
                kSecAttrService as String: KeychainDomain.probeService,
                kSecMatchLimit as String: kSecMatchLimitAll
            ]
            domain.apply(to: &query)
            // "not found" on the keychain this build can reach, and a refusal
            // on the one it cannot — either way nothing of the probe's is
            // findable, which is the whole claim.
            #expect(SecItemCopyMatching(query as CFDictionary, nil) != errSecSuccess)
        }
    }

    /// What the probe answers on the configurations this repository builds,
    /// and the one assertion that would change the day an entitlement lands.
    /// iOS has the data-protection keychain and nothing else. The Mac app is
    /// ad-hoc signed with no `keychain-access-groups` entitlement, so
    /// `SecItemAdd` with `kSecUseDataProtectionKeychain` returns
    /// `errSecMissingEntitlement` and the probe falls open to the login
    /// keychain — which is exactly why this change is behaviourally inert here.
    @Test
    func theProbeAnswersWhatThisBuildCanActuallyReach() {
        #if os(iOS)
        #expect(KeychainDomain.probe() == .dataProtection)
        #else
        #expect(KeychainDomain.probe() == .login)
        #endif
    }

    /// Memoised, and memoised on the probe rather than on a guess.
    @Test
    func theMemoisedDomainIsTheProbesAnswer() {
        #expect(KeychainDomain.current == KeychainDomain.probe())
    }
}
