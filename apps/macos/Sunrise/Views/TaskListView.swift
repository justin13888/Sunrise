import SwiftUI

/// Today or Inbox: capture at the top, rows below.
struct TaskListView: View {
    let model: TaskListModel
    @Bindable var capture: CaptureModel

    @State private var editing: TaskItem?
    @State private var foldedSections: Set<Int> = [TodaySection.overdue.rank]

    var body: some View {
        VStack(spacing: 0) {
            // A context list is the one place capture has nowhere to land: a
            // capture writes to a stream, and there is no annotation for
            // "give this the context I am looking at".
            if model.kind.acceptsCapture {
                CaptureBar(model: capture) { draft in await model.create(draft) }
            }

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
                    if model.kind == .today {
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
        .navigationTitle(model.kind.title)
        .sheet(item: $editing) { task in
            TaskEditorView(
                task: task,
                apply: { await model.apply($0, to: task) },
                delete: { await model.delete(task) }
            )
        }
        .task(id: model.kind) { await model.refresh() }
        .task { await model.follow() }
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
