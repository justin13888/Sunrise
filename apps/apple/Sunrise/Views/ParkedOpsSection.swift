import SwiftUI

/// `Settings → Diagnostics`: the parked-change count ADR-0045 §4
/// "Visibility" puts in diagnostics.
///
/// Shown only with an open vault, for the reason ``AccountView``'s `devices`
/// is optional: with no core behind it, "None" would be a fact nobody read.
/// Its own view rather than another section on ``AccountView``, whose file is
/// at the length limit, and so that the model's lifetime is this section's.
struct ParkedOpsSection: View {
    let bridge: CoreBridge
    @State private var model = ParkedOpsModel()

    var body: some View {
        Section("Diagnostics") {
            switch model.rows {
            case nil:
                LabeledContent("Changes waiting for an update") {
                    Text(model.errorMessage ?? "Checking…").foregroundStyle(.secondary)
                }
            case let rows? where rows.isEmpty:
                LabeledContent("Changes waiting for an update", value: "None")
                    .foregroundStyle(.secondary)
            case let rows?:
                ForEach(rows) { row in
                    LabeledContent(row.label, value: "\(row.count)")
                }
            }
            if let rows = model.rows {
                Text(ParkedOpsModel.caption(for: rows))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .accessibilityIdentifier("account.parkedOps")
        // Keyed on the bridge, so switching vaults reads the new vault's count
        // rather than showing the old one's.
        .task(id: ObjectIdentifier(bridge)) { await model.refresh(from: bridge) }
    }
}
