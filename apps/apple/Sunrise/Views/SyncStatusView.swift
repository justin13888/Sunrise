import SwiftUI

/// The sync badge.
struct SyncStatusView: View {
    let presentation: SyncPresentation

    var body: some View {
        Label(presentation.label, systemImage: presentation.symbol)
            .foregroundStyle(colour)
            .help(presentation.detail ?? presentation.label)
            .accessibilityLabel(
                presentation.detail.map { "\(presentation.label). \($0)" } ?? presentation.label
            )
    }

    private var colour: Color {
        switch presentation.tone {
        case .ok: .green
        case .working: .accentColor
        case .idle: .secondary
        case .alert: .orange
        }
    }
}

/// The banner an incomplete vault gets, on top of the badge.
///
/// A badge alone is not enough for `Degraded`: the user has to be told that
/// edits exist which this Mac will not receive, and a tooltip is not telling
/// them.
struct SyncWarningBanner: View {
    let presentation: SyncPresentation

    var body: some View {
        if presentation.isKnownIncomplete {
            HStack(alignment: .top, spacing: 10) {
                Image(systemName: presentation.symbol)
                    .foregroundStyle(.orange)
                VStack(alignment: .leading, spacing: 2) {
                    Text(presentation.label).font(.headline)
                    if let detail = presentation.detail {
                        Text(detail).font(.callout).foregroundStyle(.secondary)
                    }
                }
                Spacer(minLength: 0)
            }
            .padding(12)
            .background(.orange.opacity(0.12), in: .rect(cornerRadius: 8))
            .padding(.horizontal, 12)
            .padding(.top, 8)
        }
    }
}

#Preview {
    VStack {
        SyncStatusView(presentation: .unknown)
        SyncWarningBanner(
            presentation: SyncPresentation(
                SyncSnapshot(state: .degraded, outboxPending: 0, peerDevices: 1, lastSyncMs: nil)
            )
        )
    }
    .padding()
}
