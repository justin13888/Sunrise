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
