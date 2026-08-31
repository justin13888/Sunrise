import Foundation

/// Which review is on screen.
enum ReviewTab: String, CaseIterable, Identifiable {
    case weekly
    case daily
    case trends
    case history

    var id: Self { self }

    var title: String {
        switch self {
        case .weekly: "Weekly"
        case .daily: "Daily"
        case .trends: "Trends"
        case .history: "History"
        }
    }
}

/// Review: the weekly walk-through, the 60-second daily glance, the trends,
/// the saved snapshots, and the export.
///
/// Every one of those is a single query. The core assembles each report in one
/// read — that is the point of `Query::WeeklyReview` returning every step's
/// list at once — so this model runs one query per tab and never stitches.
@MainActor
@Observable
final class ReviewModel {
    var tab: ReviewTab = .weekly {
        didSet { Task { await refresh() } }
    }

    /// How many weeks of history the trends cover.
    var weeks: UInt32 = 8 {
        didSet { Task { await refresh() } }
    }

    private(set) var weekly: WeeklyReviewReport?
    private(set) var daily: DailyReviewReport?
    private(set) var trends: TrendReport?
    private(set) var snapshots: [Snapshot] = []
    private(set) var names = NameBook()
    private(set) var nowMs: UInt64 = 0
    private(set) var errorMessage: String?
    /// Set once a snapshot is saved, so the screen can say the week is on the
    /// record. Cleared on the next refresh.
    private(set) var savedNote: String?

    private let bridge: CoreBridge

    init(bridge: CoreBridge) {
        self.bridge = bridge
    }

    /// The 60-second glance looks back a day.
    private static let dayMs: UInt64 = 24 * 60 * 60 * 1000

    func refresh() async {
        nowMs = await bridge.nowMs()
        names = await NameBook.load(from: bridge)
        do {
            switch tab {
            case .weekly:
                if case let .weeklyReview(report) = try await bridge.query(
                    .weeklyReview(weekStartMs: nil, nowMs: nowMs)
                ) {
                    weekly = report
                }
            case .daily:
                if case let .dailyReview(report) = try await bridge.query(
                    .dailyReview(sinceMs: nowMs &- Self.dayMs, nowMs: nowMs)
                ) {
                    daily = report
                }
            case .trends:
                if case let .trends(report) = try await bridge.query(
                    .streamTrends(weeks: weeks, nowMs: nowMs)
                ) {
                    trends = report
                }
            case .history:
                if case let .reviewSnapshots(rows) = try await bridge.query(
                    .reviewHistory(limit: 50)
                ) {
                    snapshots = rows
                }
            }
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func follow() async {
        for await batch in await bridge.changes() {
            guard !batch.isClosed else { return }
            await refresh()
        }
    }

    /// Save the week that is on screen.
    ///
    /// A snapshot is the one fact about a review that cannot be re-derived
    /// from the op log — that someone *did* it, and what they saw when they
    /// did. So it carries the counts the screen showed rather than being
    /// recomputed at read time.
    func saveSnapshot(note: String?) async {
        guard let weekly else { return }
        let draft = SnapshotDraftIn(
            windowStartMs: weekly.window.startMs,
            windowEndMs: weekly.window.endMs,
            totals: weekly.totals,
            streams: weekly.streams.map {
                SnapshotStream(
                    stream: $0.stream,
                    name: $0.name,
                    completed: UInt32($0.completed.count),
                    deferred: UInt32($0.deferred.count),
                    created: UInt32($0.createdUntouched.count)
                )
            },
            streaks: weekly.streaks,
            note: note?.nilIfBlank
        )
        do {
            _ = try await bridge.submit(.saveReviewSnapshot(draft: draft))
            savedNote = "This week is on the record."
            await refresh()
            savedNote = "This week is on the record."
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func dismissSavedNote() { savedNote = nil }

    /// Render one dataset. The document is the core's — CSV quoting and JSON
    /// shape included — so an export from here and one from `sunrise-cli` are
    /// the same bytes.
    func export(_ dataset: ExportDataset, as format: ExportFormat) async -> String? {
        let now = await bridge.nowMs()
        guard case let .export(body)? = try? await bridge.query(
            .exportStats(dataset: dataset, format: format, weeks: weeks, nowMs: now)
        ) else {
            errorMessage = "That export could not be produced."
            return nil
        }
        return body
    }

    /// A filename that says what is in it and when it was taken.
    func exportFilename(_ dataset: ExportDataset, as format: ExportFormat) -> String {
        let stamp = ISO8601DateFormatter()
        stamp.formatOptions = [.withFullDate]
        let day = stamp.string(from: Date(timeIntervalSince1970: Double(nowMs) / 1000))
        let name = exportDatasetName(dataset: dataset)
        return "sunrise-\(name)-\(day).\(exportFormatName(format: format))"
    }
}

extension ExportDataset {
    /// What the menu item says. Title case is this platform's convention; the
    /// name itself comes from the seam, so it matches `sunrise-cli export`.
    var title: String { exportDatasetName(dataset: self).capitalized }

    static let all: [ExportDataset] = [.trends, .activity, .focus, .streaks]
}

extension ExportFormat {
    static let all: [ExportFormat] = [.csv, .json]
}
