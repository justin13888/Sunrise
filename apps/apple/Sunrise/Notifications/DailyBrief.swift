import Foundation

/// One section of a daily brief: a heading, a count, and rows.
///
/// The two briefs are different reports with the same shape on screen, so the
/// shape is named once. Nothing here decides *what* goes in a section — the
/// core did that — only that an empty one still says so rather than vanishing,
/// because "nothing to triage" is the answer someone opened the view for.
struct BriefSection: Identifiable, Equatable {
    let id: String
    let title: String
    /// Shown under the heading when the section is empty.
    let emptyMessage: String
    let tasks: [TaskItem]

    var count: Int { tasks.count }
}

/// The morning summary — `Query::MorningSummary`.
///
/// `docs/08-features/notifications.md`: *"what got done since the previous
/// calendar date, and what still needs a decision today"*, and *"the client's
/// job is to render them, not to recompute them"*. So this runs one query and
/// splits one list, using the boundary the report itself carries.
@MainActor
@Observable
final class MorningSummaryModel {
    private(set) var report: MorningReport?
    private(set) var names = NameBook()
    private(set) var nowMs: UInt64 = 0
    private(set) var timeZone: String = TimeZone.current.identifier
    private(set) var errorMessage: String?

    /// For the editor sheet a row opens, as ``TaskListModel`` exposes it.
    let bridge: CoreBridge

    init(bridge: CoreBridge) {
        self.bridge = bridge
    }

    func refresh() async {
        timeZone = TimeZone.current.identifier
        nowMs = await bridge.nowMs()
        names = await NameBook.load(from: bridge)
        do {
            if case let .morningSummary(report) = try await bridge.query(
                .morningSummary(nowMs: nowMs)
            ) {
                self.report = report
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

    /// The report, in the order the morning reads.
    ///
    /// Yesterday first — the day that closed — then the two things that need a
    /// decision. The split between yesterday and already-today is the reason
    /// `MorningReport` carries `today_start` at all: the seam's comment says
    /// it is there "so a client can split `completed` … without a second
    /// query", and this is that client.
    var sections: [BriefSection] {
        guard let report else { return [] }
        let boundary = report.todayStart
        let zone = timeZone
        let finishedToday = { (task: TaskItem) -> Bool in
            guard let completed = task.completedAt else { return false }
            return timeValueMs(value: completed, tz: zone) >= boundary
        }
        let today = report.completed.filter(finishedToday)
        let yesterday = report.completed.filter { !finishedToday($0) }
        return [
            BriefSection(
                id: "yesterday",
                title: "Finished yesterday",
                emptyMessage: "Nothing was completed yesterday.",
                tasks: yesterday
            ),
            BriefSection(
                id: "today",
                title: "Already done today",
                emptyMessage: "The day is still ahead of you.",
                tasks: today
            ),
            BriefSection(
                id: "triage",
                title: "To triage",
                emptyMessage: "Your Inbox is empty.",
                tasks: report.toTriage
            ),
            BriefSection(
                id: "due",
                title: "Landing today",
                emptyMessage: "Nothing is scheduled or due today.",
                tasks: report.dueToday
            )
        ]
    }

    /// The one line the header says.
    var headline: String {
        guard let report else { return "Reading your morning…" }
        let done = report.completed.count
        let ahead = report.dueToday.count
        let triage = report.toTriage.count
        var parts = ["\(done) completed since yesterday"]
        if ahead > 0 { parts.append("\(ahead) landing today") }
        if triage > 0 { parts.append("\(triage) to triage") }
        return parts.joined(separator: " · ")
    }

    func complete(_ task: TaskItem) async {
        await run(.completeTask(id: task.id))
    }

    func snooze(_ task: TaskItem, _ span: SnoozeSpan) async {
        await run(.deferTask(id: task.id, toMs: await DailyBrief.snoozeTarget(span, bridge: bridge)))
    }

    func apply(_ edit: TaskEdit, to task: TaskItem) async {
        await run(.updateTask(id: task.id, edit: edit))
    }

    func delete(_ task: TaskItem) async {
        await run(.deleteTask(id: task.id))
    }

    private func run(_ command: CoreCommand) async {
        do {
            _ = try await bridge.submit(command)
            await refresh()
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}

/// The end-of-day plan — `Query::EndOfDayPlan`.
///
/// *"Today's still-open tasks (overdue included), the seven civil days ahead,
/// and the unscheduled backlog to plan from."* One query, three lists, and the
/// evening's actual question — what moves to tomorrow — answered by the same
/// civil snooze the notification buttons use.
@MainActor
@Observable
final class EndOfDayPlanModel {
    private(set) var plan: EveningReport?
    private(set) var names = NameBook()
    private(set) var nowMs: UInt64 = 0
    private(set) var timeZone: String = TimeZone.current.identifier
    private(set) var errorMessage: String?

    let bridge: CoreBridge

    init(bridge: CoreBridge) {
        self.bridge = bridge
    }

    func refresh() async {
        timeZone = TimeZone.current.identifier
        nowMs = await bridge.nowMs()
        names = await NameBook.load(from: bridge)
        do {
            if case let .endOfDayPlan(plan) = try await bridge.query(
                .endOfDayPlan(nowMs: nowMs)
            ) {
                self.plan = plan
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

    var sections: [BriefSection] {
        guard let plan else { return [] }
        return [
            BriefSection(
                id: "open",
                title: "Still open today",
                emptyMessage: "Today is clear. Nothing was left behind.",
                tasks: plan.stillOpen
            ),
            BriefSection(
                id: "week",
                title: "The week ahead",
                emptyMessage: "Nothing is scheduled in the next seven days.",
                tasks: plan.weekAhead
            ),
            BriefSection(
                id: "backlog",
                title: "Unscheduled",
                emptyMessage: "Everything open has a date on it.",
                tasks: plan.unscheduled
            )
        ]
    }

    var headline: String {
        guard let plan else { return "Reading your evening…" }
        var parts = ["\(plan.stillOpen.count) still open today"]
        if !plan.weekAhead.isEmpty { parts.append("\(plan.weekAhead.count) this week") }
        if !plan.unscheduled.isEmpty { parts.append("\(plan.unscheduled.count) unscheduled") }
        return parts.joined(separator: " · ")
    }

    func complete(_ task: TaskItem) async {
        await run(.completeTask(id: task.id))
    }

    /// Move a task out by a civil span. The evening's whole verb.
    func snooze(_ task: TaskItem, _ span: SnoozeSpan) async {
        await run(.deferTask(id: task.id, toMs: await DailyBrief.snoozeTarget(span, bridge: bridge)))
    }

    /// Push everything still open today to tomorrow, in one pass.
    ///
    /// The one bulk action the evening earns: deciding task by task that a day
    /// did not happen is the friction that makes people stop planning at all.
    func moveEverythingToTomorrow() async {
        guard let open = plan?.stillOpen, !open.isEmpty else { return }
        let target = await DailyBrief.snoozeTarget(.tomorrow, bridge: bridge)
        do {
            for task in open {
                _ = try await bridge.submit(.deferTask(id: task.id, toMs: target))
            }
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
        await refresh()
    }

    func apply(_ edit: TaskEdit, to task: TaskItem) async {
        await run(.updateTask(id: task.id, edit: edit))
    }

    func delete(_ task: TaskItem) async {
        await run(.deleteTask(id: task.id))
    }

    private func run(_ command: CoreCommand) async {
        do {
            _ = try await bridge.submit(command)
            await refresh()
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}

extension SnoozeSpan {
    /// The spans a "not now" menu offers, in order.
    ///
    /// One list, so a snooze offered in the evening view and one offered on a
    /// notification cannot come to mean different things.
    static let offered: [SnoozeSpan] = [.oneHour, .tomorrow, .nextWeek]

    /// What the button says.
    var buttonTitle: String {
        switch self {
        case .oneHour: "Defer 1 Hour"
        case .tomorrow: "Snooze Until Tomorrow"
        case .nextWeek: "Snooze Until Next Week"
        }
    }
}

/// What the two briefs share.
enum DailyBrief {
    /// When a snooze lands, from the seam.
    ///
    /// The same call the notification buttons make, so "Tomorrow" means the
    /// same instant whether it was pressed on a banner or in the evening view
    /// — including on the two nights a year when it is not 24 hours away.
    static func snoozeTarget(_ span: SnoozeSpan, bridge: CoreBridge) async -> UInt64 {
        let now = await bridge.nowMs()
        let target = snoozeTargetMs(fromMs: now, span: span, tz: TimeZone.current.identifier)
        return target <= 0 ? now : UInt64(target)
    }
}
