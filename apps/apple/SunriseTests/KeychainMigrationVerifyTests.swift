import Foundation
import Security
import Testing

@testable import Sunrise

/// The verify step's two conditions, pinned apart.
///
/// Outside `KeychainMigrationTests` because that file is exactly on the
/// 520-line `file_length` ceiling `swiftlint --strict` enforces, and outside
/// `KeychainMigrationFallbackTests`'s `#if os(macOS)` gate because neither case
/// here needs two keychains: both run `.login` -> `.login` across two services,
/// which are distinct stored items on every Apple platform.
struct KeychainMigrationVerifyTests {
    private let secret = Data(repeating: 11, count: 32)

    private func scratchPair() -> (source: KeychainItem, destination: KeychainItem) {
        let run = UUID().uuidString
        func item(_ role: String) -> KeychainItem {
            KeychainItem(
                service: "dev.sunrise.Sunrise.tests.\(run).\(role)",
                account: "verify",
                accessibility: .afterFirstUnlockThisDeviceOnly,
                domain: .login
            )
        }
        return (item("source"), item("destination"))
    }

    /// A destination removed between the write and the read-back holds **no**
    /// competing secret, so it must not be reported as two secrets under one
    /// name. Before the guard was split this raised `migrationUnverified` —
    /// the one error `run()`'s fallback refuses to absorb — so
    /// `SessionModel.start()` hard-locked on `.keychainUnavailable` while the
    /// secret sat readable in the source.
    @Test
    func aDestinationRemovedBeforeTheReadBackFallsBackToTheSource() throws {
        let pair = scratchPair()
        defer { try? pair.source.delete(); try? pair.destination.delete() }
        try pair.source.write(secret)

        let migration = KeychainMigration(source: pair.source, destination: pair.destination)
        let answered = try migration.run { step in
            if step == .verify { try? pair.destination.delete() }
        }
        #expect(answered == secret, "the fallback arm answers with the source's bytes")
        #expect(try pair.source.read() == secret, "and step 4 must not have run")
    }

    /// The second of the two guards the split produced, asserted beside the
    /// first so the pair reads together: bytes that disagree are still the one
    /// refusal. Not the only pin on this condition —
    /// `aDestinationHoldingDifferentBytesIsRefusedAndNothingIsDeleted` in
    /// `KeychainMigrationTests` asserts the same throw and adds that neither
    /// copy is deleted, which this case does not. Deleting that one as
    /// redundant would lose those two survival assertions.
    @Test
    func aDestinationHoldingDifferentBytesIsStillRefused() throws {
        let pair = scratchPair()
        defer { try? pair.source.delete(); try? pair.destination.delete() }
        try pair.source.write(secret)
        try pair.destination.write(Data(repeating: 4, count: 32))

        let migration = KeychainMigration(source: pair.source, destination: pair.destination)
        #expect(throws: KeychainError.migrationUnverified) { try migration.run() }
    }
}
