import SwiftUI

/// The entity kinds an edit action can write, by the registry tag the core's
/// ``EditGate`` names them with.
///
/// Spelled once here so a view asks `allows(.task)` rather than carrying a
/// string the compiler cannot check.
enum EditedEntity: String, CaseIterable, Sendable {
    case task
    case stream
    case context
    case routine
    case block
    case attachment
    case focusSession = "focus_session"
    case reviewSnapshot = "review_snapshot"
}

/// What this build may edit in the open vault (ADR-0045 §8).
///
/// A vault can require a feature a newer Sunrise wrote and this one does not
/// have. Sync and reading carry on; edits to what the feature covers would
/// overwrite data this build cannot represent, so the core refuses them and
/// this model says so first: the shells show ``ReadOnlyBanner`` and the edit
/// actions on a locked kind are disabled rather than left to fail.
///
/// It follows the change feed, because the op that locks a scope arrives by
/// sync and the core reports it as a change.
@MainActor
@Observable
final class EditGateModel {
    private(set) var gate: EditGate

    /// Open until the first read says otherwise. A test hands in the gate it
    /// wants to draw.
    init(gate: EditGate = EditGate(readOnly: false, locksAll: false, lockedTags: [], missing: [])) {
        self.gate = gate
    }

    /// Whether to show "Update Sunrise to edit".
    var isReadOnly: Bool { gate.readOnly }

    /// Whether edits that write `entity` are open.
    func allows(_ entity: EditedEntity) -> Bool {
        editGateAllows(gate: gate, tag: entity.rawValue)
    }

    /// Re-read the gate. A read that fails keeps the last answer: the core
    /// refuses a locked write whatever this model believes, so a stale gate
    /// costs a refused edit, never an overwrite.
    func refresh(from bridge: CoreBridge) async {
        guard let fresh = try? await bridge.editGate() else { return }
        gate = fresh
    }

    /// Refresh now and after every change batch, until the vault closes.
    func follow(_ bridge: CoreBridge) async {
        await refresh(from: bridge)
        for await batch in await bridge.changes() {
            guard !batch.isClosed else { return }
            await refresh(from: bridge)
        }
    }
}

/// The persistent banner a vault this build can only read shows.
///
/// Not dismissible: the state lasts until Sunrise is updated, and a banner the
/// user could hide would leave every disabled button unexplained.
struct ReadOnlyBanner: View {
    let model: EditGateModel

    var body: some View {
        if model.isReadOnly {
            HStack(alignment: .top, spacing: 10) {
                Image(systemName: "lock")
                    .foregroundStyle(.orange)
                VStack(alignment: .leading, spacing: 2) {
                    Text("Update Sunrise to edit").font(.headline)
                    Text(detail).font(.callout).foregroundStyle(.secondary)
                }
                Spacer(minLength: 0)
            }
            .padding(12)
            .background(.orange.opacity(0.12), in: .rect(cornerRadius: 8))
            .padding(.horizontal, 12)
            .padding(.top, 8)
            .accessibilityElement(children: .combine)
            .accessibilityIdentifier("read-only-banner")
        }
    }

    private var detail: String {
        if model.gate.locksAll {
            return "This vault uses features a newer version of Sunrise added. "
                + "You can still read and sync everything here."
        }
        return "Some items in this vault use features a newer version of Sunrise added. "
            + "You can still read and sync them."
    }
}

private struct EditGateKey: EnvironmentKey {
    static let defaultValue: EditGateModel? = nil
}

extension EnvironmentValues {
    /// The open vault's edit gate, set by the shell. `nil` outside a vault,
    /// where nothing edits.
    var editGate: EditGateModel? {
        get { self[EditGateKey.self] }
        set { self[EditGateKey.self] = newValue }
    }
}

extension View {
    /// Disable this control while the vault's edit gate locks any of
    /// `entities`: an action that writes two kinds (a routine and the tasks
    /// it materializes) names both.
    func disabledUnlessEditable(_ entities: EditedEntity...) -> some View {
        modifier(EditableGate(entities: entities))
    }
}

private struct EditableGate: ViewModifier {
    let entities: [EditedEntity]
    @Environment(\.editGate) private var gate

    func body(content: Content) -> some View {
        content.disabled(gate.map { g in !entities.allSatisfy { g.allows($0) } } ?? false)
    }
}
