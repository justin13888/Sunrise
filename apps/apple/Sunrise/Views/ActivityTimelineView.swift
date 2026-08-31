import SwiftUI

/// One entity's activity, newest first.
struct ActivityTimelineView: View {
    let model: ActivityModel

    @State private var deviceID = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Activity").font(.headline)
            if let error = model.errorMessage {
                Text(error).font(.callout).foregroundStyle(.orange)
            }
            if model.rows.isEmpty {
                ContentUnavailableView(
                    "Nothing recorded yet",
                    systemImage: "clock.arrow.circlepath",
                    description: Text("Completions, deferrals and moves show up here.")
                )
                .frame(maxWidth: .infinity, minHeight: 160)
            } else {
                List(model.rows, id: \.opId) { row in
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        Image(systemName: symbol(for: row.detail))
                            .foregroundStyle(.secondary)
                            .frame(width: 16)
                        VStack(alignment: .leading, spacing: 1) {
                            // The domain's phrasing, so this reads the same as
                            // the CSV export and `sunrise-cli`.
                            Text(row.phrase)
                            Text(caption(for: row))
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                        Spacer(minLength: 0)
                    }
                    .padding(.vertical, 2)
                }
                .listStyle(.inset)
                .accessibilityIdentifier("activity-timeline")
            }
        }
        .padding(12)
        .frame(minHeight: 280)
        .task {
            deviceID = await model.deviceIdentifier()
            await model.refresh()
        }
        .task { await model.follow() }
    }

    private func caption(for row: ActivityRow) -> String {
        let day = model.day(of: row)
        let origin = model.isThisDevice(row, deviceID: deviceID) ? "this device" : "another device"
        return "\(day.text) \(model.clock(of: row)) · \(origin)"
    }

    /// An icon per kind. Decoration over the phrase, never instead of it: the
    /// words are the seam's and the glyph is this platform's.
    private func symbol(for detail: ActivityDetail) -> String {
        switch detail {
        case .taskCreated, .streamCreated: "plus.circle"
        case .taskCompleted: "checkmark.circle"
        case .taskReopened: "arrow.uturn.backward.circle"
        case .taskCancelled: "xmark.circle"
        case .taskDeferred: "clock.arrow.circlepath"
        case .taskMoved: "arrow.right.circle"
        case .taskUpdated: "pencil.circle"
        case .taskDeleted, .streamDeleted: "trash.circle"
        case .focusStarted: "play.circle"
        case .focusEnded: "stop.circle"
        }
    }
}
