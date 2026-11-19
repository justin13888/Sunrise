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
    private(set) var conflicts: [BlockConflict] = []
    private(set) var names = NameBook()
    private(set) var nowMs: UInt64 = 0
    private(set) var timeZone: String = TimeZone.current.identifier
    /// The first day of a week, read from the core on every refresh.
    private(set) var weekStart: Weekday = .su
    private(set) var errorMessage: String?
    /// Set after a Resolve action, so the grid can say what it did.
    private(set) var note: String?

    /// The plan on screen: what the drag or the keyboard nudge in progress
    /// would do if dropped now. `nil` when no gesture is previewing.
    private(set) var plan: PlanDiffItem?
    /// The line under the gesture that names `plan`'s consequence.
    var planSummary: String? { plan.map(PlanGhosts.summary(of:)) }
    /// The keyboard nudge in progress, if any.
    private(set) var nudge: PlanNudge?

    /// The intent `plan` was solved for.
    private var planIntent: PlanIntentItem?
    /// What the gesture is, for its undo label.
    private var planGesture: PlanGesture?
    private var pump = PlanPreviewPump<PlanRequest>()
    /// Bumped when a gesture ends, so a preview still in flight for it is
    /// not drawn over the grid afterwards.
    private var generation = 0

    private let bridge: CoreBridge

    init(bridge: CoreBridge) {
        self.bridge = bridge
    }

    // MARK: - Reading

    func refresh() async {
        timeZone = TimeZone.current.identifier
        nowMs = await bridge.nowMs()
        if anchorMs == 0 { anchorMs = nowMs }
        weekStart = await Self.weekStart(from: bridge)
        names = await NameBook.load(from: bridge)
        do {
            let query: CoreQuery = span == .day
                ? .dayBlocks(dayMs: anchorMs)
                : .weekBlocks(weekMs: anchorMs)
            guard case let .blocks(rows) = try await bridge.query(query) else {
                self.rows = []
                conflicts = []
                return
            }
            self.rows = rows
            // The domain's answer, not a comparison written here, resolved in
            // the zone the grid draws in.
            conflicts = blockConflicts(rows: rows, tz: timeZone)
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

    /// The `week_start` preference as the core resolves it. Sunday, the
    /// preference's default, when the query fails or the value is missing.
    static func weekStart(from bridge: CoreBridge) async -> Weekday {
        guard case let .preferences(preferences: items)? = try? await bridge.query(.preferences),
              let item = items.first(where: { $0.key == "week_start" }),
              case let .weekday(day: day)? = item.value
        else { return .su }
        return day
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

    /// Drop an existing task onto the grid at `atMs`.
    ///
    /// The block is created with the task already bound and **no title**, which
    /// is not an omission: `Command::CreateBlock` shadow-copies the single
    /// bound task's title as it is now, so the block gets the right name from
    /// the core rather than one composed here that could disagree with it.
    ///
    /// How long it is, is the planner's answer: the slot `plan_preview` gives
    /// the task dropped there, which is its estimate, or the
    /// `planner.default_task_duration_s` preference when it has none. The
    /// preview is cancelled once read; the drop writes the block, not the
    /// task's `planned_at`, because the grid draws blocks.
    func dropTask(_ task: EntityRef, atMs: Int64) async {
        let at = timeValue(ms: atMs, kind: .zoned)
        do {
            let diff = try await bridge.planPreview(.moveTaskTo(task: task, at: at))
            await bridge.planCancel(diff.id)
            guard case let .slot(start, end)? = diff.moves.first(where: { $0.item == task })?.after
            else { return }
            let draft = BlockDraftIn(
                streamId: inboxStreamId(),
                startsAt: start,
                endsAt: end,
                title: nil,
                titleTrackTask: false,
                tasks: [task]
            )
            await run(.createBlock(draft: draft), label: L10n.Calendar.undoCreateBlock)
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// Set a block's times outright, from the Adjust times sheet of a
    /// conflict's Resolve menu: the person typed both, so nothing is planned.
    ///
    /// The kind is preserved, as for a drag. `reach` is the answer to "which
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

    /// Move or resize one event of a repeating block by dragging it.
    ///
    /// The planner reads a repeating block as its series row, not as the
    /// event on screen, so a drag of one event is not a plan it can preview:
    /// this keeps the edit the grid made before the planner, and `reach`
    /// says how far it goes. A block that does not repeat is planned
    /// (``previewDrag(_:mode:toMs:)``).
    ///
    /// The kind is preserved: dragging a floating block an hour later must not
    /// quietly pin it to a zone. That is why this reads the row's own kind back
    /// rather than taking one from the caller.
    func moveRepeating(
        _ row: BlockGridRow,
        mode: BlockDragMode,
        toMs: Int64,
        reach: BlockEditReach?
    ) async {
        let start = timeValueMs(value: row.block.startsAt, tz: timeZone)
        let end = timeValueMs(value: row.block.endsAt, tz: timeZone)
        var edit = BlockEdit()
        switch mode {
        case .move:
            edit.startsAt = Self.sameKind(as: row.block.startsAt, ms: toMs, timeZone: timeZone)
            edit.endsAt = Self.sameKind(as: row.block.endsAt, ms: toMs + (end - start), timeZone: timeZone)
        case .resizeEnd:
            guard toMs > start else { return }
            edit.endsAt = Self.sameKind(as: row.block.endsAt, ms: toMs, timeZone: timeZone)
        }
        await update(row, with: edit, reach: reach, label: L10n.Calendar.undoMoveBlock)
    }

    // MARK: - Planning a drag

    /// The intent for dragging `row` in `mode` so that the held edge lands
    /// at `toMs`: its start for a move, its end for a resize. The time keeps
    /// the kind the block already has.
    func intent(for row: BlockGridRow, mode: BlockDragMode, toMs: Int64) -> PlanIntentItem {
        switch mode {
        case .move:
            .moveBlock(
                block: row.block.id,
                toStart: Self.sameKind(as: row.block.startsAt, ms: toMs, timeZone: timeZone)
            )
        case .resizeEnd:
            .resizeBlock(
                block: row.block.id,
                edge: .end,
                to: Self.sameKind(as: row.block.endsAt, ms: toMs, timeZone: timeZone)
            )
        }
    }

    /// A drag of `row` reached a new snapped position: preview it. The diff
    /// is drawn when it comes back, unless a newer position or the end of
    /// the gesture overtook it.
    func previewDrag(_ row: BlockGridRow, mode: BlockDragMode, toMs: Int64) async {
        await preview(intent(for: row, mode: mode, toMs: toMs), gesture: PlanGesture(row: row, mode: mode))
    }

    /// Preview `intent` under the one-in-flight rule (``PlanPreviewPump``).
    func preview(_ intent: PlanIntentItem, gesture: PlanGesture) async {
        let request = PlanRequest(generation: generation, intent: intent, gesture: gesture)
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
                planIntent = sent.intent
                planGesture = sent.gesture
            case let .failure(error):
                errorMessage = error.localizedDescription
            }
        }
    }

    /// The drag of `row` ended with its held edge at `toMs`: commit the plan
    /// for that position. Dropped where it started, it commits nothing.
    func dropDrag(_ row: BlockGridRow, mode: BlockDragMode, toMs: Int64) async {
        await drop(intent(for: row, mode: mode, toMs: toMs), gesture: PlanGesture(row: row, mode: mode))
    }

    /// Commit the plan for `intent`: the diff on screen when it was solved
    /// for this very position, or a fresh preview of it when the last one
    /// to come back was for an earlier position.
    func drop(_ intent: PlanIntentItem, gesture: PlanGesture) async {
        let diff: PlanDiffItem
        if let plan, planIntent == intent {
            diff = plan
        } else {
            do {
                diff = try await bridge.planPreview(intent)
            } catch {
                errorMessage = error.localizedDescription
                endPlan()
                return
            }
        }
        await commit(diff, intent: intent, gesture: gesture, retry: true)
    }

    /// Commit `diff`, exactly. Stale or forgotten, it is previewed once more
    /// at the same position and committed only if the new diff moves the
    /// same things to the same places; otherwise the new one is left on
    /// screen for Return or Esc (`docs/08-features/planner.md` §Client
    /// contract).
    private func commit(
        _ diff: PlanDiffItem,
        intent: PlanIntentItem,
        gesture: PlanGesture,
        retry: Bool
    ) async {
        // Dropped back where it started: a cancel.
        guard !diff.moves.isEmpty || !diff.unscheduled.isEmpty else {
            endPlan(cancelling: diff.id)
            return
        }
        guard diff.committable else {
            note = L10n.Calendar.notePlanRefused
            endPlan(cancelling: diff.id)
            return
        }
        let label = gesture.label(moved: diff.moves.count + diff.unscheduled.count)
        do {
            _ = try await bridge.planCommit(diff.id, label: label)
            // Re-read before the ghosts go, so the grid steps from the plan
            // to the blocks where it put them without showing the old ones.
            await refresh()
            endPlan()
        } catch BindingError.PlanStale, BindingError.PlanUnknown {
            guard retry, let again = try? await bridge.planPreview(intent) else {
                endPlan()
                await refresh()
                return
            }
            if again.moves == diff.moves, again.unscheduled == diff.unscheduled {
                await commit(again, intent: intent, gesture: gesture, retry: false)
            } else {
                await refresh()
                plan = again
                planIntent = intent
                planGesture = gesture
                note = L10n.Calendar.notePlanChanged
            }
        } catch {
            errorMessage = error.localizedDescription
            endPlan()
        }
    }

    /// Commit whatever plan is on screen: Return, after a keyboard nudge or
    /// after a drop whose plan changed underneath it.
    func commitPlan() async {
        guard let plan, let planIntent, let planGesture else { return }
        await commit(plan, intent: planIntent, gesture: planGesture, retry: true)
    }

    /// Esc, or the end of a gesture: forget the plan on screen and write
    /// nothing.
    func cancelPlan() {
        endPlan(cancelling: plan?.id)
    }

    private func endPlan(cancelling id: String? = nil) {
        if let id { Task { [bridge] in await bridge.planCancel(id) } }
        generation += 1
        pump.reset()
        plan = nil
        planIntent = nil
        planGesture = nil
        nudge = nil
    }

    /// The ghosts of the plan on screen that the `offset`-th day draws.
    func ghosts(dayOffset: Int) -> [PlanGhost] {
        guard let plan else { return [] }
        return PlanGhosts.place(
            diff: plan,
            fromMs: dayStartMs(offset: dayOffset),
            toMs: dayStartMs(offset: dayOffset + 1),
            timeZone: timeZone
        )
    }

    // MARK: - Planning from the keyboard

    /// One planning key on the block `row`: ⌥↑/⌥↓ move it a snap, ⇧⌥↑/⇧⌥↓
    /// move its end a snap, ⌥←/⌥→ move it a day, each previewing the same
    /// plan a drag there would; Return commits it and Esc cancels it.
    ///
    /// Returns whether the key did anything. A repeating block is not
    /// planned (``moveRepeating(_:mode:toMs:reach:)``), so its keys fall
    /// through. Nudges add up until Return or Esc; switching between moving
    /// and resizing starts again from where the block is.
    @discardableResult
    func plan(_ key: PlanKey, on row: BlockGridRow) async -> Bool {
        switch key {
        case .commit:
            guard plan != nil else { return false }
            await commitPlan()
            return true
        case .cancel:
            guard plan != nil || nudge != nil else { return false }
            cancelPlan()
            return true
        default:
            guard !row.recurs else { return false }
            var next = nudge.flatMap { $0.block == row.gridKey ? $0 : nil } ?? PlanNudge(block: row.gridKey)
            next.press(key)
            nudge = next
            await preview(intent(for: row, nudge: next), gesture: PlanGesture(row: row, mode: next.mode))
            return true
        }
    }

    /// The intent a nudge stands for, from where `row` is now.
    func intent(for row: BlockGridRow, nudge: PlanNudge) -> PlanIntentItem {
        let step = Int64(snapMinutes) * 60_000
        switch nudge.mode {
        case .move:
            var calendar = Calendar(identifier: .gregorian)
            calendar.timeZone = TimeZone(identifier: timeZone) ?? .current
            let start = timeValueMs(value: row.block.startsAt, tz: timeZone)
            let date = Date(timeIntervalSince1970: Double(start) / 1000)
            let moved = calendar.date(byAdding: .day, value: nudge.days, to: date) ?? date
            let ms = Int64(moved.timeIntervalSince1970 * 1000) + Int64(nudge.steps) * step
            return intent(for: row, mode: .move, toMs: ms)
        case .resizeEnd:
            let end = timeValueMs(value: row.block.endsAt, tz: timeZone)
            return intent(for: row, mode: .resizeEnd, toMs: end + Int64(nudge.steps) * step)
        }
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

    /// The same value at a new instant, keeping whichever of the four kinds it
    /// already had. `timeZone` is the zone the grid draws in.
    static func sameKind(as existing: TimeValue, ms: Int64, timeZone: String) -> TimeValue {
        switch existing {
        case .instant: .instant(at: ms)
        case let .zoned(_, tz): .zoned(civil: civilText(ms: ms, tz: tz), tz: tz)
        case .floating: .floating(civil: civilText(ms: ms, tz: timeZone))
        // An all-day block dragged to a time is no longer all-day: the user
        // just gave it one. Widening to zoned is the honest reading of that.
        case .allDay: .zoned(civil: civilText(ms: ms, tz: timeZone), tz: timeZone)
        // A kind a newer build wrote cannot be re-expressed at another
        // instant here; the drag is an explicit set, so it reads as local
        // time, as all-day does.
        case .unknown: .zoned(civil: civilText(ms: ms, tz: timeZone), tz: timeZone)
        }
    }

    private func civilText(ms: Int64) -> String {
        Self.civilText(ms: ms, tz: timeZone)
    }

    /// `2026-03-04T09:15:00`, the text `jiff` parses a civil date-time from.
    private static func civilText(ms: Int64, tz: String) -> String {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(identifier: tz) ?? .current
        let parts = calendar.dateComponents(
            [.year, .month, .day, .hour, .minute, .second],
            from: Date(timeIntervalSince1970: Double(ms) / 1000)
        )
        return String(
            format: "%04d-%02d-%02dT%02d:%02d:%02d",
            parts.year ?? 1970, parts.month ?? 1, parts.day ?? 1,
            parts.hour ?? 0, parts.minute ?? 0, parts.second ?? 0
        )
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

/// What a planned gesture did to its block, for the one undo step it
/// becomes: "move ‘Standup’ (moved 3)".
struct PlanGesture: Equatable, Sendable {
    let title: String
    let mode: BlockDragMode

    init(title: String, mode: BlockDragMode) {
        self.title = title
        self.mode = mode
    }

    init(row: BlockGridRow, mode: BlockDragMode) {
        self.init(title: row.title ?? L10n.Calendar.untitledBlock, mode: mode)
    }

    /// `moved` counts every item the commit moves, the dragged block too.
    func label(moved: Int) -> String {
        switch mode {
        case .move: L10n.Calendar.undoPlanMove(title: title, count: moved)
        case .resizeEnd: L10n.Calendar.undoPlanResize(title: title, count: moved)
        }
    }
}

/// One preview the pump sends: the gesture it belongs to and the position.
struct PlanRequest {
    let generation: Int
    let intent: PlanIntentItem
    let gesture: PlanGesture
}

/// A keyboard nudge in progress: how far the planning keys have taken one
/// block since the last Return or Esc.
struct PlanNudge: Equatable, Sendable {
    /// The grid key of the block's row.
    let block: String
    private(set) var mode: BlockDragMode = .move
    /// Snaps: of the start for a move, of the end for a resize.
    private(set) var steps = 0
    /// Whole days, for a move.
    private(set) var days = 0

    init(block: String) {
        self.block = block
    }

    /// Add one key's worth. A key of the other kind starts the count again.
    mutating func press(_ key: PlanKey) {
        let wanted: BlockDragMode = key == .shorter || key == .longer ? .resizeEnd : .move
        if wanted != mode {
            mode = wanted
            steps = 0
            days = 0
        }
        switch key {
        case .earlier, .shorter: steps -= 1
        case .later, .longer: steps += 1
        case .previousDay: days -= 1
        case .nextDay: days += 1
        case .commit, .cancel: break
        }
    }
}
