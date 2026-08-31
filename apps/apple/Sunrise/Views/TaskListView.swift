import SwiftUI

/// Any list of tasks: capture at the top where it makes sense, rows below.
struct TaskListView: View {
    let model: TaskListModel
    @Bindable var capture: CaptureModel
    let selection: ListSelection
    let sheets: RowSheets
    let preferences: KeyboardPreferences
    var escapes = ListEscapes()
    var focus: FocusState<PaneFocus?>.Binding

    var body: some View {
        VStack(spacing: 0) {
            // A context or search list is where capture has nowhere to land: a
            // capture writes to a stream, and there is no annotation for
            // "give this the context I am looking at".
            if model.kind.acceptsCapture {
                CaptureBar(model: capture, focus: focus) { draft in await model.create(draft) }
            }
            TaskRows(
                model: model,
                selection: selection,
                sheets: sheets,
                preferences: preferences,
                escapes: escapes,
                focus: focus
            )
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
    let selection: ListSelection
    let sheets: RowSheets
    let preferences: KeyboardPreferences
    var escapes = ListEscapes()
    var focus: FocusState<PaneFocus?>.Binding

    @State private var vim = VimNormalMode()
    /// Which row a drag is over, for the drop-target treatment.
    @State private var targeted: EntityRef?
    @State private var foldedSections: Set<Int> = [TodaySection.overdue.rank]

    /// The rows the keyboard can reach: what is drawn, folded sections
    /// excluded. A cursor inside a collapsed section would be a cursor nobody
    /// can see, and `X` would then complete a task that is not on screen.
    private var visible: [TaskItem] {
        guard model.kind.isToday else { return model.tasks }
        return model.groups
            .filter { !foldedSections.contains($0.section.rank) }
            .flatMap(\.tasks)
    }

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
                list
            }
        }
        // The list is a query result, so it can change under the cursor at any
        // moment — a sync, a completion, a fold. The selection is told every
        // time, which is the only reason a cursor survives completing the row
        // it was standing on.
        .onChange(of: visible.map(\.id), initial: true) { _, ids in
            selection.reconcile(with: ids)
            vim.reset()
        }
    }

    private var list: some View {
        List(selection: selectionBinding) {
            if model.kind.isToday {
                ForEach(model.groups) { group in
                    section(group)
                }
            } else {
                ForEach(model.tasks, id: \.id) { row($0) }
            }
        }
        .listStyle(.inset)
        // The second way out of the capture keyboard, beside the Done button
        // the bar puts above it. Reaching for the list is what someone does
        // when they have finished typing and want to see what they wrote, so
        // the drag that gets them there is allowed to mean it. Inert on macOS,
        // which has no software keyboard to dismiss.
        .scrollDismissesKeyboard(.interactively)
        .focused(focus, equals: .rows)
        // Every row-scoped binding in `docs/08-features/keyboard.md` arrives
        // here, and only here: a focused capture field consumes its own
        // keystrokes before the list ever sees them, which is what lets `d`
        // mean Defer and still be a letter you can type.
        .onKeyChord(scope: .list, vim: preferences.vimMode ? vim : nil) { action in
            ListCommand.perform(
                action,
                list: model,
                selection: selection,
                sheets: sheets,
                escapes: escapes
            )
        }
        .accessibilityLabel(model.kind.title)
    }

    /// The list's own selection, expressed over the model.
    ///
    /// Bound rather than left to SwiftUI so that a click, a shift-click and an
    /// arrow key all end up in one place. It also means the mouse and the
    /// keyboard cannot disagree: whatever the list decides, ``ListSelection``
    /// is told, and every action reads the answer from there.
    private var selectionBinding: Binding<Set<EntityRef>> {
        Binding(
            get: { Set(selection.targets) },
            set: { selection.adopt($0) }
        )
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
            isCursor: selection.cursor == task.id,
            isTicked: selection.explicit.contains(task.id),
            complete: { await model.complete(task) },
            edit: { sheets.editing = task }
        )
        .tag(task.id)
        // **Task → Task (reorder).** Dropping a row onto another puts it
        // immediately above that one; a whole ⇧-selected run moves together
        // and keeps its internal order.
        //
        // A drop target rather than `ForEach.onMove`, because the row is
        // already `.draggable` — it has to reach the calendar grid and the
        // sidebar too — and `onMove` would take that drag over for itself.
        //
        // Declined outright in Today and in Search: the first is sectioned by
        // urgency and the second ranked by relevance, so a row dropped there
        // would snap back on the next refresh. Returning `false` is what makes
        // the cursor say "no" rather than promising a move that never happens.
        .dropDestination(for: String.self) { items, _ in
            let moved = DropPayload.taskIDs(items).filter { $0 != task.id }
            guard !moved.isEmpty else { return false }
            return model.reorder(moved, before: task.id)
        } isTargeted: { targeted = $0 ? task.id : (targeted == task.id ? nil : targeted) }
        .dropHighlight(isActive: model.acceptsReordering && targeted == task.id)
        .contextMenu { rowMenu(task) }
        .swipeActions(edge: .trailing) {
            Button("Delete", role: .destructive) { Task { await model.delete(task) } }
            Button("Tomorrow") { Task { await model.defer_(task, byDays: 1) } }
                .tint(.orange)
        }
    }

    /// The context menu, which is also the visible affordance every row
    /// shortcut is required to have — see
    /// `docs/10-cross-cutting/accessibility.md`. Each item prints its key, so
    /// the menu teaches the keymap rather than merely duplicating it.
    @ViewBuilder
    private func rowMenu(_ task: TaskItem) -> some View {
        if task.state == .done || task.state == .cancelled {
            Button("Reopen") { Task { await model.reopen(task) } }
        } else {
            Button(labelled("Complete", .markDone)) { Task { await model.complete(task) } }
        }
        Button(labelled("Edit…", .openDetail)) { sheets.editing = task }
        Divider()
        Button(labelled("Defer to tomorrow", .deferTask)) {
            Task { await model.defer_(task, byDays: 1) }
        }
        Button("Defer a week") { Task { await model.defer_(task, byDays: 7) } }
        Button(labelled("Schedule…", .schedule)) { sheets.scheduling = TaskBatch([task]) }
        Button(labelled("Move to stream…", .moveToStream)) { sheets.moving = TaskBatch([task]) }
        Button(labelled("Start focus session", .focusMode)) {
            Task {
                await model.startFocus(task)
                escapes.showFocus()
            }
        }
        Divider()
        Button("Delete", role: .destructive) { Task { await model.delete(task) } }
    }

    /// A menu title with its key beside it.
    ///
    /// `contextMenu` items cannot carry a `keyboardShortcut` — the keys here
    /// are bare letters bound on the list, not menu equivalents — so the label
    /// is where the binding becomes visible. Read from `Keymap` so it cannot
    /// claim a key the list does not answer.
    private func labelled(_ title: String, _ action: AppAction) -> String {
        let keys = Keymap.shortcutLabel(for: action)
        return keys.isEmpty ? title : "\(title)   \(keys)"
    }
}

/// `S`: give the selection a time.
struct ScheduleSheet: View {
    let count: Int
    let commit: (Date) async -> Void

    @Environment(\.dismiss) private var dismiss
    @State private var date = Date()

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(count == 1 ? "Schedule" : "Schedule \(count) tasks")
                .font(.headline)
            DatePicker("When", selection: $date)
                .datePickerStyle(.graphical)
                .labelsHidden()
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button("Schedule") {
                    Task {
                        await commit(date)
                        dismiss()
                    }
                }
                .keyboardShortcut(.defaultAction)
            }
        }
        .padding(16)
        .frame(width: 380)
    }
}

/// `M`: move the selection into a stream.
struct MoveToStreamSheet: View {
    let count: Int
    let streams: [StreamChoice]
    let commit: (EntityRef) async -> Void

    @Environment(\.dismiss) private var dismiss
    @State private var chosen: EntityRef = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(count == 1 ? "Move to stream" : "Move \(count) tasks")
                .font(.headline)
            Picker("Stream", selection: $chosen) {
                ForEach(streams) { stream in
                    Text(stream.name).tag(stream.id)
                }
            }
            .labelsHidden()
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button("Move") {
                    Task {
                        await commit(chosen)
                        dismiss()
                    }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(chosen.isEmpty)
            }
        }
        .padding(16)
        .frame(width: 320)
        .onAppear { chosen = streams.first?.id ?? "" }
    }
}

/// The generated record has an `id` already; conforming makes `sheet(item:)`
/// and `ForEach` take it directly.
extension TaskItem: Identifiable {}
