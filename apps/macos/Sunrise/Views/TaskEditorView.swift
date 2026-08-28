import SwiftUI

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
    @State private var title: String
    @State private var priority: Int
    @State private var energy: Energy?
    @State private var estimateMinutes: Int
    @State private var hasDue: Bool
    @State private var due: Date

    init(task: TaskItem, apply: @escaping (TaskEdit) async -> Void, delete: @escaping () async -> Void) {
        self.task = task
        self.apply = apply
        self.delete = delete
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
        .frame(width: 420)
        .padding(.vertical, 8)
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
