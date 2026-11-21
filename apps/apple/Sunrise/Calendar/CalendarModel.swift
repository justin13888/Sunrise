import Foundation

/// Day or week. The two queries behind them differ, so this is not a display
/// toggle over one result set.
enum CalendarSpan: String, CaseIterable, Identifiable {
    case day
    case week

    var id: Self { self }
    var title: String { self == .day ? L10n.Calendar.spanDay : L10n.Calendar.spanWeek }
    /// How many civil days the grid draws.
    var dayCount: Int { self == .day ? 1 : 7 }
}

/// How a newly drawn block records its time.
///
/// The four `SunriseTime` kinds are the reason this picker exists rather than
/// a default nobody can see. `docs/02-domain/time-blocks.md` is explicit: a
/// 09:00 block and a block at a fixed instant are different commitments, and
/// flying to another timezone must move one and not the other. A calendar that
/// silently picked one for you would be deciding which of your meetings move.
enum BlockTimeKind: String, CaseIterable, Identifiable {
    /// 09:00 in this device's zone, named. Travels with the place.
    case zoned
    /// 09:00 wherever you are. Travels with you.
    case floating
    /// A fixed point on the timeline. Moves for nobody.
    case instant

    var id: Self { self }

    var title: String {
        switch self {
        case .zoned: L10n.Calendar.kindZoned
        case .floating: L10n.Calendar.kindFloating
        case .instant: L10n.Calendar.kindInstant
        }
    }

    var explanation: String {
        switch self {
        case .zoned: L10n.Calendar.kindZonedExplanation
        case .floating: L10n.Calendar.kindFloatingExplanation
        case .instant: L10n.Calendar.kindInstantExplanation
        }
    }
}

/// The calendar grid: day and week time-blocking.
///
/// One query per span (`DayBlocks` / `WeekBlocks`) and one call to
/// `blockConflicts` on the result. Neither the overlap rule nor the merge is
/// computed here — both live in `sunrise-domain`, because "back-to-back is not
/// a conflict" and "which time kind survives a union" are decisions two clients
/// must not answer differently.
///
/// Beside the blocks it draws the open tasks planned for a time of day, read
/// through `Upcoming` (``timedTasks()``): a task dropped on the grid is
/// planned there, not turned into a block (``CalendarPlanner``).
@MainActor
@Observable
final class CalendarModel {
    var span: CalendarSpan = .day {
        didSet { Task { await refresh() } }
    }

    /// Minutes a drag snaps to. `interaction-patterns.md` sets the default and
    /// the choices.
    var snapMinutes = 15
    static let snapChoices = [5, 10, 15, 30, 60]

    /// Any instant inside the day or week on screen. Not "the start of": the
    /// two queries take an instant and resolve the boundary themselves, so the
    /// grid never has to agree with the core about when a week begins.
    private(set) var anchorMs: UInt64 = 0
    private(set) var rows: [BlockGridRow] = []
    /// The open tasks planned for a time of day in or around the window,
    /// drawn apart from blocks (``placedTasks(dayOffset:)``).
    private(set) var tasks: [TaskItem] = []
    /// How long a task with no estimate is drawn: the
    /// `planner.default_task_duration_s` preference, the slot the planner
    /// gives it, read from the core on every refresh.
    private(set) var defaultTaskDurationS: UInt64 = 30 * 60
    private(set) var conflicts: [BlockConflict] = []
    private(set) var names = NameBook()
    private(set) var nowMs: UInt64 = 0
    private(set) var timeZone: String = TimeZone.current.identifier
    /// The first day of a week, read from the core on every refresh.
    private(set) var weekStart: Weekday = .su
    private(set) var errorMessage: String?
    /// Set after a Resolve action, so the grid can say what it did.
    private(set) var note: String?

    /// The grid's drags, planned by the core: see ``CalendarPlanner``.
    @ObservationIgnored let planner: CalendarPlanner

    private let bridge: CoreBridge

    init(bridge: CoreBridge) {
        self.bridge = bridge
        planner = CalendarPlanner(bridge: bridge)
        planner.afterWrite = { [weak self] in await self?.refresh() }
    }

    // MARK: - Reading

    func refresh() async {
        timeZone = TimeZone.current.identifier
        nowMs = await bridge.nowMs()
        if anchorMs == 0 { anchorMs = nowMs }
        (weekStart, defaultTaskDurationS) = await Self.gridPreferences(from: bridge)
        names = await NameBook.load(from: bridge)
        do {
            let query: CoreQuery = span == .day
                ? .dayBlocks(dayMs: anchorMs)
                : .weekBlocks(weekMs: anchorMs)
            guard case let .blocks(rows) = try await bridge.query(query) else {
                self.rows = []
                tasks = []
                conflicts = []
                return
            }
            self.rows = rows
            // The domain's answer, not a comparison written here, resolved in
            // the zone the grid draws in.
            conflicts = blockConflicts(rows: rows, tz: timeZone)
            tasks = try await timedTasks()
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// Follow the change stream. A lagged batch and a complete one get the same
    /// full re-query: the ids in an incomplete batch are not the whole story,
    /// and a grid that patched only the named blocks would be right until the
    /// first sync burst and silently wrong after it.
    func follow() async {
        for await batch in await bridge.changes() {
            guard !batch.isClosed else { return }
            await refresh()
        }
    }

    // MARK: - Navigating

    /// Move by whole spans. Civil days, via `Calendar`, so a week that contains
    /// a DST change is still seven days rather than 167 hours.
    func step(_ direction: Int) async {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(identifier: timeZone) ?? .current
        let anchor = Date(timeIntervalSince1970: Double(anchorMs) / 1000)
        let moved = calendar.date(
            byAdding: .day,
            value: direction * span.dayCount,
            to: anchor
        ) ?? anchor
        anchorMs = UInt64(max(0, moved.timeIntervalSince1970 * 1000))
        await refresh()
    }

    func goToToday() async {
        anchorMs = await bridge.nowMs()
        await refresh()
    }

    /// Move the grid onto the day a block is on, and hand the row back.
    ///
    /// What a `sunrise://entity/<blk_…>` link needs, which is the link every
    /// block reminder carries (`ReminderPlan/notification(for:timeZone:)`).
    /// Opening the calendar was never the hard part; opening it on **today**
    /// when the block is on Thursday is a link that lands somewhere plausible
    /// and wrong, and nothing on the screen says so.
    ///
    /// `nil` when the vault has no such block — deleted elsewhere, or an id
    /// from another vault — and the grid then stays where it was, which
    /// `docs/07-clients/interaction-patterns.md` asks for: "A link naming an
    /// entity that does not exist resolves to the nearest sensible screen
    /// rather than an error dialog."
    func reveal(_ block: EntityRef) async -> BlockGridRow? {
        guard case let .blocks(rows)? = try? await bridge.query(.entityById(id: block)),
              let row = rows.first else { return nil }
        // The start resolved through the same seam the grid draws with, so a
        // floating block lands on the day this device reads it as.
        let start = timeValueMs(value: row.block.startsAt, tz: timeZone)
        anchorMs = UInt64(max(0, start))
        await refresh()
        return self.row(block)
    }

    // MARK: - The grid's geometry

    /// Start of the first civil day the grid draws, epoch ms.
    var windowStartMs: Int64 {
        dayStartMs(offset: 0)
    }

    /// End of the last civil day, epoch ms.
    var windowEndMs: Int64 {
        dayStartMs(offset: span.dayCount)
    }

    /// Start of the `offset`-th day of the grid.
    ///
    /// A week starts on the `week_start` preference, the one
    /// `Query::WeekBlocks` reads (`docs/10-cross-cutting/time.md` §4).
    /// Getting this wrong would draw the right blocks in the wrong columns —
    /// the sort of bug that looks like a sync failure.
    func dayStartMs(offset: Int) -> Int64 {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(identifier: timeZone) ?? .current
        calendar.firstWeekday = Self.firstWeekday(weekStart)
        let anchor = Date(timeIntervalSince1970: Double(anchorMs) / 1000)
        let base: Date = span == .day
            ? calendar.startOfDay(for: anchor)
            : calendar.dateInterval(of: .weekOfYear, for: anchor)?.start
                ?? calendar.startOfDay(for: anchor)
        let day = calendar.date(byAdding: .day, value: offset, to: base) ?? base
        return Int64(day.timeIntervalSince1970 * 1000)
    }

    /// The grid's two preferences, as the core resolves them: `week_start`
    /// and `planner.default_task_duration_s`. Each keeps its default, Sunday
    /// and 30 minutes, when the query fails or the value is missing.
    static func gridPreferences(from bridge: CoreBridge) async -> (weekStart: Weekday, taskS: UInt64) {
        var found: (weekStart: Weekday, taskS: UInt64) = (.su, 30 * 60)
        guard case let .preferences(preferences: items)? = try? await bridge.query(.preferences) else {
            return found
        }
        for item in items {
            switch (item.key, item.value) {
            case let ("week_start", .weekday(day)?): found.weekStart = day
            case let ("planner.default_task_duration_s", .uint(secs)?) where secs > 0: found.taskS = secs
            default: break
            }
        }
        return found
    }

    /// The open tasks planned for a time of day that the window can draw,
    /// read through `Query::Upcoming`, the core's fold of which open task is
    /// planned for which day. Its days start at "now", so it is asked from
    /// the day before the window over a span that covers the window whatever
    /// bounds the day schedule gives a planner day; each column keeps what
    /// falls in it (``placedTasks(dayOffset:)``).
    private func timedTasks() async throws -> [TaskItem] {
        let from = UInt64(max(0, windowStartMs - 86_400_000))
        let query = CoreQuery.upcoming(nowMs: from, spanDays: span == .day ? 7 : 14, contexts: [])
        guard case let .upcoming(report) = try await bridge.query(query) else { return [] }
        return BlockLayout.timedTasks(in: report, timeZone: timeZone)
    }

    /// `Calendar.firstWeekday` for a preference day: 1 is Sunday. A day token
    /// this build does not know starts the week on the default, Sunday, as the
    /// core does.
    nonisolated static func firstWeekday(_ day: Weekday) -> Int {
        switch day {
        case .su: return 1
        case .mo: return 2
        case .tu: return 3
        case .we: return 4
        case .th: return 5
        case .fr: return 6
        case .sa: return 7
        case .unknown: return 1
        }
    }

    /// The blocks of the `offset`-th day, laid out.
    func placed(dayOffset: Int) -> [PlacedBlock] {
        BlockLayout.place(
            rows: rows,
            fromMs: dayStartMs(offset: dayOffset),
            toMs: dayStartMs(offset: dayOffset + 1),
            timeZone: timeZone
        )
    }

    /// The timed tasks of the `offset`-th day, laid out in their own track.
    func placedTasks(dayOffset: Int) -> [PlacedTask] {
        BlockLayout.placeTasks(
            tasks,
            defaultDurationS: defaultTaskDurationS,
            fromMs: dayStartMs(offset: dayOffset),
            toMs: dayStartMs(offset: dayOffset + 1),
            timeZone: timeZone
        )
    }

    /// The shaded conflict regions of the `offset`-th day.
    func shading(dayOffset: Int) -> [PlacedConflict] {
        BlockLayout.shade(
            conflicts: conflicts,
            fromMs: dayStartMs(offset: dayOffset),
            toMs: dayStartMs(offset: dayOffset + 1)
        )
    }

    func snap(_ ms: Int64, dayOffset: Int) -> Int64 {
        BlockLayout.snap(
            ms: ms,
            toMinutes: snapMinutes,
            dayStartMs: dayStartMs(offset: dayOffset)
        )
    }

    /// The row for an id, for a Resolve menu that has a conflict and needs its
    /// two blocks.
    func row(_ id: EntityRef) -> BlockGridRow? {
        rows.first { $0.block.id == id }
    }

    /// The row for an id that `conflict` names: for a repeating block, the
    /// event the overlap is in rather than the first of its events on the
    /// grid, which a conflict's two block ids alone cannot tell apart.
    func row(_ id: EntityRef, in conflict: BlockConflict) -> BlockGridRow? {
        rows.first { row in
            guard row.block.id == id else { return false }
            let start = timeValueMs(value: row.block.startsAt, tz: timeZone)
            let end = timeValueMs(value: row.block.endsAt, tz: timeZone)
            return start < conflict.toMs && end > conflict.fromMs
        }
    }

    // MARK: - Writing

    /// Create a block from a drag on the grid.
    ///
    /// `stream` is where it is filed; the Inbox when the caller has no better
    /// answer, which is the same default a capture takes.
    func createBlock(
        fromMs: Int64,
        toMs: Int64,
        title: String?,
        kind: BlockTimeKind,
        stream: EntityRef? = nil,
        tasks: [EntityRef] = []
    ) async {
        let draft = BlockDraftIn(
            streamId: stream ?? inboxStreamId(),
            startsAt: timeValue(ms: fromMs, kind: kind),
            endsAt: timeValue(ms: toMs, kind: kind),
            title: title?.nilIfBlank,
            titleTrackTask: false,
            tasks: tasks
        )
        await run(.createBlock(draft: draft), label: L10n.Calendar.undoCreateBlock)
    }

    /// Set a block's times outright: from the Adjust times sheet, where the
    /// person typed both, or from a drag of one event of a repeating block,
    /// which the planner cannot preview (``CalendarPlanner/repeatingSpan(of:mode:toMs:timeZone:)``).
    ///
    /// The kind is preserved: dragging a floating block an hour later must
    /// not quietly pin it to a zone. `reach` is the answer to "which
    /// events?" for one event of a repeating block, and is ignored for a
    /// block that does not recur.
    func moveBlock(
        _ row: BlockGridRow,
        toStartMs: Int64,
        toEndMs: Int64,
        reach: BlockEditReach? = nil
    ) async {
        var edit = BlockEdit()
        edit.startsAt = Self.sameKind(as: row.block.startsAt, ms: toStartMs, timeZone: timeZone)
        edit.endsAt = Self.sameKind(as: row.block.endsAt, ms: toEndMs, timeZone: timeZone)
        await update(row, with: edit, reach: reach, label: L10n.Calendar.undoMoveBlock)
    }

    func bind(task: EntityRef, to block: EntityRef) async {
        await run(.bindTask(block: block, task: task), label: L10n.Calendar.undoBindTask)
    }

    /// Unbind `task` from `block`: from the occurrence `occurrenceKey` names,
    /// or with none from the whole block.
    func unbind(task: EntityRef, from block: EntityRef, occurrenceKey: String? = nil) async {
        await run(
            .unbindTask(block: block, task: task, occurrenceKey: occurrenceKey),
            label: L10n.Calendar.undoUnbindTask
        )
    }

    /// Unbind `task` from the block `row` shows. An occurrence's row lists
    /// the tasks bound to that occurrence beside those bound to the whole
    /// block, and the core removes an occurrence binding only when the
    /// unbind names its key, so a task bound to this occurrence is unbound
    /// from it, and any other from the whole block.
    func unbind(task: EntityRef, from row: BlockGridRow) async {
        let key = row.occurrenceKey.flatMap { key in
            row.block.occurrenceTasks.contains { $0.key == key && $0.task == task } ? key : nil
        }
        await unbind(task: task, from: row.block.id, occurrenceKey: key)
    }

    /// Delete a block, or as much of a repeating one as `reach` says.
    func delete(_ row: BlockGridRow, reach: BlockEditReach? = nil) async {
        guard let command = Self.deleteCommand(row: row, reach: reach) else { return }
        await run(command, label: L10n.Calendar.undoDeleteBlock)
    }

    /// Apply an edit from the block editor, as far as `reach` says for one
    /// event of a repeating block.
    func apply(_ edit: BlockEdit, to row: BlockGridRow, reach: BlockEditReach? = nil) async {
        await update(row, with: edit, reach: reach, label: L10n.Calendar.undoEditBlock)
    }

    /// Send `edit` for `row`. An edit of every event moves the series'
    /// anchor, which is not on the row, so it is read first.
    private func update(
        _ row: BlockGridRow,
        with edit: BlockEdit,
        reach: BlockEditReach?,
        label: String
    ) async {
        var anchor: BlockItem?
        if row.recurs, reach == .allEvents, edit.movesTimes {
            do {
                guard case let .blocks(stored) = try await bridge.query(.entityById(id: row.block.id)),
                      let first = stored.first
                else { return }
                anchor = first.block
            } catch {
                errorMessage = error.localizedDescription
                return
            }
        }
        guard let command = Self.updateCommand(
            row: row,
            edit: edit,
            reach: reach,
            anchor: anchor,
            timeZone: timeZone
        ) else { return }
        await run(command, label: label)
    }

    // MARK: - Resolving a conflict

    /// **Keep both** — the documented no-op. It exists so the menu has three
    /// entries and the user can say "yes, on purpose"; nothing is written.
    func keepBoth() {
        note = L10n.Calendar.noteKeptBoth
    }

    /// **Merge** — tombstone the two blocks and create their union.
    ///
    /// The draft is `merged_block_draft`'s, not one composed here: the union's
    /// time *kind* is a domain decision, and getting it wrong would silently
    /// re-anchor one of the two commitments.
    ///
    /// Three commands, not one. The core has no multi-op transaction, so a
    /// failure part-way leaves the earlier ones applied — which is why the
    /// create is submitted last: a merge that fails to create leaves two
    /// tombstones and no block, and a merge that fails to tombstone leaves the
    /// user looking at three blocks. The second is recoverable by hand and the
    /// first is not, so the create is validated by the seam *before* anything
    /// is deleted.
    ///
    /// A repeating block is not merged: the tombstone would remove every
    /// event of it, not the one that overlaps. The note says to adjust its
    /// times instead, which asks how far the change reaches.
    func merge(_ conflict: BlockConflict) async {
        guard let first = row(conflict.a, in: conflict),
              let second = row(conflict.b, in: conflict) else { return }
        guard !first.recurs, !second.recurs else {
            note = L10n.Calendar.noteMergeRepeating
            return
        }
        do {
            let draft = try mergedBlockDraft(a: first, b: second, tz: timeZone)
            for id in [first.block.id, second.block.id] {
                _ = try await bridge.submit(.deleteBlock(id: id))
            }
            _ = try await bridge.submit(.createBlock(draft: draft))
            note = L10n.Calendar.noteMerged
            await refresh()
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func dismissNote() { note = nil }

    func dismissError() { errorMessage = nil }

    // MARK: - Time values

    /// An instant, expressed in the kind the user chose.
    ///
    /// The civil text is this device's calendar rendering of the instant; the
    /// meaning of that text afterwards is the domain's, which is why the value
    /// crosses as a `TimeValue` rather than a number.
    func timeValue(ms: Int64, kind: BlockTimeKind) -> TimeValue {
        switch kind {
        case .instant:
            return .instant(at: ms)
        case .zoned:
            return .zoned(civil: civilText(ms: ms), tz: timeZone)
        case .floating:
            return .floating(civil: civilText(ms: ms))
        }
    }

    private func civilText(ms: Int64) -> String {
        Self.civilText(ms: ms, tz: timeZone)
    }

    private func run(_ command: CoreCommand, label: String) async {
        do {
            let outcome = try await bridge.submitUndoable(command, label: label)
            if let refusal = outcome.notUndoable {
                note = undoRefusalExplanation(refusal: refusal)
            }
            await refresh()
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}
