import Foundation

/// How many changes this device keeps but cannot apply yet, by reason.
///
/// ADR-0045 §4 "Visibility": an op this build cannot apply is parked rather
/// than dropped, counted toward the sync cursor, and replayed after an
/// upgrade. Nothing is lost, but before this read the only trace of it was a
/// `core.op.parked` log line, so a user whose newer device was writing
/// something this one cannot read had no way to know why it did not show up.
///
/// A snapshot, read when the diagnostics appear: the count moves only when a
/// sync delivers an op of a kind this build does not know, or an upgrade
/// replays them, and neither is something a settings screen has to chase.
@MainActor
@Observable
final class ParkedOpsModel {
    /// One reason, as the diagnostics render it.
    struct Row: Identifiable, Equatable {
        /// The core's reason string, which is also the row's identity.
        let id: String
        let label: String
        let count: UInt64
    }

    /// `nil` before the first answer, so the screen says "Checking…" rather
    /// than "None" about a count it has not read.
    private(set) var rows: [Row]?
    private(set) var errorMessage: String?

    func refresh(from bridge: CoreBridge) async {
        do {
            let result = try await bridge.query(.parkedOpsSummary)
            guard case let .parkedOps(counts) = result else {
                errorMessage = "The core answered the parked-change count with something else."
                return
            }
            rows = counts.map(Self.row)
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// The row for one reason. A reason this build has no words for is shown
    /// by its stored name: a vault a newer build parked into can be opened by
    /// this one, and a count under an unfamiliar name is still a count.
    nonisolated static func row(_ count: ParkedReasonCount) -> Row {
        Row(id: count.reason, label: label(forReason: count.reason), count: count.count)
    }

    nonisolated static func label(forReason reason: String) -> String {
        switch reason {
        case "unknown_kind": "From a newer version of Sunrise"
        case "replay_refused": "Refused after an update"
        default: reason
        }
    }

    /// What the section's caption says, given what is parked.
    nonisolated static func caption(for rows: [Row]) -> String {
        if rows.isEmpty {
            return "Every change this device has received has been applied."
        }
        return "Changes another device made that this one keeps but cannot apply yet. "
            + "Nothing is lost: each update of Sunrise on this device tries them again."
    }
}
