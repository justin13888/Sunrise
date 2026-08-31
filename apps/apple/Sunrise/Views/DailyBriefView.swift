import SwiftUI

/// The shared body of the two daily briefs.
///
/// The morning summary and the end-of-day plan are different reports asking
/// different questions, but on screen they are the same object: a headline, a
/// stack of sections, and rows that can be acted on without leaving. Writing
/// that twice would be two places for "Snooze until tomorrow" to mean
/// different things — the failure `TaskRows` was already split out to avoid.
struct DailyBriefBody: View {
    let sections: [BriefSection]
    let headline: String
    let subtitle: String
    let symbol: String
    let errorMessage: String?
    let bridge: CoreBridge
    /// The three facts a row's facets are built from. Passed as values rather
    /// than as a closure back into the model, so the whole strip is resolved
    /// once per refresh and two rows cannot disagree about what "today" is.
    let nowMs: UInt64
    let timeZone: String
    let names: NameBook
    let complete: (TaskItem) async -> Void
    let snooze: (TaskItem, SnoozeSpan) async -> Void
    let apply: (TaskEdit, TaskItem) async -> Void
    let remove: (TaskItem) async -> Void

    @State private var editing: TaskItem?

    var body: some View {
        VStack(spacing: 0) {
            header
            if let errorMessage {
                Label(errorMessage, systemImage: "exclamationmark.triangle")
                    .font(.callout)
                    .foregroundStyle(.orange)
                    .padding(.horizontal, 12)
                    .padding(.bottom, 6)
            }
            Divider()
            List {
                ForEach(sections) { section in
                    Section("\(section.title) (\(section.count))") {
                        if section.tasks.isEmpty {
                            Text(section.emptyMessage)
                                .font(.callout)
                                .foregroundStyle(.secondary)
                        } else {
                            ForEach(section.tasks, id: \.id) { row($0) }
                        }
                    }
                }
            }
            .listStyle(.inset)
        }
        .sheet(item: $editing) { task in
            TaskEditorView(
                task: task,
                bridge: bridge,
                apply: { await apply($0, task) },
                delete: { await remove(task) }
            )
        }
    }

    private var header: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Image(systemName: symbol)
                .font(.title2)
                .foregroundStyle(.tint)
            VStack(alignment: .leading, spacing: 2) {
                Text(headline).font(.headline)
                Text(subtitle).font(.caption).foregroundStyle(.secondary)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
    }

    private func row(_ task: TaskItem) -> some View {
        TaskRowView(
            facets: TaskFacets(task: task, nowMs: nowMs, timeZone: timeZone, names: names),
            complete: { await complete(task) },
            edit: { editing = task }
        )
        .contextMenu {
            Button("Complete") { Task { await complete(task) } }
            Button("Edit…") { editing = task }
            Divider()
            // The spans are the domain's, not this file's — see `SnoozeSpan`
            // and `snooze_target_ms`. "Tomorrow" is a date, and the two nights
            // a year when it is not 24 hours away are the reason.
            ForEach(SnoozeSpan.offered, id: \.self) { span in
                Button(span.buttonTitle) { Task { await snooze(task, span) } }
            }
        }
        .swipeActions(edge: .trailing) {
            Button("Tomorrow") { Task { await snooze(task, .tomorrow) } }
                .tint(.orange)
        }
    }
}

/// The morning summary — what closed yesterday, and what needs a decision now.
///
/// One of the two views `docs/08-features/notifications.md` calls "the
/// dedicated views issue #9 asks a notification to open into". Reachable three
/// ways: the sidebar, ⌘⌥M, and `sunrise://morning` — which is what the body of
/// a morning notification carries.
struct MorningSummaryView: View {
    let model: MorningSummaryModel

    var body: some View {
        DailyBriefBody(
            sections: model.sections,
            headline: model.headline,
            subtitle: "What closed yesterday, and what today is asking for.",
            symbol: "sunrise",
            errorMessage: model.errorMessage,
            bridge: model.bridge,
            nowMs: model.nowMs,
            timeZone: model.timeZone,
            names: model.names,
            complete: { await model.complete($0) },
            snooze: { await model.snooze($0, $1) },
            apply: { await model.apply($0, to: $1) },
            remove: { await model.delete($0) }
        )
        .navigationTitle("Morning")
        .task { await model.refresh() }
        .task { await model.follow() }
    }
}

/// The end-of-day plan — what today did not finish, and where it goes.
///
/// The other of the two, behind `sunrise://evening` and ⌘⌥E.
struct EndOfDayPlanView: View {
    let model: EndOfDayPlanModel

    @State private var confirmingMove = false

    var body: some View {
        DailyBriefBody(
            sections: model.sections,
            headline: model.headline,
            subtitle: "What today did not finish, the week ahead, and the backlog to plan from.",
            symbol: "moon.stars",
            errorMessage: model.errorMessage,
            bridge: model.bridge,
            nowMs: model.nowMs,
            timeZone: model.timeZone,
            names: model.names,
            complete: { await model.complete($0) },
            snooze: { await model.snooze($0, $1) },
            apply: { await model.apply($0, to: $1) },
            remove: { await model.delete($0) }
        )
        .navigationTitle("Evening")
        .toolbar {
            ToolbarItem {
                Button("Move today to tomorrow", systemImage: "arrow.uturn.right") {
                    confirmingMove = true
                }
                .disabled(model.plan?.stillOpen.isEmpty ?? true)
            }
        }
        .confirmationDialog(
            "Move \(model.plan?.stillOpen.count ?? 0) open tasks to tomorrow?",
            isPresented: $confirmingMove,
            titleVisibility: .visible
        ) {
            Button("Move them") {
                Task { await model.moveEverythingToTomorrow() }
            }
        } message: {
            // Each one counts as a deferral, and the row says so from the
            // second time — which is the pattern worth seeing, and the reason
            // this asks first rather than making it a one-click habit.
            Text(
                "Each task's deferral count goes up. That is the point: it is how a task "
                    + "that keeps slipping becomes visible."
            )
        }
        .task { await model.refresh() }
        .task { await model.follow() }
    }
}
