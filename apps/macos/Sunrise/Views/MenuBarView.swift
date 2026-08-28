import SwiftUI

/// The menu bar's daily snapshot.
struct MenuBarView: View {
    let model: MenuBarModel
    let openMain: () -> Void
    let openCapture: () -> Void
    let hotkey: HotkeyStatus

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if model.snapshot.isEmpty {
                Text("Nothing outstanding today.")
                    .font(.callout)
            } else {
                counts
            }

            Divider()

            HStack(spacing: 6) {
                Image(systemName: model.sync.symbol)
                    .foregroundStyle(model.sync.tone == .alert ? .orange : .secondary)
                Text(model.sync.label)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            if let detail = model.sync.detail {
                Text(detail).font(.caption2).foregroundStyle(.secondary)
            }
            if let error = model.errorMessage {
                Text(error).font(.caption2).foregroundStyle(.orange)
            }

            Divider()

            Button("Quick capture", action: openCapture)
                .keyboardShortcut("n", modifiers: [.command, .shift])
            if !hotkey.isActive {
                Text(hotkey.explanation).font(.caption2).foregroundStyle(.secondary)
            }
            Button("Open Sunrise", action: openMain)
            Button("Quit Sunrise") { NSApplication.shared.terminate(nil) }
                .keyboardShortcut("q")
        }
        .padding(12)
        .frame(width: 260)
        .accessibilityIdentifier("menu-bar-snapshot")
        .onAppear { model.isVisible = true }
        .onDisappear { model.isVisible = false }
    }

    private var counts: some View {
        VStack(alignment: .leading, spacing: 3) {
            row("Overdue", model.snapshot.overdue, emphasised: model.snapshot.overdue > 0)
            row("Due today", model.snapshot.due)
            row("Scheduled", model.snapshot.scheduled)
            row("Inbox", model.snapshot.inbox)
            if model.snapshot.doneToday > 0 {
                row("Done today", model.snapshot.doneToday)
            }
        }
    }

    @ViewBuilder
    private func row(_ label: String, _ count: Int, emphasised: Bool = false) -> some View {
        if count > 0 {
            HStack {
                Text(label).font(.callout)
                Spacer()
                Text("\(count)")
                    .font(.callout.monospacedDigit().weight(emphasised ? .bold : .regular))
                    .foregroundStyle(emphasised ? .orange : .primary)
            }
        }
    }
}
