import SwiftUI

/// Any list of tasks: capture at the top where it makes sense, rows below.
struct TaskListView: View {
    let model: TaskListModel
    @Bindable var capture: CaptureModel

    var body: some View {
        VStack(spacing: 0) {
            // A context or search list is where capture has nowhere to land: a
            // capture writes to a stream, and there is no annotation for
            // "give this the context I am looking at".
            if model.kind.acceptsCapture {
                CaptureBar(model: capture) { draft in await model.create(draft) }
            }
            TaskRows(model: model)
        }
        .navigationTitle(model.kind.title)
        .task(id: model.kind) { await model.refresh() }
        .task { await model.follow() }
    }
}

/// The rows, their grouping, and everything that can be done to one.
///
/// Split out of `TaskListView` because Search shows the same rows under a
/// different header, and two copies of the context menu would be two places
/// for "Defer a week" to mean different things.
struct TaskRows: View {
    let model: TaskListModel

    @State private var editing: TaskItem?
    @State private var foldedSections: Set<Int> = [TodaySection.overdue.rank]

    var body: some View {
        Group {
            if let message = model.errorMessage {
                Label(message, systemImage: "exclamationmark.triangle")
                    .font(.callout)
                    .foregroundStyle(.orange)
                    .padding(.horizontal, 12)
                    .padding(.top, 6)
            }

            if model.tasks.isEmpty {
                ContentUnavailableView(
                    model.kind.title,
                    systemImage: model.kind.symbol,
                    description: Text(model.kind.emptyMessage)
                )
            } else {
                List {
                    if model.kind.isToday {
                        ForEach(model.groups) { group in
                            section(group)
                        }
                    } else {
                        ForEach(model.tasks, id: \.id) { row($0) }
                    }
                }
                .listStyle(.inset)
            }
        }
        .sheet(item: $editing) { task in
            TaskEditorView(
                task: task,
                apply: { await model.apply($0, to: task) },
                delete: { await model.delete(task) }
            )
        }
    }

    @ViewBuilder
    private func section(_ group: TodayGroup) -> some View {
        let isFolded = foldedSections.contains(group.section.rank)
        Section {
            if !isFolded {
                ForEach(group.tasks, id: \.id) { row($0) }
            }
        } header: {
            Button {
                if isFolded {
                    foldedSections.remove(group.section.rank)
                } else {
                    foldedSections.insert(group.section.rank)
                }
            } label: {
                HStack(spacing: 6) {
                    Image(systemName: isFolded ? "chevron.right" : "chevron.down")
                        .font(.caption2)
                    Text(group.section.heading)
                    Text("\(group.tasks.count)")
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                }
            }
            .buttonStyle(.plain)
        }
    }

    private func row(_ task: TaskItem) -> some View {
        TaskRowView(
            facets: model.facets(for: task),
            complete: { await model.complete(task) },
            edit: { editing = task }
        )
        .contextMenu {
            if task.state == .done || task.state == .cancelled {
                Button("Reopen") { Task { await model.reopen(task) } }
            } else {
                Button("Complete") { Task { await model.complete(task) } }
            }
            Button("Edit…") { editing = task }
            Divider()
            Button("Defer to tomorrow") { Task { await model.defer_(task, byDays: 1) } }
            Button("Defer a week") { Task { await model.defer_(task, byDays: 7) } }
            Divider()
            Button("Delete", role: .destructive) { Task { await model.delete(task) } }
        }
        .swipeActions(edge: .trailing) {
            Button("Delete", role: .destructive) { Task { await model.delete(task) } }
            Button("Tomorrow") { Task { await model.defer_(task, byDays: 1) } }
                .tint(.orange)
        }
    }
}

/// The generated record has an `id` already; conforming makes `sheet(item:)`
/// and `ForEach` take it directly.
extension TaskItem: Identifiable {}
