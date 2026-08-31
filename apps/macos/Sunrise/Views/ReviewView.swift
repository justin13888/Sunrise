import SwiftUI
import UniformTypeIdentifiers

/// Review: the weekly walk-through, the daily glance, the trends and the
/// saved snapshots — plus the export.
struct ReviewView: View {
    @Bindable var model: ReviewModel

    @State private var savingSnapshot = false
    @State private var snapshotNote = ""
    @State private var exporting: ExportDocument?

    var body: some View {
        VStack(spacing: 0) {
            Picker("Review", selection: $model.tab) {
                ForEach(ReviewTab.allCases) { Text($0.title).tag($0) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .padding(.horizontal, 12)
            .padding(.vertical, 8)

            if let note = model.savedNote {
                NoteBanner(text: note) { model.dismissSavedNote() }
            }
            if let message = model.errorMessage {
                Label(message, systemImage: "exclamationmark.triangle")
                    .font(.callout)
                    .foregroundStyle(.orange)
                    .padding(12)
            }

            Divider()
            body(for: model.tab)
        }
        .navigationTitle("Review")
        .toolbar {
            ToolbarItem {
                Menu("Export", systemImage: "square.and.arrow.up") {
                    ForEach(ExportDataset.all, id: \.self) { dataset in
                        Menu(dataset.title) {
                            ForEach(ExportFormat.all, id: \.self) { format in
                                Button(exportFormatName(format: format).uppercased()) {
                                    Task { await beginExport(dataset, format) }
                                }
                            }
                        }
                    }
                }
            }
            ToolbarItem {
                Button("Save snapshot", systemImage: "checkmark.seal") {
                    savingSnapshot = true
                }
                .disabled(model.weekly == nil)
            }
        }
        .task { await model.refresh() }
        .task { await model.follow() }
        .sheet(isPresented: $savingSnapshot) {
            SnapshotNoteSheet(note: $snapshotNote) {
                await model.saveSnapshot(note: snapshotNote)
                snapshotNote = ""
            }
        }
        .fileExporter(
            isPresented: Binding(
                get: { exporting != nil },
                set: { if !$0 { exporting = nil } }
            ),
            document: exporting,
            // The bytes are the core's; only where they land is decided here.
            contentType: exporting?.contentType ?? .plainText,
            defaultFilename: exporting?.filename
        ) { _ in exporting = nil }
    }

    @ViewBuilder
    private func body(for tab: ReviewTab) -> some View {
        switch tab {
        case .weekly:
            if let weekly = model.weekly {
                WeeklyReviewBody(report: weekly, names: model.names)
            } else {
                ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        case .daily:
            if let daily = model.daily {
                DailyReviewBody(report: daily)
            } else {
                ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        case .trends:
            TrendsBody(trends: model.trends, names: model.names, weeks: $model.weeks)
        case .history:
            SnapshotHistoryBody(snapshots: model.snapshots)
        }
    }

    private func beginExport(_ dataset: ExportDataset, _ format: ExportFormat) async {
        guard let body = await model.export(dataset, as: format) else { return }
        exporting = ExportDocument(
            text: body,
            filename: model.exportFilename(dataset, as: format),
            contentType: format == .csv ? .commaSeparatedText : .json
        )
    }
}

/// The weekly walk-through, one section per step.
struct WeeklyReviewBody: View {
    let report: WeeklyReviewReport
    let names: NameBook

    var body: some View {
        List {
            Section("This week") {
                counts(report.totals)
            }

            if !report.inbox.isEmpty {
                Section("Inbox to triage (\(report.inbox.count))") {
                    ForEach(report.inbox, id: \.id) { Text($0.title) }
                }
            }

            if !report.slipped.isEmpty {
                Section("Slipped past its date (\(report.slipped.count))") {
                    ForEach(report.slipped, id: \.id) { Text($0.title) }
                }
            }

            ForEach(report.streams, id: \.stream) { stream in
                Section(stream.name) {
                    LabeledContent("Completed", value: "\(stream.completed.count)")
                    LabeledContent("Deferred", value: "\(stream.deferred.count)")
                    LabeledContent("Created and untouched", value: "\(stream.createdUntouched.count)")
                    if let focus = stream.focus {
                        LabeledContent(
                            "Focused",
                            value: shortDuration(secs: focus.focusedMs / 1000)
                        )
                    }
                }
            }

            if !report.driftingRoutines.isEmpty {
                Section("Routines drifting") {
                    ForEach(report.driftingRoutines, id: \.routine) { drift in
                        LabeledContent(drift.title) {
                            Text("\(drift.missed) of \(drift.expected) missed")
                                .foregroundStyle(.orange)
                        }
                    }
                }
            }

            if !report.streaks.isEmpty {
                Section("Streaks") {
                    ForEach(report.streaks, id: \.routine) { streak in
                        LabeledContent(streak.title, value: "\(streak.streak)")
                    }
                }
            }
        }
        .listStyle(.inset)
    }

    @ViewBuilder
    private func counts(_ totals: ReviewCounts) -> some View {
        LabeledContent("Completed", value: "\(totals.completed)")
        LabeledContent("Deferred", value: "\(totals.deferred)")
        LabeledContent("Dropped", value: "\(totals.dropped)")
        LabeledContent("Created", value: "\(totals.created)")
        LabeledContent("Reopened", value: "\(totals.reopened)")
    }
}

/// The 60-second glance.
struct DailyReviewBody: View {
    let report: DailyReviewReport

    var body: some View {
        List {
            section("Just captured", report.inbox)
            section("Today", report.today)
            section("Blocked", report.blocked)
        }
        .listStyle(.inset)
    }

    @ViewBuilder
    private func section(_ title: String, _ tasks: [TaskItem]) -> some View {
        Section("\(title) (\(tasks.count))") {
            if tasks.isEmpty {
                Text("Nothing").foregroundStyle(.secondary)
            } else {
                ForEach(tasks, id: \.id) { Text($0.title) }
            }
        }
    }
}

/// Weekly trends, as a small bar per week.
struct TrendsBody: View {
    let trends: TrendReport?
    let names: NameBook
    @Binding var weeks: UInt32

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text("Weeks").font(.caption).foregroundStyle(.secondary)
                Picker("Weeks", selection: $weeks) {
                    ForEach([4, 8, 12, 26] as [UInt32], id: \.self) { Text("\($0)").tag($0) }
                }
                .labelsHidden()
                .frame(width: 90)
                Spacer()
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 6)

            if let trends, !trends.overall.isEmpty {
                List {
                    Section("Whole vault") {
                        WeekBars(weeks: trends.overall)
                    }
                    ForEach(trends.perStream, id: \.stream) { row in
                        Section(names.stream(row.stream) ?? "Stream") {
                            WeekBars(weeks: row.weeks)
                        }
                    }
                }
                .listStyle(.inset)
            } else {
                ContentUnavailableView(
                    "No trend yet",
                    systemImage: "chart.line.uptrend.xyaxis",
                    description: Text("Complete something and it will show up here.")
                )
            }
        }
    }
}

/// One row of bars: completions per week, oldest on the left.
struct WeekBars: View {
    let weeks: [WeekCounts]

    var body: some View {
        HStack(alignment: .bottom, spacing: 3) {
            ForEach(Array(weeks.enumerated()), id: \.offset) { _, week in
                VStack(spacing: 2) {
                    Rectangle()
                        .fill(.tint)
                        .frame(width: 14, height: height(week.completed))
                    Text("\(week.completed)")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                }
                .accessibilityLabel("\(week.completed) completed")
            }
            Spacer()
        }
        .frame(height: 64, alignment: .bottom)
        .padding(.vertical, 4)
    }

    /// Scaled against the tallest week on screen, with a floor so a week with
    /// one completion is still visible.
    private func height(_ value: Int64) -> CGFloat {
        let peak = max(weeks.map(\.completed).max() ?? 1, 1)
        guard value > 0 else { return 1 }
        return max(4, CGFloat(value) / CGFloat(peak) * 48)
    }
}

/// Saved snapshots, newest first.
struct SnapshotHistoryBody: View {
    let snapshots: [Snapshot]

    var body: some View {
        if snapshots.isEmpty {
            ContentUnavailableView(
                "No reviews saved",
                systemImage: "clock.arrow.circlepath",
                description: Text("Saving a review records that you did it, and what you saw.")
            )
        } else {
            List(snapshots, id: \.id) { snapshot in
                VStack(alignment: .leading, spacing: 3) {
                    Text(window(snapshot)).font(.headline)
                    Text(
                        "\(snapshot.totals.completed) completed · "
                            + "\(snapshot.totals.deferred) deferred · "
                            + "\(snapshot.totals.created) created"
                    )
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    if let note = snapshot.note {
                        Text(note).font(.caption).italic()
                    }
                }
                .padding(.vertical, 2)
            }
            .listStyle(.inset)
        }
    }

    private func window(_ snapshot: Snapshot) -> String {
        let formatter = DateFormatter()
        formatter.dateStyle = .medium
        formatter.timeStyle = .none
        let start = Date(timeIntervalSince1970: Double(snapshot.windowStart) / 1000)
        let end = Date(timeIntervalSince1970: Double(snapshot.windowEnd) / 1000)
        return "\(formatter.string(from: start)) – \(formatter.string(from: end))"
    }
}

/// The optional note that goes on a snapshot.
struct SnapshotNoteSheet: View {
    @Binding var note: String
    let save: () async -> Void

    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Save this week's review").font(.headline)
            Text(
                "The counts on screen are recorded as they are. That you did the review "
                    + "is the one thing the op log cannot re-derive."
            )
            .font(.caption)
            .foregroundStyle(.secondary)
            TextField("Note (optional)", text: $note, axis: .vertical)
                .lineLimit(2...5)
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                Button("Save") {
                    Task {
                        await save()
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

/// A rendered export, on its way to a file.
///
/// The bytes are the core's — CSV quoting and JSON shape included — so an
/// export saved here and one written by `sunrise-cli export` are identical.
struct ExportDocument: FileDocument {
    static let readableContentTypes: [UTType] = [.commaSeparatedText, .json, .plainText]

    let text: String
    let filename: String
    let contentType: UTType

    init(text: String, filename: String, contentType: UTType) {
        self.text = text
        self.filename = filename
        self.contentType = contentType
    }

    init(configuration: ReadConfiguration) throws {
        // Exports are written, never opened. Reading one back would mean this
        // app could import its own statistics, which is not a feature.
        throw CocoaError(.fileReadUnsupportedScheme)
    }

    func fileWrapper(configuration: WriteConfiguration) throws -> FileWrapper {
        FileWrapper(regularFileWithContents: Data(text.utf8))
    }
}
