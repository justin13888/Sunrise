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
                    L10n.Routines.emptyTitle,
                    systemImage: "repeat",
                    description: Text(L10n.Routines.emptyDescription)
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
        .navigationTitle(L10n.Routines.title)
        .toolbar {
            ToolbarItem {
                Button(L10n.Routines.newRoutine, systemImage: "plus") { creating = true }
                    .disabledUnlessEditable(.routine, .task)
            }
            ToolbarItem {
                Menu(L10n.Routines.more, systemImage: "ellipsis.circle") {
                    Toggle(L10n.Routines.showArchived, isOn: $model.showsArchived)
                    Button(L10n.Routines.generateNow) { Task { await model.materializeNow() } }
                        .disabledUnlessEditable(.task)
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
            L10n.Routines.deleteConfirmTitle(title: confirmingDelete?.template.title ?? ""),
            isPresented: Binding(
                get: { confirmingDelete != nil },
                set: { if !$0 { confirmingDelete = nil } }
            ),
            titleVisibility: .visible
        ) {
            Button(L10n.Action.delete, role: .destructive) {
                guard let routine = confirmingDelete else { return }
                confirmingDelete = nil
                Task { await model.delete(routine) }
            }
        } message: {
            Text(L10n.Routines.deleteConfirmMessage)
        }
    }

    @ViewBuilder
    private func menu(for routine: RoutineItem) -> some View {
        Button(L10n.Routines.editEllipsis) { editing = routine }
        // Skipping can tombstone the occurrence's task, and pausing or
        // archiving is an update, which re-materializes: each writes tasks.
        Group {
            Button(L10n.Routines.skipNext) { Task { await model.skipNext(routine) } }
            Button(routine.paused ? L10n.Routines.resume : L10n.Routines.pause) {
                Task { await model.setPaused(routine, !routine.paused) }
            }
            Button(routine.archived ? L10n.Routines.unarchive : L10n.Routines.archive) {
                Task { await model.setArchived(routine, !routine.archived) }
            }
        }
        .disabledUnlessEditable(.routine, .task)
        Divider()
        Button(L10n.Routines.deleteEllipsis, role: .destructive) { confirmingDelete = routine }
            .disabledUnlessEditable(.routine)
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
                            .help(L10n.Routines.pausedHelp)
                    }
                }
                HStack(spacing: 8) {
                    Text(cadence).font(.caption).foregroundStyle(.secondary)
                    if let streamName {
                        Text("#\(streamName)").font(.caption).foregroundStyle(.secondary)
                    }
                    if let next {
                        Text(L10n.Routines.next(day: next.text))
                            .font(.caption)
                            .foregroundStyle(next.isPast ? .orange : .secondary)
                    }
                    if routine.streakCounter > 0 {
                        Text(L10n.Routines.streak(count: Int(routine.streakCounter)))
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
