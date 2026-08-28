import SwiftUI

/// Which pane of the task editor is showing.
///
/// Attachments and the activity timeline are per-task reads with their own
/// queries and their own change subscriptions, so they are panes rather than
/// sections of one form: a form that ran three queries to draw a priority
/// picker would pay for all three every time anyone edited a title.
enum TaskEditorPane: String, CaseIterable, Identifiable {
    case details
    case attachments
    case activity

    var id: Self { self }

    var title: String {
        switch self {
        case .details: "Details"
        case .attachments: "Attachments"
        case .activity: "Activity"
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
    @State private var title: String
    @State private var priority: Int
    @State private var energy: Energy?
    @State private var estimateMinutes: Int
    @State private var hasDue: Bool
    @State private var due: Date

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
        _activity = State(initialValue: ActivityModel(bridge: bridge, entity: task.id))
        _title = State(initialValue: task.title)
        _priority = State(initialValue: Int(task.priority ?? 0))
        _energy = State(initialValue: task.energy)
        _estimateMinutes = State(initialValue: Int((task.estimatedDurationS ?? 0) / 60))
        _hasDue = State(initialValue: task.dueAt != nil)
        // Seeded from the task's own deadline, resolved by the seam. Starting
        // the picker at "now" and then submitting it — which is what this did
        // — silently moved every deadline to today the moment anyone opened
        // the sheet to change something else.
        _due = State(initialValue: task.dueAt.map {
            Date(
                timeIntervalSince1970: Double(
                    timeValueMs(value: $0, tz: TimeZone.current.identifier)
                ) / 1000
            )
        } ?? Date())
    }

    var body: some View {
        VStack(spacing: 0) {
            Picker("Pane", selection: $pane) {
                ForEach(TaskEditorPane.allCases) { Text($0.title).tag($0) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .padding([.horizontal, .top], 12)

            switch pane {
            case .details: details
            case .attachments: AttachmentsView(model: attachments)
            case .activity: ActivityTimelineView(model: activity)
            }
        }
        .frame(width: 460)
        .padding(.vertical, 8)
    }

    private var details: some View {
        Form {
            TextField("Title", text: $title)

            Picker("Priority", selection: $priority) {
                Text("None").tag(0)
                ForEach(1...5, id: \.self) { Text("!\($0)").tag($0) }
            }

            Picker("Energy", selection: $energy) {
                Text(energyLabel(energy: nil)).tag(nil as Energy?)
                Text(energyLabel(energy: .low)).tag(Energy.low as Energy?)
                Text(energyLabel(energy: .med)).tag(Energy.med as Energy?)
                Text(energyLabel(energy: .high)).tag(Energy.high as Energy?)
            }

            LabeledContent("Estimate") {
                HStack {
                    TextField("", value: $estimateMinutes, format: .number)
                        .frame(width: 60)
                    Text(estimateMinutes > 0 ? shortDuration(secs: UInt64(estimateMinutes * 60)) : "none")
                        .foregroundStyle(.secondary)
                }
            }

            Toggle("Has a deadline", isOn: $hasDue)
            if hasDue {
                DatePicker("Due", selection: $due, displayedComponents: [.date, .hourAndMinute])
            }

            Section {
                HStack {
                    Button("Delete", role: .destructive) {
                        Task {
                            await delete()
                            dismiss()
                        }
                    }
                    Spacer()
                    Button("Cancel") { dismiss() }
                    Button("Save") {
                        Task {
                            await apply(edit)
                            dismiss()
                        }
                    }
                    .keyboardShortcut(.defaultAction)
                    .disabled(title.trimmed.isEmpty)
                }
            }
        }
        .formStyle(.grouped)
    }

    /// The split-optional shape the seam uses: `set` carries a new value,
    /// `clear` empties the field, and setting both means clear.
    private var edit: TaskEdit {
        var edit = TaskEdit()
        edit.title = title.trimmed
        if priority == 0 {
            edit.clearPriority = true
        } else {
            edit.setPriority = UInt8(priority)
        }
        if let energy {
            edit.setEnergy = energy
        } else {
            edit.clearEnergy = true
        }
        if estimateMinutes > 0 {
            edit.setEstimatedDurationS = UInt64(estimateMinutes * 60)
        } else {
            edit.clearEstimatedDuration = true
        }
        if hasDue {
            // An instant, not a civil time: a deadline the user picked on a
            // wall clock in this zone is a fixed point, and the seam is what
            // decides how it is stored.
            edit.setDueAt = .instant(at: Timestamp(due.timeIntervalSince1970 * 1000))
        } else {
            edit.clearDueAt = true
        }
        return edit
    }
}
