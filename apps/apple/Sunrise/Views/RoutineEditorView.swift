import SwiftUI

/// Create or edit a routine, with the cadence typed in plain English.
///
/// The recurrence field is the point of this sheet. It reads what the user
/// types with `sunrise_domain::parse_recurrence` and echoes it back with
/// `rrule_summary` — so `every mon, wed` is confirmed as `every week on Mo,
/// We` before it is saved. Nothing here parses or words a cadence; a second
/// parser in Swift is how `weekends` would come to mean two days in one client
/// and something else in another.
struct RoutineEditorView: View {
    let routine: RoutineItem?
    let streams: NameBook
    /// One of the two is non-nil: a draft for a creation, an edit otherwise.
    let commit: (RoutineDraftIn?, RoutineEdit?) async -> Void

    @Environment(\.dismiss) private var dismiss
    @State private var title: String
    @State private var recurrence: RecurrenceField
    @State private var streamID: EntityRef
    @State private var timeZone: String
    @State private var priority: Int
    @State private var energy: Energy?
    @State private var estimateMinutes: Int
    @State private var catchup: RoutineCatchupPolicy
    @State private var hasEnd: Bool
    @State private var endsAt: Date

    init(
        routine: RoutineItem?,
        streams: NameBook,
        commit: @escaping (RoutineDraftIn?, RoutineEdit?) async -> Void
    ) {
        self.routine = routine
        self.streams = streams
        self.commit = commit
        let template = routine?.template
        _title = State(initialValue: template?.title ?? "")
        // An existing routine's phrase is its rule described back. That is the
        // only honest starting text: the phrase originally typed is not
        // stored, and the summary is what the rule actually means.
        _recurrence = State(initialValue: RecurrenceField(
            text: routine.map { recurrenceSummary(rule: $0.rrule) } ?? "every day"
        ))
        _streamID = State(initialValue: template?.streamId ?? inboxStreamId())
        _timeZone = State(initialValue: routine?.timezone ?? TimeZone.current.identifier)
        _priority = State(initialValue: Int(template?.priority ?? 0))
        _energy = State(initialValue: template?.energy)
        _estimateMinutes = State(initialValue: Int((template?.estimatedDurationS ?? 0) / 60))
        _catchup = State(initialValue: routine?.catchupPolicy ?? .skip)
        _hasEnd = State(initialValue: routine?.endsAt != nil)
        _endsAt = State(
            initialValue: routine?.endsAt
                .map { Date(timeIntervalSince1970: Double($0) / 1000) } ?? Date()
        )
    }

    private var isCreating: Bool { routine == nil }

    var body: some View {
        Form {
            TextField("Title", text: $title, prompt: Text("Water the plants"))

            Section("Cadence") {
                TextField(
                    "Repeats",
                    text: $recurrence.text,
                    prompt: Text("every 2 weeks on tue")
                )
                // A grammar too, and one the core parses word by word: an
                // autocorrect that turns `tue` into `true` produces a rule
                // the field then reports as unreadable.
                .textInput(.syntax)
                if let summary = recurrence.summary {
                    Label(summary, systemImage: "checkmark.circle")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                } else if let problem = recurrence.problem {
                    Label(problem, systemImage: "exclamationmark.triangle")
                        .font(.caption)
                        .foregroundStyle(.orange)
                }
                Picker("Timezone", selection: $timeZone) {
                    ForEach(Self.zones, id: \.self) { Text($0).tag($0) }
                }
                Picker("Missed occurrences", selection: $catchup) {
                    Text("Skip").tag(RoutineCatchupPolicy.skip)
                    Text("Merge into one").tag(RoutineCatchupPolicy.merge)
                    Text("Queue them all").tag(RoutineCatchupPolicy.queue)
                }
                Toggle("Stops on a date", isOn: $hasEnd)
                if hasEnd {
                    DatePicker("Until", selection: $endsAt, displayedComponents: [.date])
                }
            }

            Section("Each task") {
                Picker("Stream", selection: $streamID) {
                    ForEach(streams.streamsByName, id: \.0) { id, name in
                        Text(name).tag(id)
                    }
                }
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
                            .textInput(.number)
                            .frame(width: 60)
                        Text(estimateMinutes > 0
                            ? shortDuration(secs: UInt64(estimateMinutes * 60))
                            : "none")
                            .foregroundStyle(.secondary)
                    }
                }
            }

            Section {
                HStack {
                    Spacer()
                    Button("Cancel") { dismiss() }
                    Button(isCreating ? "Create" : "Save", action: save)
                        .keyboardShortcut(.defaultAction)
                        .disabled(!canSave)
                }
            }
        }
        .formStyle(.grouped)
        .macSheetFrame(width: 440)
        .padding(.vertical, 8)
        .navigationTitle(isCreating ? "New routine" : "Edit routine")
    }

    /// The device's zone first, then the rest. A routine is nearly always in
    /// the zone of the person setting it up, and scrolling six hundred names
    /// to find that one is not a feature.
    private static let zones: [String] = {
        let current = TimeZone.current.identifier
        let rest = TimeZone.knownTimeZoneIdentifiers.filter { $0 != current }.sorted()
        return [current] + rest
    }()

    private var canSave: Bool {
        !title.trimmed.isEmpty && recurrence.isValid
    }

    private func save() {
        guard let rule = recurrence.rule else { return }
        let template = Template(
            title: title.trimmed,
            streamId: streamID,
            contexts: routine?.template.contexts ?? [],
            energy: energy,
            priority: priority == 0 ? nil : UInt8(priority),
            estimatedDurationS: estimateMinutes > 0 ? UInt64(estimateMinutes * 60) : nil,
            body: routine?.template.body
        )
        let end = hasEnd ? Timestamp(endsAt.timeIntervalSince1970 * 1000) : nil
        Task {
            if isCreating {
                await commit(RoutineDraftIn(
                    template: template,
                    rrule: rule,
                    timezone: timeZone,
                    // The anchor is now: a routine created today starts today,
                    // and the rule decides which days it fires on.
                    startsAt: Timestamp(Date().timeIntervalSince1970 * 1000),
                    endsAt: end,
                    schedulingConstraints: [],
                    catchupPolicy: catchup
                ), nil)
            } else {
                var edit = RoutineEdit()
                edit.template = template
                edit.rrule = rule
                edit.timezone = timeZone
                edit.catchupPolicy = catchup
                if let end {
                    edit.setEndsAt = end
                } else {
                    edit.clearEndsAt = true
                }
                await commit(nil, edit)
            }
            dismiss()
        }
    }
}

extension NameBook {
    /// Streams as `(id, name)` pairs, by name — what a picker needs.
    var streamsByName: [(EntityRef, String)] {
        streams.map { ($0.key, $0.value) }.sorted { $0.1.localizedStandardCompare($1.1) == .orderedAscending }
    }
}
