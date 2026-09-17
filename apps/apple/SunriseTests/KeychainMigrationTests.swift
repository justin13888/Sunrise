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
/// *successful* cross-domain migration cannot be exercised. What these cases do
/// prove is the part that is not about domains at all — the resume rule, the
/// order of write-before-delete, and the terminal state after an interruption
/// at each of the five steps. `KeychainMigrationFallbackTests` takes the other
/// half: what a destination that genuinely refuses costs the user.
struct KeychainMigrationTests {
    /// Thrown from the step hook to kill a migration the way a crash would,
    /// leaving whatever the Keychain has actually been told so far. It comes
    /// back wrapped in a `KeychainMigration.Interruption`, which is what keeps
    /// it out of the fallback — see
    /// `aKeychainFailureRaisedByTheHookIsNotSwallowedByTheFallback`.
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

    /// Step 0. The first thing `run` does is a read, so a kill here cannot have
    /// changed anything — which is the case worth pinning, because "cannot have
    /// changed anything" is an assertion about the *order* of the five steps
    /// and not a tautology.
    @Test
    func anInterruptionBeforeReadingTheDestinationChangesNothing() throws {
        let pair = scratchPair()
        defer { pair.removeBoth() }
        try pair.source.write(secret)

        #expect(throws: KeychainMigration.Interruption.self) {
            try pair.migration.run { step in
                if step == .readDestination { throw Interrupted() }
            }
        }
        #expect(try pair.source.read() == secret)
        #expect(try pair.destination.read() == nil)

        #expect(try pair.migration.run() == secret)
        #expect(try pair.destination.read() == secret)
        #expect(try pair.source.read() == nil)
    }

    /// Step 1. Nothing has been written, so the source is the only copy and
    /// the next launch restarts from the top.
    @Test
    func anInterruptionBeforeReadingTheSourceResumesFromTheTop() throws {
        let pair = scratchPair()
        defer { pair.removeBoth() }
        try pair.source.write(secret)

        #expect(throws: KeychainMigration.Interruption.self) {
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

        #expect(throws: KeychainMigration.Interruption.self) {
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

        #expect(throws: KeychainMigration.Interruption.self) {
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

        #expect(throws: KeychainMigration.Interruption.self) {
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
    ///
    /// The answer is pinned as a **value** per platform rather than as
    /// `== !KeychainDomain.domainsAreDistinctStores`, which is what this case
    /// used to say and which would hold however that flag were wired — it
    /// restated the implementation instead of constraining it.
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
        #if os(macOS) || targetEnvironment(macCatalyst)
        #expect(!migration.sourceAndDestinationAreOneItem, "two keychains, so this is real work")
        #else
        #expect(migration.sourceAndDestinationAreOneItem, "one keychain, so this must be a no-op")
        #endif
    }

    /// The step hook's exemption from the fallback has to be **structural**.
    ///
    /// The hook stands in for the process dying, so what it raises must reach
    /// the caller. Before `Interruption` the catch was on `KeychainError` and
    /// the exemption held only because the tests happened to raise a type of
    /// their own: a case throwing a `KeychainError` from the hook — the
    /// natural way to pin what a `Security` failure at a given step does —
    /// would have been swallowed, returned the source's bytes, and let the
    /// case assert against the fallback while believing it had observed a
    /// partial keychain state. This is that case, and it must not be swallowed.
    @Test
    func aKeychainFailureRaisedByTheHookIsNotSwallowedByTheFallback() throws {
        let pair = scratchPair()
        defer { pair.removeBoth() }
        try pair.source.write(secret)

        let interruption = #expect(throws: KeychainMigration.Interruption.self) {
            try pair.migration.run { step in
                if step == .writeDestination { throw KeychainError.unexpected(errSecIO) }
            }
        }
        #expect(interruption?.step == .writeDestination)
        #expect(interruption?.cause as? KeychainError == .unexpected(errSecIO))
        // The fallback would have answered `secret` and left exactly this
        // state, so the assertion that separates the two is the throw above —
        // these two only add that nothing was lost on the way out.
        #expect(try pair.source.read() == secret)
        #expect(try pair.destination.read() == nil)
    }

    /// The three steps every store's `load` runs, on the ordinary path.
    @Test
    func aLoadMigratesRaisesAndAnswersWithTheMovedSecret() throws {
        let pair = scratchPair()
        defer { pair.removeBoth() }
        try pair.source.write(secret)

        #expect(try pair.migration.loadMigratingIfNeeded() == secret)
        #expect(try pair.destination.read() == secret)
        #expect(try pair.source.read() == nil)
    }

    /// Nothing anywhere stays `nil` through the whole composed step, rather
    /// than becoming an error — the distinction `KeychainItem.read` exists to
    /// keep, because turning it into one generates a new root and orphans the
    /// vault.
    @Test
    func aLoadWithNothingAnywhereAnswersNothing() throws {
        let pair = scratchPair()
        defer { pair.removeBoth() }

        #expect(try pair.migration.loadMigratingIfNeeded() == nil)
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
            // Deliberately weaker than `== errSecItemNotFound`, which is
            // what both domains answer today — a *query* is answered rather
            // than refused on every configuration this repository builds, which
            // `theUnreachableDomainRefusesMutationsAndAnswersReadsAsEmpty`
            // measures. The claim here is only that nothing of the probe's is
            // findable, and that one survives an entitlement landing.
            #expect(SecItemCopyMatching(query as CFDictionary, nil) != errSecSuccess)
        }
    }

    /// What the probe answers on the configurations this repository builds, and **one
    /// of five** assertions that change the day an entitlement lands — an earlier
    /// revision of this line called it the only one. The other four are in
    /// `KeychainMigrationFallbackTests`: `aDestinationThisBuildCannotReachFallsBackToTheSource`,
    /// `theUnreachableDomainRefusesMutationsAndAnswersReadsAsEmpty`,
    /// `aRefusalOnThisDomainDoesNotSpareTheCopyInTheOther` and
    /// `aWriteRefusedInItsOwnDomainIsNotReportedAsAPartialSuccess`. All five are
    /// **rewritten to assert the entitled behaviour**, not deleted and not guarded:
    /// deleting drops the coverage exactly when the path first runs for real, guarding
    /// leaves the entitled build asserting nothing. `docs/07-clients/desktop.md` names
    /// the same five under its Apple-team handoff. iOS has the data-protection keychain
    /// and nothing else; the Mac app is ad-hoc signed with no `keychain-access-groups`
    /// entitlement, so `SecItemAdd` with `kSecUseDataProtectionKeychain` is refused and
    /// the probe falls open to the login keychain — which is why this change is inert.
    @Test
    func theProbeAnswersWhatThisBuildCanActuallyReach() {
        #if os(iOS)
        #expect(KeychainDomain.probe() == .dataProtection)
        #else
        #expect(KeychainDomain.probe() == .login)
        #endif
    }

    /// Memoised: asked any number of times, the probe runs **once**.
    ///
    /// `current == probe()` was what this case used to say, and it would pass
    /// identically with no memoisation at all — the probe is deterministic
    /// within a process, so equality cannot tell a cached answer from a
    /// recomputed one. Counting the runs can. The equality is kept as the
    /// second assertion, because "memoised on the probe rather than on a
    /// guess" is the other half of the claim.
    @Test
    func theMemoisedDomainRunsTheProbeExactlyOnce() {
        // Force the `static let` to completion first: `swift_once` blocks every
        // caller until the initialiser returns, so after this line no probe of
        // `current`'s can still be in flight and the count is stable.
        let answer = KeychainDomain.current
        let runsBefore = KeychainDomain.probeRuns.load(ordering: .relaxed)
        for _ in 0 ..< 8 { _ = KeychainDomain.current }
        #expect(KeychainDomain.probeRuns.load(ordering: .relaxed) == runsBefore)

        #expect(answer == KeychainDomain.probe())
        #expect(
            KeychainDomain.probeRuns.load(ordering: .relaxed) == runsBefore + 1,
            "the counter must move when the probe really runs, or the assertion above proves nothing"
        )
    }

    /// The `#if` behind `domainsAreDistinctStores`, pinned as a value.
    ///
    /// `KeychainMigration` deletes the source on the strength of this flag, so
    /// a build where it is wrong deletes the only copy of a vault root. The
    /// case is written against the platform directly rather than against the
    /// flag, so flipping the flag's own condition fails here.
    @Test
    func theTwoDomainsAreTwoStoresOnTheMacAndNowhereElse() {
        #if os(macOS) || targetEnvironment(macCatalyst)
        #expect(KeychainDomain.domainsAreDistinctStores)
        #else
        #expect(!KeychainDomain.domainsAreDistinctStores)
        #endif
    }

    /// `other` has to be an involution, because a cross-domain read that
    /// answered with the domain it started in would silently be no fallback.
    @Test
    func theOtherDomainIsTheOneThisIsNot() {
        #expect(KeychainDomain.login.other == .dataProtection)
        #expect(KeychainDomain.dataProtection.other == .login)
    }

    /// The probe must test the reachability that actually matters.
    ///
    /// It writes its byte under the class the three stores write under, not
    /// the platform default: "can this binary reach the data-protection
    /// keychain at all" and "can it store a secret there the way this app
    /// stores secrets" are different questions, and only the second one
    /// decides where a vault root ends up. This pins that the answer stays in
    /// step with all three stores.
    @Test
    func theProbeTestsTheClassTheStoresWriteUnder() {
        #expect(KeychainDomain.probeAccessibility == KeychainVaultRootStore.accessibility)
        #expect(KeychainDomain.probeAccessibility == KeychainCredentialStore.accessibility)
        #expect(KeychainDomain.probeAccessibility == KeychainRelayDeviceIDStore.accessibility)
    }

    /// …and the probe must actually *submit* that class, which the case above
    /// cannot see. It compares a constant against three other constants, so a
    /// `probe()` that named a different class, or that dropped the domain key
    /// and asked the login keychain — making the answer always
    /// `.dataProtection` and sending every vault root to the wrong store —
    /// leaves it green. This reads the dictionary the probe hands `SecItemAdd`.
    @Test
    func theProbeSubmitsTheClassAndTheDomainItClaimsTo() {
        let account = UUID().uuidString
        let insert = KeychainDomain.probeInsertQuery(account: account)

        #expect(insert[kSecAttrAccessible as String] as? String
            == KeychainDomain.probeAccessibility.attribute as String)
        #expect(insert[kSecUseDataProtectionKeychain as String] as? Bool == true)
        #expect(insert[kSecAttrService as String] as? String == KeychainDomain.probeService)
        #expect(insert[kSecAttrAccount as String] as? String == account)
        #expect(insert[kSecValueData as String] as? Data == Data([0]), "one fixed non-secret byte")
    }

    /// `writeAcrossDomains` must never delete the item it has just written.
    ///
    /// The method's second half deletes the *other* domain's copy, and on every
    /// Apple platform but macOS there is no other domain — `.login` and
    /// `.dataProtection` are two names for one store, so an unguarded delete
    /// would remove the secret one line after storing it. That is the same trap
    /// `sourceAndDestinationAreOneItem` exists for, and this is the case that
    /// fails on iOS if the guard is ever dropped.
    ///
    /// On the Mac it pins a second property, and one that only became a
    /// property once the blanket `try?` there was narrowed: the other domain's
    /// refusal is the missing-entitlement status, which
    /// `meansTheOtherStoreWasUnreachable` swallows, so the write still lands.
    /// Drop that status from the predicate and this case fails. What it does
    /// **not** pin is the delete's own effect — `KeychainItem` declares why no
    /// configuration this repository builds can observe that.
    @Test
    func aCrossDomainWriteKeepsWhatItJustWrote() throws {
        let item = KeychainItem(
            service: "dev.sunrise.Sunrise.tests.\(UUID().uuidString).cross-write",
            account: "credentials",
            accessibility: .afterFirstUnlockThisDeviceOnly,
            domain: KeychainDomain.current
        )
        defer { try? item.delete() }

        let secret = Data(repeating: 4, count: 32)
        try item.writeAcrossDomains(secret)
        #expect(try item.read() == secret)

        let renewed = Data(repeating: 5, count: 32)
        try item.writeAcrossDomains(renewed)
        #expect(try item.read() == renewed, "and a rewrite must replace, not remove")
    }
}
