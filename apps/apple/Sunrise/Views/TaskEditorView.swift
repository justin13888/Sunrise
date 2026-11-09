import SwiftUI

/// Which pane of the task editor is showing.
///
/// Attachments and the activity timeline are per-task reads with their own
/// queries and their own change subscriptions, so they are panes rather than
/// sections of one form: a form that ran three queries to draw a priority
/// picker would pay for all three every time anyone edited a title.
enum TaskEditorPane: String, CaseIterable, Identifiable {
    case details
    case notes
    case attachments
    case activity

    var id: Self { self }

    var title: String {
        switch self {
        case .details: L10n.TaskEditor.paneDetails
        case .notes: L10n.TaskEditor.paneNotes
        case .attachments: L10n.TaskEditor.paneAttachments
        case .activity: L10n.TaskEditor.paneActivity
        }
    }
}

/// Edit the facets a Today or Inbox row shows.
///
/// Only the fields these two views render. An editor that offered everything
/// `TaskEdit` can express would be a form nobody reads; the rest arrives with
/// the views that show it.
struct TaskEditorView: View {
    let task: TaskItem
    let apply: (TaskEdit) async -> Void
    let delete: () async -> Void

    @Environment(\.dismiss) private var dismiss
    @State private var pane: TaskEditorPane = .details
    @State private var attachments: AttachmentsModel
    @State private var activity: ActivityModel
    @State private var note: NoteEditorModel
    @State private var fields: TaskEditorFields

    init(
        task: TaskItem,
        bridge: CoreBridge,
        apply: @escaping (TaskEdit) async -> Void,
        delete: @escaping () async -> Void
    ) {
        self.task = task
        self.apply = apply
        self.delete = delete
        _attachments = State(initialValue: AttachmentsModel(bridge: bridge, task: task.id))
        // Seeded from the task's own body. The editor decides for itself
        // whether that body is one it may write back — see `NoteEditorModel`.
        _note = State(initialValue: NoteEditorModel(body: task.body))
        _activity = State(initialValue: ActivityModel(bridge: bridge, entity: task.id))
        _fields = State(initialValue: TaskEditorFields(task: task))
    }

    var body: some View {
        VStack(spacing: 0) {
            Picker(L10n.TaskEditor.panePicker, selection: $pane) {
                ForEach(TaskEditorPane.allCases) { Text($0.title).tag($0) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .padding([.horizontal, .top], 12)

            switch pane {
            case .details: details
            case .notes: NoteBodyEditor(model: note)
            case .attachments: AttachmentsView(model: attachments)
            case .activity: ActivityTimelineView(model: activity)
            }
        }
        .macSheetFrame(width: 460)
        .padding(.vertical, 8)
    }

    private var details: some View {
        Form {
            TextField(L10n.TaskEditor.titleField, text: $fields.title)

            Picker(L10n.TaskEditor.priority, selection: $fields.priority) {
                Text(L10n.TaskEditor.priorityNone).tag(0)
                ForEach(1...5, id: \.self) { Text("!\($0)").tag($0) }
            }

            Picker(L10n.TaskEditor.energy, selection: $fields.energy) {
                Text(energyLabel(energy: nil)).tag(nil as Energy?)
                Text(energyLabel(energy: .low)).tag(Energy.low as Energy?)
                Text(energyLabel(energy: .med)).tag(Energy.med as Energy?)
                Text(energyLabel(energy: .high)).tag(Energy.high as Energy?)
                // An energy a newer client wrote: offered as itself, so saving
                // without touching it keeps it.
                if let held = task.energy, case .unknown = held {
                    Text(energyLabel(energy: held)).tag(held as Energy?)
                }
            }

            LabeledContent(L10n.TaskEditor.estimate) {
                HStack {
                    TextField("", value: $fields.estimateMinutes, format: .number)
                        .textInput(.number)
                        .frame(width: 60)
                    Text(fields.estimateMinutes > 0
                        ? shortDuration(secs: UInt64(fields.estimateMinutes * 60))
                        : L10n.TaskEditor.estimateNone)
                        .foregroundStyle(.secondary)
                }
            }

            Toggle(L10n.TaskEditor.hasDeadline, isOn: $fields.hasDue)
            if fields.hasDue {
                DatePicker(
                    L10n.TaskEditor.due,
                    selection: $fields.due,
                    displayedComponents: [.date, .hourAndMinute]
                )
            }

            Section {
                HStack {
                    Button(L10n.Action.delete, role: .destructive) {
                        Task {
                            await delete()
                            dismiss()
                        }
                    }
                    // Both writes wait while the vault locks tasks on this
                    // build (ADR-0045 §8); Cancel never writes, so it stays.
                    .disabledUnlessEditable(.task)
                    Spacer()
                    Button(L10n.Action.cancel) { dismiss() }
                    Button(L10n.Action.save) {
                        // A form the seam cannot write (a zone it does not
                        // know, a moment no timestamp holds) writes nothing:
                        // the sheet stays open on what the user entered.
                        guard var edit = try? fields.edit(of: task) else { return }
                        // Sets neither body field unless the note was
                        // actually changed; a body it could not fully read
                        // is one it must leave exactly as it found it.
                        note.apply(to: &edit)
                        Task {
                            await apply(edit)
                            dismiss()
                        }
                    }
                    .keyboardShortcut(.defaultAction)
                    .disabled(fields.title.trimmed.isEmpty)
                    .disabledUnlessEditable(.task)
                }
            }
        }
        .formStyle(.grouped)
    }
}

/// What the task editor's details pane holds, and the edit a save makes.
///
/// The seam seeds it (`taskEditorForm`) and turns it back into an edit
/// (`taskEditorEdit`) that carries only what the user changed, with a moved
/// deadline kept in the kind it was stored as. A title edit therefore sends
/// neither `setDueAt` nor `clearDueAt`, and an all-day, zoned, floating or
/// unknown deadline survives it (#515, ADR-0045 §6, ADR-0066 §4). Split from
/// the view so a save is testable without a window.
struct TaskEditorFields {
    var title: String
    /// 1...5, or 0 for none.
    var priority: Int
    var energy: Energy?
    /// Whole minutes; 0 is no estimate.
    var estimateMinutes: Int
    var hasDue: Bool
    /// The deadline picker. Seeded from the task's own deadline — starting it
    /// at "now" moved every deadline to today the moment anyone opened the
    /// sheet to change something else — and from now only when there is none.
    var due: Date

    /// The form the seam seeded. A save posts it back with the fields above
    /// laid over it, so what this sheet does not show (the soft deadline) is
    /// handed back exactly as it came.
    private let seed: TaskEditorForm
    /// The IANA zone the seam reads and writes the deadline in.
    private let zone: String

    init(task: TaskItem, zone: String = TimeZone.current.identifier) {
        self.zone = zone
        // A zone the seam refuses still opens the sheet on the task's values;
        // the save then fails in `edit(of:)` rather than writing in the wrong
        // zone.
        let seed = (try? taskEditorForm(task: task, tz: zone)) ?? TaskEditorForm(
            title: task.title,
            priority: task.priority,
            energy: task.energy,
            estimatedDurationS: task.estimatedDurationS,
            dueAt: task.dueAt.map { PickedTime(ms: timeValueMs(value: $0, tz: zone), kind: nil) }
        )
        self.seed = seed
        title = seed.title
        priority = Int(seed.priority ?? 0)
        energy = seed.energy
        estimateMinutes = Int((seed.estimatedDurationS ?? 0) / 60)
        hasDue = seed.dueAt != nil
        due = seed.dueAt.map { Date(timeIntervalSince1970: Double($0.ms) / 1000) } ?? Date()
    }

    /// The edit these fields make to `task`: only what changed. The body is
    /// not this form's; the caller adds the note editor's fields.
    ///
    /// Throws when the seam cannot write the form: a zone it does not know,
    /// or a moment no timestamp holds.
    func edit(of task: TaskItem) throws -> TaskEdit {
        var form = seed
        form.title = title
        form.priority = priority == 0 ? nil : UInt8(priority)
        form.energy = energy
        // Minutes left where they were seeded post back the seed's own
        // seconds, so an untouched 90 s estimate is not rewritten as 60.
        let seededMinutes = Int((seed.estimatedDurationS ?? 0) / 60)
        form.estimatedDurationS = estimateMinutes == seededMinutes
            ? seed.estimatedDurationS
            : (estimateMinutes > 0 ? UInt64(estimateMinutes * 60) : nil)
        // Rounded, not truncated: the seed's milliseconds survive the trip
        // through `Date`'s floating seconds, so an untouched picker compares
        // equal to its seed and the seam sends no deadline at all.
        form.dueAt = hasDue
            ? PickedTime(ms: Int64((due.timeIntervalSince1970 * 1000).rounded()), kind: nil)
            : nil
        return try taskEditorEdit(task: task, form: form, tz: zone)
    }
}
