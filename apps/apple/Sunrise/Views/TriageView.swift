import SwiftUI

/// The triage queue: the late and stale tasks waiting on a decision, a
/// selection over them, and the four actions of ADR-0047 §4.
///
/// What `sunrise://triage` opens — the triage nudge's tap and the morning
/// brief's Open Triage — on both platforms. Each row says why the core put it
/// here; the actions apply to the ticked rows from the bar at the bottom, or
/// to one row from its context menu. Already done and Drop ask one more
/// question first, in a sheet; Defer and Keep need none.
struct TriageView: View {
    @Bindable var model: TriageModel

    @State private var prompt: Prompt?
    @State private var doneAt = Date()
    @State private var reason = ""

    var body: some View {
        VStack(spacing: 0) {
            header
            if let errorMessage = model.errorMessage {
                NoteBanner(text: errorMessage) { model.dismissError() }
            }
            Divider()
            if model.rows.isEmpty {
                ContentUnavailableView(
                    L10n.Triage.emptyTitle,
                    systemImage: "checkmark.circle",
                    description: Text(L10n.Triage.emptyMessage)
                )
                .frame(maxHeight: .infinity)
            } else {
                queue
                Divider()
                actionBar
            }
        }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("triage.queue")
        .sheet(item: $prompt) { promptSheet($0) }
        .task { await model.refresh() }
        .task { await model.follow() }
    }

    private var header: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Image(systemName: "tray.full")
                .font(.title2)
                .foregroundStyle(.tint)
            VStack(alignment: .leading, spacing: 2) {
                Text(L10n.Triage.headline(count: model.rows.count)).font(.headline)
                Text(L10n.Triage.subtitle).font(.caption).foregroundStyle(.secondary)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
    }

    private var queue: some View {
        List(selection: $model.selection) {
            ForEach(model.rows, id: \.task.id) { row in
                TriageRow(row: row)
                    .contextMenu {
                        ForEach(Action.allCases) { action in
                            actionButton(action, on: [row.task.id])
                        }
                    }
            }
        }
        .listStyle(.inset)
        #if os(iOS)
        // A phone shows a list's selection only while editing, and the
        // selection is what the bar acts on, so the queue is always in it.
        .environment(\.editMode, .constant(.active))
        #endif
    }

    private var actionBar: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text(L10n.Triage.selection(count: model.selection.count))
                    .font(.callout)
                    .foregroundStyle(.secondary)
                Spacer(minLength: 0)
                Button(L10n.Triage.selectAll) { model.selectAll() }
                    .disabled(model.selection.count == model.rows.count)
            }
            HStack {
                ForEach(Action.allCases) { action in
                    actionButton(action, on: model.selected)
                        .buttonStyle(.bordered)
                        .disabled(model.selection.isEmpty)
                }
            }
            .controlSize(.small)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
    }

    private func actionButton(_ action: Action, on ids: [EntityRef]) -> some View {
        Button(action.title, systemImage: action.symbol, role: action == .drop ? .destructive : nil) {
            take(action, on: ids)
        }
        .help(action.help)
        .accessibilityIdentifier("triage.\(action.rawValue)")
        .disabledUnlessEditable(.task)
    }

    private func take(_ action: Action, on ids: [EntityRef]) {
        switch action {
        case .deferToNextFreeSlot:
            Task { await model.deferToNextFreeSlot(ids) }
        case .keep:
            Task { await model.keep(ids) }
        case .alreadyDone:
            doneAt = model.now
            prompt = .alreadyDone(ids)
        case .drop:
            reason = ""
            prompt = .drop(ids)
        }
    }

    @ViewBuilder
    private func promptSheet(_ prompt: Prompt) -> some View {
        switch prompt {
        case let .alreadyDone(ids):
            PromptSheet(title: L10n.Triage.doneTitle, confirm: L10n.Triage.doneConfirm) {
                DatePicker(
                    L10n.Triage.doneAt,
                    selection: $doneAt,
                    in: ...model.now,
                    displayedComponents: [.date, .hourAndMinute]
                )
            } onConfirm: {
                let date = doneAt
                Task { await model.markDone(ids, at: date) }
            }
        case let .drop(ids):
            PromptSheet(title: L10n.Triage.dropTitle(count: ids.count), confirm: L10n.Triage.dropConfirm) {
                TextField(L10n.Triage.dropReason, text: $reason, axis: .vertical)
                    .lineLimit(1...4)
            } onConfirm: {
                let text = reason
                Task { await model.drop(ids, reason: text) }
            }
        }
    }
}

extension TriageView {
    /// The four actions, in the order ADR-0047 §4 lists them.
    enum Action: String, CaseIterable, Identifiable {
        case deferToNextFreeSlot = "defer"
        case alreadyDone = "done"
        case drop
        case keep

        var id: Self { self }

        var title: String {
            switch self {
            case .deferToNextFreeSlot: L10n.Triage.deferAction
            case .alreadyDone: L10n.Triage.alreadyDone
            case .drop: L10n.Triage.drop
            case .keep: L10n.Triage.keep
            }
        }

        var help: String {
            switch self {
            case .deferToNextFreeSlot: L10n.Triage.deferHelp
            case .alreadyDone: L10n.Triage.alreadyDoneHelp
            case .drop: L10n.Triage.dropHelp
            case .keep: L10n.Triage.keepHelp
            }
        }

        var symbol: String {
            switch self {
            case .deferToNextFreeSlot: "arrow.turn.up.right"
            case .alreadyDone: "checkmark.circle"
            case .drop: "xmark.circle"
            case .keep: "hand.raised"
            }
        }
    }

    /// The question an action asks before it is taken, and the tasks it will
    /// be taken on — captured when the sheet opens, so a row that leaves the
    /// queue meanwhile is still the row the user chose.
    enum Prompt: Identifiable {
        case alreadyDone([EntityRef])
        case drop([EntityRef])

        var id: String {
            switch self {
            case let .alreadyDone(ids): "done:" + ids.joined(separator: ",")
            case let .drop(ids): "drop:" + ids.joined(separator: ",")
            }
        }
    }
}

/// One row of the queue: the task, and why it needs a decision. A row the
/// core flags for a broken hard scheduling window says so first; the
/// lateness line follows only when the task is also late or stale.
struct TriageRow: View {
    let row: TriageRowItem

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(row.task.title)
            if row.hardViolation {
                Label(L10n.Triage.hardViolation, systemImage: "calendar.badge.exclamationmark")
                    .font(.caption)
                    .foregroundStyle(.red)
                    .accessibilityIdentifier("triage.hardViolation")
            }
            if row.lateness.state != .onTrack {
                Label(reason, systemImage: symbol)
                    .font(.caption)
                    .foregroundStyle(tint)
            }
        }
        .padding(.vertical, 2)
    }

    /// The core's reason, with the moment it began. An on-track row is in the
    /// queue only for its hard window and shows no lateness line, so the
    /// fallback is only for completeness.
    private var reason: String {
        let date = row.lateness.since.map {
            Date(timeIntervalSince1970: Double($0) / 1000).formatted(date: .abbreviated, time: .omitted)
        } ?? ""
        switch row.lateness.state {
        case .lateHard: return L10n.Triage.lateHard(date: date)
        case .lateSoft: return L10n.Triage.lateSoft(date: date)
        case .stale, .onTrack: return L10n.Triage.stale(date: date)
        }
    }

    private var symbol: String {
        switch row.lateness.state {
        case .lateHard: "exclamationmark.circle.fill"
        case .lateSoft: "exclamationmark.circle"
        case .stale, .onTrack: "clock"
        }
    }

    private var tint: Color {
        switch row.lateness.state {
        case .lateHard: .red
        case .lateSoft: .orange
        case .stale, .onTrack: .secondary
        }
    }
}

/// A sheet that asks one question before an action is taken.
private struct PromptSheet<Field: View>: View {
    let title: String
    let confirm: String
    @ViewBuilder let field: () -> Field
    let onConfirm: () -> Void

    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            Form { field() }
                .navigationTitle(title)
                .toolbar {
                    ToolbarItem(placement: .cancellationAction) {
                        Button(L10n.Action.cancel) { dismiss() }
                    }
                    ToolbarItem(placement: .confirmationAction) {
                        Button(confirm) {
                            onConfirm()
                            dismiss()
                        }
                        .accessibilityIdentifier("triage.confirm")
                    }
                }
        }
        #if os(macOS)
        .frame(minWidth: 360, minHeight: 180)
        #endif
    }
}

/// The queue as the shells present it: a sheet over whatever was on screen,
/// with its own title and a Done button, on both platforms.
struct TriageSheet: View {
    let model: TriageModel

    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            TriageView(model: model)
                .navigationTitle(L10n.Triage.title)
                .toolbar {
                    ToolbarItem(placement: .confirmationAction) {
                        Button(L10n.Action.done) { dismiss() }
                    }
                }
        }
        #if os(macOS)
        .frame(minWidth: 520, minHeight: 460)
        #endif
    }
}
