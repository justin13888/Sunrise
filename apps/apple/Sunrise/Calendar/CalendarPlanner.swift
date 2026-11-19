import Foundation

/// The calendar's drags, planned by the core (`docs/08-features/planner.md`).
///
/// Every drag that moves time is a gesture sent to `plan_preview` on each
/// change of snapped position; the diff that comes back is what the grid
/// draws, and the drop commits exactly that diff, as one undo step. Nothing
/// here decides where anything goes: the client turns a pointer or a key into
/// a target time, and the core answers with every position, conflict and
/// violation.
///
/// Apart from ``CalendarModel`` because it is a second job with its own state
/// (the plan on screen, the one preview in flight, the keyboard nudge), and
/// the grid's model already reads and writes blocks. It owns no rows: what it
/// needs of the grid — the zone the grid draws in, the snap step — is passed
/// in with each gesture, and a commit hands back to the grid to re-read.
@MainActor
@Observable
final class CalendarPlanner {
    /// The plan on screen: what the drag or the keyboard nudge in progress
    /// would do if dropped now. `nil` when no gesture is previewing.
    private(set) var plan: PlanDiffItem?
    /// The keyboard nudge in progress, if any.
    private(set) var nudge: PlanNudge?
    /// Set after a drop that was refused or whose plan changed underneath it.
    private(set) var note: String?
    private(set) var errorMessage: String?

    /// The line under the gesture that names `plan`'s consequence.
    var summary: String? { plan.map(PlanGhosts.summary(of:)) }

    /// The intent `plan` was solved for.
    private var planIntent: PlanIntentItem?
    /// What the gesture is, for its undo label.
    private var planGesture: PlanGesture?
    private var pump = PlanPreviewPump<PlanRequest>()
    /// Bumped when a gesture ends, so a preview still in flight for it is
    /// not drawn over the grid afterwards.
    private var generation = 0

    private let bridge: CoreBridge
    /// Re-read the grid after a write. Set by the grid's model, which owns
    /// this planner, once it exists.
    @ObservationIgnored var afterWrite: (@MainActor () async -> Void)?

    init(bridge: CoreBridge) {
        self.bridge = bridge
    }

    private func refresh() async {
        await afterWrite?()
    }

    func dismissNote() { note = nil }

    func dismissError() { errorMessage = nil }

    // MARK: - Intents

    /// The intent for dragging `row` in `mode` so that the held edge lands
    /// at `toMs`: its start for a move, its end for a resize. The time keeps
    /// the kind the block already has: dragging a floating block an hour
    /// later must not quietly pin it to a zone.
    static func intent(
        for row: BlockGridRow,
        mode: BlockDragMode,
        toMs: Int64,
        timeZone: String
    ) -> PlanIntentItem {
        switch mode {
        case .move:
            .moveBlock(
                block: row.block.id,
                toStart: CalendarModel.sameKind(as: row.block.startsAt, ms: toMs, timeZone: timeZone)
            )
        case .resizeEnd:
            .resizeBlock(
                block: row.block.id,
                edge: .end,
                to: CalendarModel.sameKind(as: row.block.endsAt, ms: toMs, timeZone: timeZone)
            )
        }
    }

    /// The intent a keyboard nudge stands for, from where `row` is now.
    static func intent(
        for row: BlockGridRow,
        nudge: PlanNudge,
        snapMinutes: Int,
        timeZone: String
    ) -> PlanIntentItem {
        let step = Int64(snapMinutes) * 60_000
        switch nudge.mode {
        case .move:
            var calendar = Calendar(identifier: .gregorian)
            calendar.timeZone = TimeZone(identifier: timeZone) ?? .current
            let start = timeValueMs(value: row.block.startsAt, tz: timeZone)
            let date = Date(timeIntervalSince1970: Double(start) / 1000)
            // Civil days, so a nudge across a DST change keeps the clock time.
            let moved = calendar.date(byAdding: .day, value: nudge.days, to: date) ?? date
            let ms = Int64(moved.timeIntervalSince1970 * 1000) + Int64(nudge.steps) * step
            return intent(for: row, mode: .move, toMs: ms, timeZone: timeZone)
        case .resizeEnd:
            let end = timeValueMs(value: row.block.endsAt, tz: timeZone)
            let ms = end + Int64(nudge.steps) * step
            return intent(for: row, mode: .resizeEnd, toMs: ms, timeZone: timeZone)
        }
    }

    /// Where one event of a repeating block goes when dragged in `mode` with
    /// its held edge at `toMs`, or `nil` for a resize that would end it at or
    /// before its start.
    ///
    /// The planner reads a repeating block as its series row, not as the
    /// event on screen, so a drag of one event is not a plan it can preview:
    /// that drag keeps the edit the grid made before the planner, with its
    /// "which events?" question, and this is the span it writes. A move keeps
    /// the event's length; a resize keeps its start.
    static func repeatingSpan(
        of row: BlockGridRow,
        mode: BlockDragMode,
        toMs: Int64,
        timeZone: String
    ) -> GridSpan? {
        let start = timeValueMs(value: row.block.startsAt, tz: timeZone)
        let end = timeValueMs(value: row.block.endsAt, tz: timeZone)
        switch mode {
        case .move:
            return GridSpan(startMs: toMs, endMs: toMs + (end - start))
        case .resizeEnd:
            return toMs > start ? GridSpan(startMs: start, endMs: toMs) : nil
        }
    }

    // MARK: - A drag

    /// A drag of `row` reached a new snapped position: preview it. The diff
    /// is drawn when it comes back, unless a newer position or the end of
    /// the gesture overtook it.
    func previewDrag(_ row: BlockGridRow, mode: BlockDragMode, toMs: Int64, timeZone: String) async {
        await preview(
            Self.intent(for: row, mode: mode, toMs: toMs, timeZone: timeZone),
            gesture: PlanGesture(row: row, mode: mode)
        )
    }

    /// The drag of `row` ended with its held edge at `toMs`: commit the plan
    /// for that position. Dropped where it started, it commits nothing.
    func dropDrag(_ row: BlockGridRow, mode: BlockDragMode, toMs: Int64, timeZone: String) async {
        await drop(
            Self.intent(for: row, mode: mode, toMs: toMs, timeZone: timeZone),
            gesture: PlanGesture(row: row, mode: mode)
        )
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
                end()
                return
            }
        }
        await commit(diff, intent: intent, gesture: gesture, retry: true)
    }

    /// Commit whatever plan is on screen: Return, after a keyboard nudge or
    /// after a drop whose plan changed underneath it.
    func commitPlan() async {
        guard let plan, let planIntent, let planGesture else { return }
        await commit(plan, intent: planIntent, gesture: planGesture, retry: true)
    }

    /// Esc, or the end of a gesture: forget the plan on screen and write
    /// nothing.
    func cancel() {
        end(cancelling: plan?.id)
    }

    /// The ghosts of the plan on screen that the column `[fromMs, toMs)`
    /// draws.
    func ghosts(fromMs: Int64, toMs: Int64, timeZone: String) -> [PlanGhost] {
        guard let plan else { return [] }
        return PlanGhosts.place(diff: plan, fromMs: fromMs, toMs: toMs, timeZone: timeZone)
    }

    /// Whether the plan on screen moves `block`.
    func moves(_ block: EntityRef) -> Bool {
        plan?.moves.contains { $0.item == block } ?? false
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
            end(cancelling: diff.id)
            return
        }
        guard diff.committable else {
            note = L10n.Calendar.notePlanRefused
            end(cancelling: diff.id)
            return
        }
        let label = gesture.label(moved: diff.moves.count + diff.unscheduled.count)
        do {
            _ = try await bridge.planCommit(diff.id, label: label)
            // Re-read before the ghosts go, so the grid steps from the plan
            // to the blocks where it put them without showing the old ones.
            await refresh()
            end()
        } catch BindingError.PlanStale(_), BindingError.PlanUnknown(_) {
            guard retry, let again = try? await bridge.planPreview(intent) else {
                await refresh()
                end()
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
            end()
        }
    }

    private func end(cancelling id: String? = nil) {
        if let id { Task { [bridge] in await bridge.planCancel(id) } }
        generation += 1
        pump.reset()
        plan = nil
        planIntent = nil
        planGesture = nil
        nudge = nil
    }

    // MARK: - The keyboard

    /// One planning key on the block `row`: ⌥↑/⌥↓ move it a snap, ⇧⌥↑/⇧⌥↓
    /// move its end a snap, ⌥←/⌥→ move it a day, each previewing the same
    /// plan a drag there would; Return commits it and Esc cancels it.
    ///
    /// Returns whether the key did anything. A repeating block is not
    /// planned (``repeatingSpan(of:mode:toMs:timeZone:)``), so its moving
    /// keys fall through. Nudges add up until Return or Esc; switching
    /// between moving and resizing starts again from where the block is.
    @discardableResult
    func press(_ key: PlanKey, on row: BlockGridRow, snapMinutes: Int, timeZone: String) async -> Bool {
        switch key {
        case .commit:
            guard plan != nil else { return false }
            await commitPlan()
            return true
        case .cancel:
            guard plan != nil || nudge != nil else { return false }
            cancel()
            return true
        default:
            guard !row.recurs else { return false }
            var next = nudge.flatMap { $0.block == row.gridKey ? $0 : nil } ?? PlanNudge(block: row.gridKey)
            next.press(key)
            nudge = next
            await preview(
                Self.intent(for: row, nudge: next, snapMinutes: snapMinutes, timeZone: timeZone),
                gesture: PlanGesture(row: row, mode: next.mode)
            )
            return true
        }
    }

    // MARK: - A task onto the grid

    /// Drop the task `task` onto the grid at `atMs`, in the grid's zone.
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
    func dropTask(_ task: EntityRef, atMs: Int64, timeZone: String) async {
        // A drop onto a zoned grid writes a `Zoned` time in the grid's zone
        // (`planner.md` §`PlanIntent`): the instant, re-expressed as one.
        let zoned = TimeValue.zoned(civil: "", tz: timeZone)
        let at = CalendarModel.sameKind(as: zoned, ms: atMs, timeZone: timeZone)
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
            let outcome = try await bridge.submitUndoable(
                .createBlock(draft: draft),
                label: L10n.Calendar.undoCreateBlock
            )
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
