import Foundation

/// The daily snapshot the menu bar shows.
///
/// Counts, not lists: the menu bar is a glance, and its job is to say whether
/// opening the app is worth it. The numbers come from the same `Query::Today`
/// and `Query::Inbox` the main window uses, so a disagreement between the two
/// is impossible rather than merely unlikely.
struct DailySnapshot: Equatable {
    var scheduled = 0
    var due = 0
    var overdue = 0
    var inbox = 0
    var doneToday = 0

    /// What the menu bar's own label says.
    ///
    /// Deliberately one number. A menu bar item that renders four counts is
    /// four things to read at a glance, which is none.
    var badge: String {
        let outstanding = scheduled + due + overdue
        return outstanding == 0 ? "" : "\(outstanding)"
    }

    var isEmpty: Bool {
        scheduled + due + overdue + inbox == 0
    }
}

/// The `MenuBarExtra`'s state: the daily snapshot and sync health.
///
/// `docs/07-clients/desktop.md` specifies the refresh policy exactly — on
/// launch, every 60 s **while visible**, and immediately on a relevant change,
/// debounced 500 ms. All three are here, and the third is the one that matters:
/// a menu bar item is the surface a user leaves open for hours, so it is the
/// one most likely to be showing yesterday's numbers.
@MainActor
@Observable
final class MenuBarModel {
    private(set) var snapshot = DailySnapshot()
    private(set) var sync = SyncPresentation.unknown
    private(set) var lastRefreshMs: UInt64 = 0
    private(set) var errorMessage: String?

    /// Whether the menu is open. The 60 s poll runs only while it is: a closed
    /// menu is not being read, and waking the vault every minute to redraw
    /// something nobody can see is the sort of thing that shows up in a battery
    /// report.
    var isVisible = false {
        didSet {
            guard isVisible, isVisible != oldValue else { return }
            Task { await refresh() }
        }
    }

    /// Per the spec.
    static let pollInterval: Duration = .seconds(60)
    static let changeDebounce: Duration = .milliseconds(500)

    private let bridge: CoreBridge

    init(bridge: CoreBridge) {
        self.bridge = bridge
    }

    func refresh() async {
        let now = await bridge.nowMs()
        let timeZone = TimeZone.current.identifier
        do {
            var built = DailySnapshot()
            if case let .tasks(rows) = try await bridge.query(
                .today(nowMs: now, contexts: [])
            ) {
                for task in rows where task.state != .done && task.state != .cancelled {
                    // The section is the domain's decision — the overdue
                    // boundary is `due_at < start_of_today_local`, not
                    // `due_at < now`, and a menu bar counting it itself would
                    // disagree with the list behind it every evening.
                    switch todaySection(
                        scheduledAt: task.scheduledAt,
                        dueAt: task.dueAt,
                        nowMs: now,
                        tz: timeZone
                    ) {
                    case .scheduled: built.scheduled += 1
                    case .due: built.due += 1
                    case .overdue: built.overdue += 1
                    }
                }
            }
            // Completions come from `MorningSummary`, not from `Query::Today`.
            // Today is a *plan*: it lists what is still to happen, so a task
            // completed this morning drops straight out of it and counting
            // done rows there always answers zero. The morning report is the
            // read the domain built for "what got done", and it is also what
            // the morning view behind the notification shows — so the menu bar
            // and that screen cannot disagree.
            if case let .morningSummary(report) = try await bridge.query(
                .morningSummary(nowMs: now)
            ) {
                let dayStart = report.todayStart
                built.doneToday = report.completed.count { task in
                    task.completedAt.map {
                        timeValueMs(value: $0, tz: timeZone) >= dayStart
                    } ?? false
                }
            }
            if case let .tasks(rows) = try await bridge.query(.inbox) {
                built.inbox = rows.count { $0.state != .done && $0.state != .cancelled }
            }
            if case let .syncStatus(status) = try await bridge.query(.syncStatus) {
                sync = SyncPresentation(status)
            }
            snapshot = built
            lastRefreshMs = now
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// Refresh every 60 s while the menu is open.
    func poll(every interval: Duration = MenuBarModel.pollInterval) async {
        while !_Concurrency.Task.isCancelled {
            try? await _Concurrency.Task.sleep(for: interval)
            guard isVisible else { continue }
            await refresh()
        }
    }

    /// Refresh on a change, debounced.
    ///
    /// The change feed is already coalesced on 50 ms; this adds the spec's
    /// 500 ms on top, because the menu bar is showing counts rather than rows
    /// and redrawing "3" as "3" ten times a second is work with no reader.
    ///
    /// **A lagged batch refreshes exactly like a complete one.** The snapshot
    /// is derived from a full read either way, so there is nothing to patch —
    /// and a menu bar that ignored `isComplete` would be the surface left open
    /// longest showing numbers from before the last sync burst.
    func follow(debounce: Duration = MenuBarModel.changeDebounce) async {
        for await batch in await bridge.changes() {
            guard !batch.isClosed else { return }
            try? await _Concurrency.Task.sleep(for: debounce)
            await refresh()
        }
    }
}

extension Array {
    /// How many elements satisfy `predicate`.
    ///
    /// `filter { }.count` allocates an array to throw it away, and the menu bar
    /// does it four times per refresh, once a minute, for as long as the app is
    /// open.
    func count(where predicate: (Element) -> Bool) -> Int {
        reduce(0) { predicate($1) ? $0 + 1 : $0 }
    }
}
