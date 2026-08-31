import SwiftUI

/// Confirm a block drawn on the grid.
///
/// The times come from the drag and are shown, not re-derived: a sheet that
/// seeded its pickers with `Date()` would move a 14:00 block to now the moment
/// someone opened it to type a title. That exact bug shipped twice in this app
/// before self-audit caught it, so the seeding is the first thing to read here.
struct BlockDraftSheetView: View {
    let model: CalendarModel
    let draft: BlockDraftSheet

    @Environment(\.dismiss) private var dismiss
    @State private var title = ""
    @State private var kind: BlockTimeKind = .zoned
    @State private var startsAt: Date
    @State private var endsAt: Date

    init(model: CalendarModel, draft: BlockDraftSheet) {
        self.model = model
        self.draft = draft
        _startsAt = State(initialValue: Date(timeIntervalSince1970: Double(draft.fromMs) / 1000))
        _endsAt = State(initialValue: Date(timeIntervalSince1970: Double(draft.toMs) / 1000))
    }

    var body: some View {
        Form {
            TextField("Title", text: $title)
                .accessibilityIdentifier("block-title")
            DatePicker("Starts", selection: $startsAt)
            DatePicker("Ends", selection: $endsAt)
            TimeKindPicker(kind: $kind)
        }
        .formStyle(.grouped)
        .frame(width: 420)
        .toolbar {
            ToolbarItem(placement: .cancellationAction) {
                Button("Cancel") { dismiss() }
            }
            ToolbarItem(placement: .confirmationAction) {
                Button("Add block") {
                    Task {
                        await model.createBlock(
                            fromMs: ms(startsAt),
                            toMs: ms(endsAt),
                            title: title,
                            kind: kind
                        )
                        dismiss()
                    }
                }
                .disabled(endsAt <= startsAt)
                .keyboardShortcut(.defaultAction)
            }
        }
        .padding(.bottom, 8)
    }

    private func ms(_ date: Date) -> Int64 { Int64(date.timeIntervalSince1970 * 1000) }
}

/// Edit one block.
///
/// Everything on screen is seeded from `row`, and every field is compared
/// against what it was seeded with before it goes into the `BlockEdit`. The
/// split-optional edit shape means an untouched field must be left `nil`, not
/// resubmitted: a form that posts every field it renders will happily rewrite
/// a value it never showed the user — which is how a stream's review cadence
/// was silently reset by a rename in this app's own history.
struct BlockEditorView: View {
    let model: CalendarModel
    let row: BlockGridRow

    @Environment(\.dismiss) private var dismiss

    @State private var title: String
    @State private var startsAt: Date
    @State private var endsAt: Date
    @State private var trackTask: Bool
    @State private var kind: BlockTimeKind

    /// What the fields were seeded with, so an edit can carry only what moved.
    private let original: Seeded

    /// The values the form opened with.
    ///
    /// A named type, not a tuple: this is the record every field is compared
    /// against before it is submitted, and it is the thing that stops the
    /// editor rewriting values it never showed.
    private struct Seeded {
        let title: String
        let startMs: Int64
        let endMs: Int64
        let track: Bool
    }

    init(model: CalendarModel, row: BlockGridRow) {
        self.model = model
        self.row = row
        // The **stored** title, not the resolved one. `BlockGridRow.title` is
        // what the grid renders and may be a bound task's live title; writing
        // that back would turn a tracking block into one with a hard-coded
        // copy of a name it was deliberately following.
        let stored = row.block.title ?? ""
        let startMs = timeValueMs(value: row.block.startsAt, tz: TimeZone.current.identifier)
        let endMs = timeValueMs(value: row.block.endsAt, tz: TimeZone.current.identifier)
        original = Seeded(
            title: stored,
            startMs: startMs,
            endMs: endMs,
            track: row.block.titleTrackTask
        )
        _title = State(initialValue: stored)
        _startsAt = State(initialValue: Date(timeIntervalSince1970: Double(startMs) / 1000))
        _endsAt = State(initialValue: Date(timeIntervalSince1970: Double(endMs) / 1000))
        _trackTask = State(initialValue: row.block.titleTrackTask)
        _kind = State(initialValue: BlockTimeKind(row.block.startsAt))
    }

    var body: some View {
        Form {
            Section {
                TextField("Title", text: $title)
                    .accessibilityIdentifier("block-title")
                Toggle("Follow the bound task's title", isOn: $trackTask)
                    .disabled(row.block.tasks.count != 1)
                    .help(
                        row.block.tasks.count == 1
                            ? "Renaming the task renames this block."
                            : "Only a block bound to exactly one task can follow a title."
                    )
            }
            Section {
                DatePicker("Starts", selection: $startsAt)
                DatePicker("Ends", selection: $endsAt)
                TimeKindPicker(kind: $kind)
            }
            if !row.taskTitles.isEmpty {
                Section("Tasks") {
                    ForEach(Array(zip(row.block.tasks, row.taskTitles)), id: \.0) { task, name in
                        HStack {
                            Text(name)
                            Spacer()
                            Button("Unbind", systemImage: "minus.circle") {
                                Task { await model.unbind(task: task, from: row.block.id) }
                            }
                            .labelStyle(.iconOnly)
                            .buttonStyle(.plain)
                        }
                    }
                }
            }
        }
        .formStyle(.grouped)
        .frame(width: 460)
        .toolbar {
            ToolbarItem(placement: .cancellationAction) {
                Button("Cancel") { dismiss() }
            }
            ToolbarItem(placement: .destructiveAction) {
                Button("Delete", role: .destructive) {
                    Task {
                        await model.delete(row.block.id)
                        dismiss()
                    }
                }
            }
            ToolbarItem(placement: .confirmationAction) {
                Button("Save") {
                    Task {
                        await model.apply(edit, to: row.block.id)
                        dismiss()
                    }
                }
                .disabled(endsAt <= startsAt)
                .keyboardShortcut(.defaultAction)
            }
        }
        .padding(.bottom, 8)
    }

    /// Only what changed.
    private var edit: BlockEdit {
        var edit = BlockEdit()
        let trimmed = title.trimmed
        if trimmed != original.title {
            if trimmed.isEmpty {
                edit.clearTitle = true
            } else {
                edit.setTitle = trimmed
            }
        }
        if trackTask != original.track {
            edit.titleTrackTask = trackTask
        }
        // The times move when the *instant* moves or when the user changed what
        // kind of commitment this is — re-expressing an unchanged 09:00 as a
        // fresh zoned value would be a write with no edit behind it, and on a
        // floating block it would silently pin it to this device's zone.
        let kindChanged = kind != BlockTimeKind(row.block.startsAt)
        if ms(startsAt) != original.startMs || kindChanged {
            edit.startsAt = model.timeValue(ms: ms(startsAt), kind: kind)
        }
        if ms(endsAt) != original.endMs || kindChanged {
            edit.endsAt = model.timeValue(ms: ms(endsAt), kind: kind)
        }
        return edit
    }

    private func ms(_ date: Date) -> Int64 { Int64(date.timeIntervalSince1970 * 1000) }
}

/// The "what kind of commitment is this" picker.
///
/// On screen because the distinction is real and invisible otherwise:
/// `docs/02-domain/time-blocks.md` opens by saying a 09:00 block and a block
/// at a fixed instant are different commitments, and that flying to another
/// timezone must move one and not the other.
struct TimeKindPicker: View {
    @Binding var kind: BlockTimeKind

    var body: some View {
        Picker("Time", selection: $kind) {
            ForEach(BlockTimeKind.allCases) { Text($0.title).tag($0) }
        }
        Text(kind.explanation)
            .font(.caption)
            .foregroundStyle(.secondary)
    }
}

extension BlockTimeKind {
    /// Which kind a stored value already is.
    ///
    /// All-day has no picker entry — a block is a range within a day, and the
    /// core has never produced an all-day one — so it reads as the local-time
    /// case, which is what saving it would then make it.
    init(_ value: TimeValue) {
        switch value {
        case .instant: self = .instant
        case .zoned: self = .zoned
        case .floating: self = .floating
        case .allDay: self = .zoned
        }
    }
}

/// The Resolve menu's **Adjust times** action: both blocks, side by side.
///
/// A sheet showing one of them would be the ordinary editor; the point of this
/// action is that the user is deciding between two overlapping commitments and
/// needs to see both while doing it.
struct AdjustBlocksView: View {
    let model: CalendarModel
    let conflict: BlockConflict

    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("These two overlap")
                .font(.headline)
            Text(shared)
                .font(.callout)
                .foregroundStyle(.secondary)
            HStack(alignment: .top, spacing: 16) {
                ForEach([conflict.a, conflict.b], id: \.self) { id in
                    if let row = model.row(id) {
                        AdjustableBlock(model: model, row: row)
                    }
                }
            }
            HStack {
                Spacer()
                Button("Done") { dismiss() }
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(16)
        .frame(width: 620)
    }

    private var shared: String {
        let formatter = DateFormatter()
        formatter.timeZone = TimeZone(identifier: model.timeZone)
        formatter.dateFormat = "HH:mm"
        let from = Date(timeIntervalSince1970: Double(conflict.fromMs) / 1000)
        let to = Date(timeIntervalSince1970: Double(conflict.toMs) / 1000)
        return "Shared: \(formatter.string(from: from))–\(formatter.string(from: to))"
    }
}

/// One half of the side-by-side adjuster.
private struct AdjustableBlock: View {
    let model: CalendarModel
    let row: BlockGridRow

    @State private var startsAt: Date
    @State private var endsAt: Date

    init(model: CalendarModel, row: BlockGridRow) {
        self.model = model
        self.row = row
        let tz = TimeZone.current.identifier
        _startsAt = State(
            initialValue: Date(
                timeIntervalSince1970: Double(timeValueMs(value: row.block.startsAt, tz: tz)) / 1000
            )
        )
        _endsAt = State(
            initialValue: Date(
                timeIntervalSince1970: Double(timeValueMs(value: row.block.endsAt, tz: tz)) / 1000
            )
        )
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(row.title ?? "Untitled block").font(.subheadline.weight(.semibold))
            DatePicker("Starts", selection: $startsAt)
            DatePicker("Ends", selection: $endsAt)
            Button("Apply") {
                Task {
                    await model.moveBlock(
                        row,
                        toStartMs: Int64(startsAt.timeIntervalSince1970 * 1000),
                        toEndMs: Int64(endsAt.timeIntervalSince1970 * 1000)
                    )
                }
            }
            .disabled(endsAt <= startsAt)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

// The generated bindings are compiled into this module, so these conformances
// are ours to add and need no `@retroactive`.
extension BlockGridRow: Identifiable {
    public var id: EntityRef { block.id }
}

extension BlockConflict: Identifiable {
    public var id: String { "\(a)|\(b)" }
}
