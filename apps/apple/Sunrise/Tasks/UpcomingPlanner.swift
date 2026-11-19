import Foundation

/// One section of Upcoming as the list draws it: a day, or one stream's
/// undated tasks.
///
/// Everything in it is the core's (`Query::Upcoming`): which days, which
/// tasks on each, in what order, and how many undated ones each stream has.
/// This only flattens the report into the rows a `List` draws, in the order
/// they arrived.
struct UpcomingSection: Equatable, Identifiable {
    enum Kind: Equatable {
        /// A civil day, with when it starts in the device's zone.
        case day(date: CivilDate, startMs: Int64)
        /// One stream's undated tasks; `noStream` for the "No stream" group.
        case noDate(stream: EntityRef, noStream: Bool)
    }

    let kind: Kind
    /// Blocks overlapping the day, by start. Always empty for a no-date group.
    let blocks: [BlockGridRow]
    let rows: [UpcomingRow]
    /// How many tasks the group holds beyond `rows`: the "N more".
    let more: Int

    var id: String {
        switch kind {
        case let .day(date, _): "day-\(date)"
        case let .noDate(stream, _): "nodate-\(stream)"
        }
    }

    /// Where a task dropped on this section goes.
    var target: UpcomingTarget {
        switch kind {
        case let .day(date, _): .day(date)
        case .noDate: .noDate
        }
    }

    /// The day this section is, or `nil` for a no-date group.
    var date: CivilDate? {
        if case let .day(date, _) = kind { return date }
        return nil
    }

    var isEmpty: Bool { rows.isEmpty && blocks.isEmpty }
}

/// One task row of Upcoming, with why it is on that day.
struct UpcomingRow: Equatable, Identifiable {
    enum Mark: Equatable {
        /// Planned for the day.
        case planned
        /// Its hard deadline falls that day.
        case due
        /// Its soft deadline falls that day.
        case target
        /// It has no date: a no-date group's row.
        case undated
    }

    let task: TaskItem
    let mark: Mark

    var id: EntityRef { task.id }

    /// The badge beside a row listed on a deadline's day: which deadline.
    var badge: String? {
        switch mark {
        case .due: L10n.Upcoming.due
        case .target: L10n.Upcoming.target
        case .planned, .undated: nil
        }
    }
}

/// Where a task dropped in Upcoming goes: onto a day, or into the no-date
/// lane, which takes its plan away.
enum UpcomingTarget: Hashable, Sendable {
    case day(CivilDate)
    case noDate
}

/// The report, as sections, and the arithmetic the keyboard needs over them.
enum UpcomingLayout {
    /// Every day, in order, then every no-date group, in the order the core
    /// sent them. A day with nothing on it is kept: it is somewhere a task
    /// can be dragged to.
    static func sections(of report: UpcomingReport) -> [UpcomingSection] {
        let days = report.days.map { day in
            UpcomingSection(
                kind: .day(date: day.date, startMs: day.start),
                blocks: day.blocks,
                rows: day.planned.map { UpcomingRow(task: $0, mark: .planned) }
                    + day.due.map { UpcomingRow(task: $0, mark: .due) }
                    + day.targets.map { UpcomingRow(task: $0, mark: .target) },
                more: 0
            )
        }
        let groups = report.noDate.map { group in
            UpcomingSection(
                kind: .noDate(stream: group.stream, noStream: group.noStream),
                blocks: [],
                rows: group.tasks.map { UpcomingRow(task: $0, mark: .undated) },
                more: max(0, Int(group.total) - group.tasks.count)
            )
        }
        return days + groups
    }

    /// The tasks in screen order, each once. A task planned on one day and
    /// due on another is drawn under both; the keyboard walks it where it is
    /// first drawn.
    static func tasks(in sections: [UpcomingSection]) -> [TaskItem] {
        var seen = Set<EntityRef>()
        return sections.flatMap(\.rows).compactMap { row in
            seen.insert(row.task.id).inserted ? row.task : nil
        }
    }

    /// The day `task` is drawn under as planned, or `nil` when no day in the
    /// span holds its plan. A row under its Due or Target day is not where it
    /// is planned, so it is not counted.
    static func plannedDay(of task: EntityRef, in sections: [UpcomingSection]) -> CivilDate? {
        sections.first { section in
            section.rows.contains { $0.id == task && $0.mark == .planned }
        }?.date
    }

    /// `date` moved by whole civil days. Computed on the Gregorian calendar
    /// in UTC, which has no DST, so the answer is the civil date and not an
    /// instant that a zone could shift onto the neighbouring day.
    static func adding(_ days: Int, to date: CivilDate) -> CivilDate {
        guard let start = utcDate(date),
              let moved = utc.date(byAdding: .day, value: days, to: start) else { return date }
        let parts = utc.dateComponents([.year, .month, .day], from: moved)
        return String(format: "%04d-%02d-%02d", parts.year ?? 0, parts.month ?? 0, parts.day ?? 0)
    }

    /// A day's heading: Today, Tomorrow, or the date written out.
    static func heading(for date: CivilDate, today: CivilDate) -> String {
        if date == today { return L10n.Upcoming.today }
        if date == adding(1, to: today) { return L10n.Upcoming.tomorrow }
        guard let day = utcDate(date) else { return date }
        let formatter = DateFormatter()
        formatter.timeZone = TimeZone(identifier: "UTC")
        formatter.setLocalizedDateFormatFromTemplate("EEEEdMMM")
        return formatter.string(from: day)
    }

    private static let utc: Calendar = {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(identifier: "UTC") ?? .current
        return calendar
    }()

    private static func utcDate(_ date: CivilDate) -> Date? {
        let parts = date.split(separator: "-").compactMap { Int($0) }
        guard parts.count == 3 else { return nil }
        return utc.date(from: DateComponents(year: parts[0], month: parts[1], day: parts[2], hour: 12))
    }
}

/// Rescheduling in Upcoming, planned by the core (`docs/08-features/planner.md`,
/// `docs/08-features/planning-views.md` §Upcoming).
///
/// A task dragged onto a day is a `BulkDefer` to that day; dragged into the
/// no-date lane, an `Unschedule` that keeps nothing. While the drag hovers a
/// section the plan for dropping there is previewed and its consequence put
/// into words; the drop commits exactly that diff, as one undo step. ⌥← and
/// ⌥→ move the selection a day the same way, and Return commits it. Nothing
/// here decides where anything lands: the core answers every preview.
///
/// The calendar's ``CalendarPlanner`` is the same contract over blocks. This
/// one is apart from it because its gestures carry tasks and a day, not a
/// block and a time.
@MainActor
@Observable
final class UpcomingPlanner {
    /// The plan on screen, or `nil` when nothing is previewing.
    private(set) var plan: PlanDiffItem?
    /// Where the plan on screen moves its tasks, for the section to light up.
    private(set) var target: UpcomingTarget?
    /// Set after a drop that was refused or whose plan changed underneath it.
    private(set) var note: String?
    private(set) var errorMessage: String?
    /// A drop's plan changed underneath it and the new plan waits for Move or
    /// Cancel, which a phone has no keys for.
    private(set) var awaitingDecision = false
    /// The tasks the drag in progress carries. A drag's payload cannot be
    /// read until it is dropped, so the row records what it is lending when
    /// the drag starts, and a hover previews those.
    private(set) var dragging: [EntityRef] = []

    /// The line naming what the plan on screen would do.
    var summary: String? { plan.map(PlanGhosts.summary(of:)) }

    /// Which tasks the plan on screen moves, and where.
    var planned: (tasks: [EntityRef], target: UpcomingTarget)? {
        planRequest.map { ($0.tasks, $0.target) }
    }

    private var planRequest: UpcomingRequest?
    private var pump = PlanPreviewPump<UpcomingRequest>()
    private var generation = 0
    /// A drop is being committed: the hover that ends with it must not
    /// cancel the plan it is committing.
    private var dropping = false
    /// The keyboard move in progress: whose, from which day, and how far.
    private(set) var nudge: UpcomingNudge?

    private let bridge: CoreBridge
    /// Re-read the list after a write. Set by the list's model.
    @ObservationIgnored var afterWrite: (@MainActor () async -> Void)?

    init(bridge: CoreBridge) {
        self.bridge = bridge
    }

    func dismissNote() { note = nil }

    func dismissError() { errorMessage = nil }

    /// The gesture for moving `tasks` to `target`.
    nonisolated static func intent(moving tasks: [EntityRef], to target: UpcomingTarget) -> PlanIntentItem {
        switch target {
        case let .day(date): .bulkDefer(tasks: tasks, to: date, place: false)
        case .noDate: .unschedule(items: tasks, keep: .nothing)
        }
    }

    // MARK: - A drag

    /// A row started a drag carrying `tasks`.
    func beginDrag(_ tasks: [EntityRef]) {
        dragging = tasks
    }

    /// The drag is over `target`, or over nothing when it is `nil`. Leaving
    /// the section whose plan is on screen drops that plan; leaving another
    /// is old news.
    func hover(_ target: UpcomingTarget?, leaving left: UpcomingTarget? = nil) async {
        guard let target else {
            guard !dropping, !awaitingDecision, left == nil || left == self.target else { return }
            end(cancelling: plan?.id)
            return
        }
        guard !dragging.isEmpty else { return }
        await preview(UpcomingRequest(tasks: dragging, target: target, generation: generation))
    }

    /// Mark the drop as begun, synchronously, before its commit is awaited:
    /// the drop target's "no longer targeted" may arrive first.
    func willDrop() {
        dropping = true
    }

    /// `tasks` were dropped on `target`: commit the plan for it.
    func drop(_ tasks: [EntityRef], on target: UpcomingTarget, label: String) async {
        dropping = true
        defer {
            dropping = false
            dragging = []
        }
        await commit(UpcomingRequest(tasks: tasks, target: target, generation: generation), label: label)
    }

    // MARK: - The keyboard

    /// ⌥← or ⌥→ on `tasks`, planned for `from`: move them a day and preview
    /// it. Undated tasks (`from` is `nil`) count from the day before `today`,
    /// so their first ⌥→ plans them for today. Moves add up until Return
    /// commits them (``commitPlan(label:)``) or Esc cancels (``cancel()``).
    func move(_ tasks: [EntityRef], days: Int, from: CivilDate?, today: CivilDate) async {
        guard !tasks.isEmpty else { return }
        var next = nudge.flatMap { $0.tasks == tasks ? $0 : nil }
            ?? UpcomingNudge(tasks: tasks, base: from ?? UpcomingLayout.adding(-1, to: today))
        next.days += days
        nudge = next
        let day = UpcomingLayout.adding(next.days, to: next.base)
        await preview(UpcomingRequest(tasks: tasks, target: .day(day), generation: generation))
    }

    /// Commit the plan on screen: Return after a keyboard move, or Move after
    /// a drop whose plan changed underneath it.
    func commitPlan(label: String) async {
        guard let planRequest else { return }
        await commit(planRequest, label: label)
    }

    /// Esc, or Cancel: forget the plan on screen and write nothing. A changed
    /// plan's note goes with it, as the answer it waited for.
    func cancel() {
        settleDecision()
        end(cancelling: plan?.id)
    }

    // MARK: - Previews and commits

    private func preview(_ request: UpcomingRequest) async {
        settleDecision()
        var next = pump.request(request)
        while let sent = next {
            let result: Result<PlanDiffItem, Error>
            do {
                result = try await .success(bridge.planPreview(sent.intent))
            } catch {
                result = .failure(error)
            }
            let (current, queued) = pump.finished()
            next = queued
            guard current, sent.generation == generation else { continue }
            switch result {
            case let .success(diff):
                plan = diff
                planRequest = sent
                target = sent.target
            case let .failure(error):
                errorMessage = error.localizedDescription
            }
        }
    }

    /// Commit the plan for `request`: the one on screen when it was solved
    /// for this very request, a fresh preview of it otherwise. A stale diff
    /// is previewed once more and committed only if it moves the same tasks
    /// to the same places; a changed one waits on screen for Move or Cancel.
    private func commit(_ request: UpcomingRequest, label: String, retry: Bool = true) async {
        settleDecision()
        let diff: PlanDiffItem
        if let plan, planRequest?.same(as: request) == true {
            diff = plan
        } else {
            do {
                diff = try await bridge.planPreview(request.intent)
            } catch {
                errorMessage = error.localizedDescription
                end()
                return
            }
        }
        guard !diff.moves.isEmpty || !diff.unscheduled.isEmpty else {
            end(cancelling: diff.id)
            return
        }
        guard diff.committable else {
            note = L10n.Upcoming.notePlanRefused
            end(cancelling: diff.id)
            return
        }
        do {
            _ = try await bridge.planCommit(diff.id, label: label)
            await afterWrite?()
            end()
        } catch BindingError.PlanStale(_), BindingError.PlanUnknown(_) {
            guard retry, let again = try? await bridge.planPreview(request.intent) else {
                await afterWrite?()
                end()
                return
            }
            if again.moves == diff.moves, again.unscheduled == diff.unscheduled {
                plan = again
                planRequest = request
                await commit(request, label: label, retry: false)
            } else {
                await afterWrite?()
                plan = again
                planRequest = request
                target = request.target
                note = L10n.Calendar.notePlanChanged
                awaitingDecision = true
            }
        } catch {
            errorMessage = error.localizedDescription
            end()
        }
    }

    /// The choice a changed plan waited for is being made, one way or the
    /// other: its note goes with it.
    private func settleDecision() {
        guard awaitingDecision else { return }
        awaitingDecision = false
        note = nil
    }

    private func end(cancelling id: String? = nil) {
        if let id { Task { [bridge] in await bridge.planCancel(id) } }
        generation += 1
        pump.reset()
        plan = nil
        planRequest = nil
        target = nil
        nudge = nil
        awaitingDecision = false
    }
}

/// Upcoming's half of the list model: what the screen says when it is empty,
/// and the gestures that reschedule its rows.
extension TaskListModel {
    /// Whether the list has nothing to draw. Upcoming always has its days,
    /// so it is empty when no day holds a task or a block and no stream has
    /// an undated task.
    var showsNothing: Bool {
        kind.isUpcoming ? upcoming.allSatisfy(\.isEmpty) : tasks.isEmpty
    }

    /// The empty state's sentence, with the span the core actually used.
    func emptyDescription(hinted: Bool) -> String {
        guard kind.isUpcoming else { return kind.emptyDescription(hinted: hinted) }
        let message = L10n.Upcoming.empty(span: String(upcomingSpan))
        let hint = hinted ? kind.emptyHint : ""
        return hint.isEmpty ? message : "\(message) \(hint)"
    }

    /// The line above the undated tasks when no day in the span holds
    /// anything (`docs/07-clients/shared-ui-system.md` §Empty states).
    var upcomingNotice: String? {
        guard kind.isUpcoming, !showsNothing,
              upcoming.filter({ $0.date != nil }).allSatisfy(\.isEmpty) else { return nil }
        return L10n.Upcoming.onlyUndated(span: String(upcomingSpan))
    }

    /// A day's heading.
    func upcomingHeading(_ date: CivilDate) -> String {
        UpcomingLayout.heading(for: date, today: upcomingToday)
    }

    /// **Drag to reschedule**: `ids` were dropped on `target`.
    func reschedule(_ ids: [EntityRef], to target: UpcomingTarget) async {
        guard !ids.isEmpty else { return }
        await upcomingPlanner.drop(ids, on: target, label: upcomingLabel(ids, to: target))
    }

    /// ⌥← / ⌥→ on the selection: preview moving it `days` from the day the
    /// first of it is planned for. A task drawn under a deadline's day counts
    /// from its plan, not from that day, so ⌥→ always moves a plan later.
    func moveUpcoming(_ ids: [EntityRef], days: Int) async {
        guard let first = ids.first else { return }
        let from = UpcomingLayout.plannedDay(of: first, in: upcoming)
            ?? tasks.first { $0.id == first }?.scheduledAt.map(plannedDate)
        await upcomingPlanner.move(ids, days: days, from: from, today: upcomingToday)
    }

    /// The civil day a plan outside the span falls on, in the zone the list
    /// was read in.
    private func plannedDate(_ value: TimeValue) -> CivilDate {
        if case let .allDay(date) = value { return date }
        return DayScheduleModel.civilDate(ms: timeValueMs(value: value, tz: timeZone), timeZone: timeZone)
    }

    /// Return after ⌥← / ⌥→, or Move after a drop whose plan changed.
    func commitUpcomingPlan() async {
        guard let planned = upcomingPlanner.planned else { return }
        await upcomingPlanner.commitPlan(label: upcomingLabel(planned.tasks, to: planned.target))
    }

    /// The undo step a reschedule becomes: "reschedule ‘Report’", or a count.
    func upcomingLabel(_ ids: [EntityRef], to target: UpcomingTarget) -> String {
        let title = ids.count == 1 ? tasks.first { $0.id == ids[0] }?.title : nil
        switch (target, title) {
        case let (.day, title?): return L10n.Upcoming.undoMove(title: title)
        case (.day, nil): return L10n.Upcoming.undoMoveMany(count: ids.count)
        case let (.noDate, title?): return L10n.Upcoming.undoUnschedule(title: title)
        case (.noDate, nil): return L10n.Upcoming.undoUnscheduleMany(count: ids.count)
        }
    }
}

/// One preview the pump sends: which tasks, where, and for which gesture.
struct UpcomingRequest: Sendable {
    let tasks: [EntityRef]
    let target: UpcomingTarget
    let generation: Int

    var intent: PlanIntentItem { UpcomingPlanner.intent(moving: tasks, to: target) }

    /// The same move, whichever gesture asked for it.
    func same(as other: UpcomingRequest) -> Bool {
        tasks == other.tasks && target == other.target
    }
}

/// A keyboard move in progress: which tasks, the day they started on, and how
/// many days the keys have moved them since.
struct UpcomingNudge: Equatable, Sendable {
    let tasks: [EntityRef]
    let base: CivilDate
    var days = 0
}
