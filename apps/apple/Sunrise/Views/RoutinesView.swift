import SwiftUI

/// The routines list.
struct RoutinesView: View {
    @Bindable var model: RoutineModel

    @State private var editing: RoutineItem?
    @State private var creating = false
    @State private var confirmingDelete: RoutineItem?

    var body: some View {
        VStack(spacing: 0) {
            if let note = model.undoNote {
                NoteBanner(text: note) { model.dismissUndoNote() }
            }
            if let message = model.errorMessage {
                Label(message, systemImage: "exclamationmark.triangle")
                    .font(.callout)
                    .foregroundStyle(.orange)
                    .padding(12)
            }
            if model.visible.isEmpty {
                ContentUnavailableView(
                    "No routines",
                    systemImage: "repeat",
                    description: Text("A routine generates its task on a cadence you describe.")
                )
            } else {
                List(model.visible, id: \.id) { routine in
                    RoutineRowView(
                        routine: routine,
                        cadence: model.cadence(of: routine),
                        next: model.nextLabel(for: routine),
                        streamName: model.names.stream(routine.template.streamId)
                    )
                    .contextMenu { menu(for: routine) }
                }
                .listStyle(.inset)
            }
        }
        .navigationTitle("Routines")
        .toolbar {
            ToolbarItem {
                Button("New routine", systemImage: "plus") { creating = true }
            }
            ToolbarItem {
                Menu("More", systemImage: "ellipsis.circle") {
                    Toggle("Show archived", isOn: $model.showsArchived)
                    Button("Generate now") { Task { await model.materializeNow() } }
                }
            }
        }
        .task { await model.refresh() }
        .task { await model.follow() }
        .onChange(of: model.showsArchived) { Task { await model.refresh() } }
        .sheet(isPresented: $creating) {
            RoutineEditorView(routine: nil, streams: model.names) { draft, _ in
                if let draft { await model.create(draft) }
            }
        }
        .sheet(item: $editing) { routine in
            RoutineEditorView(routine: routine, streams: model.names) { _, edit in
                if let edit { await model.update(routine, edit) }
            }
        }
        .confirmationDialog(
            "Delete “\(confirmingDelete?.template.title ?? "")”?",
            isPresented: Binding(
                get: { confirmingDelete != nil },
                set: { if !$0 { confirmingDelete = nil } }
            ),
            titleVisibility: .visible
        ) {
            Button("Delete", role: .destructive) {
                guard let routine = confirmingDelete else { return }
                confirmingDelete = nil
                Task { await model.delete(routine) }
            }
        } message: {
            Text(
                "Generation stops and this cannot be undone. Tasks it already made stay. "
                    + "Pause it instead to stop generation and keep it."
            )
        }
    }

    @ViewBuilder
    private func menu(for routine: RoutineItem) -> some View {
        Button("Edit…") { editing = routine }
        Button("Skip next occurrence") { Task { await model.skipNext(routine) } }
        Button(routine.paused ? "Resume" : "Pause") {
            Task { await model.setPaused(routine, !routine.paused) }
        }
        Button(routine.archived ? "Unarchive" : "Archive") {
            Task { await model.setArchived(routine, !routine.archived) }
        }
        Divider()
        Button("Delete…", role: .destructive) { confirmingDelete = routine }
    }
}

/// One routine: what it makes, how often, and when it next fires.
struct RoutineRowView: View {
    let routine: RoutineItem
    /// From `recurrence_summary` — the inverse of the phrase that made it.
    let cadence: String
    let next: DayLabel?
    let streamName: String?

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    Text(routine.template.title)
                        .foregroundStyle(routine.paused || routine.archived ? .secondary : .primary)
                    if routine.paused {
                        Image(systemName: "pause.circle")
                            .foregroundStyle(.secondary)
                            .help("Paused — generates nothing")
                    }
                }
                HStack(spacing: 8) {
                    Text(cadence).font(.caption).foregroundStyle(.secondary)
                    if let streamName {
                        Text("#\(streamName)").font(.caption).foregroundStyle(.secondary)
                    }
                    if let next {
                        Text("next \(next.text)")
                            .font(.caption)
                            .foregroundStyle(next.isPast ? .orange : .secondary)
                    }
                    if routine.streakCounter > 0 {
                        Text("streak \(routine.streakCounter)")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .monospacedDigit()
                    }
                }
            }
            Spacer(minLength: 0)
        }
        .padding(.vertical, 3)
    }
}

extension RoutineItem: Identifiable {}
