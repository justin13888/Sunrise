import Foundation

/// One block, placed on the grid.
///
/// The times are already resolved to instants — by `timeValueMs`, which is the
/// domain deciding what each of the four `SunriseTime` kinds means on this
/// device. Nothing below re-reads the kind, and that is the point: layout is
/// arithmetic on two numbers, and the moment it started branching on "is this
/// floating" it would be a second implementation of the thing the seam exists
/// to own.
struct PlacedBlock: Equatable, Identifiable {
    let row: BlockGridRow
    /// Resolved start, epoch ms.
    let startMs: Int64
    /// Resolved end, epoch ms.
    let endMs: Int64
    /// Which column this block sits in, among the blocks it overlaps.
    let lane: Int
    /// How many columns that cluster of overlapping blocks needs.
    let laneCount: Int

    /// The row's grid key: one event of a repeating block is its own chip,
    /// and a weekly block on a week grid would otherwise be seven chips with
    /// one identity.
    var id: String { row.gridKey }

    /// The block this chip draws, or draws one event of.
    var blockId: EntityRef { row.block.id }
}

/// One timed task, placed on the grid: a task whose `planned_at` is a time
/// (`docs/08-features/planner.md` §`PlanIntent`), drawn in a track of its
/// own beside the blocks rather than among them.
///
/// Its span is the slot the planner gives it: from `planned_at`, as long as
/// its estimate, or the `planner.default_task_duration_s` preference when it
/// has none. Resolved to instants by `timeValueMs`, as a block is.
struct PlacedTask: Equatable, Identifiable {
    let task: TaskItem
    /// Resolved start, epoch ms, clamped to the column.
    let startMs: Int64
    /// Resolved end, epoch ms, clamped to the column.
    let endMs: Int64
    /// Which column of the task track it sits in, among the tasks it overlaps.
    let lane: Int
    /// How many columns that cluster of overlapping tasks needs.
    let laneCount: Int

    var id: EntityRef { task.id }
}

/// How a day column shares its width between blocks and timed tasks.
///
/// A task is not a block and is not laid out among them: a column that has a
/// timed task, or a plan that moves one onto it, gives the trailing part of
/// its width to a task track, and the blocks keep the rest. A column with
/// neither gives the blocks all of it, as before the grid drew tasks.
struct TaskTrack: Equatable {
    let width: CGFloat
    let shown: Bool

    /// The share of the column the task track takes when it is shown.
    static let share: CGFloat = 0.4

    var blockWidth: CGFloat { shown ? width * (1 - Self.share) : width }
    /// Where the task track starts, in points from the column's leading edge.
    var taskX: CGFloat { blockWidth }
    var taskWidth: CGFloat { width - blockWidth }

    /// A ghost is drawn in the track of what it moves.
    func x(of ghost: PlanGhost) -> CGFloat { ghost.isTask ? taskX : 0 }
    func width(of ghost: PlanGhost) -> CGFloat { ghost.isTask ? taskWidth : blockWidth }
}

/// A shaded conflict region, placed on the grid.
struct PlacedConflict: Equatable, Identifiable {
    let conflict: BlockConflict
    /// Stable across a refresh: a conflict is the pair, whichever order the
    /// grid happens to hold them in.
    var id: String { "\(conflict.a)|\(conflict.b)" }
}

/// A block with its start and end already resolved to instants, before lanes
/// are assigned. Named rather than a tuple because it is passed between three
/// functions and a positional `.0` at the third one is how a start and an end
/// get swapped.
private struct Resolved<Item> {
    let item: Item
    /// Breaks a tie between two items that start together, so the same rows
    /// always take the same lanes.
    let key: String
    let start: Int64
    let end: Int64
}

/// Where blocks go on a time grid, and how wide they are.
///
/// Pure and synchronous, so the interesting decisions — that overlapping
/// blocks share the width rather than hiding each other, and that a block
/// running past midnight is clamped to the day it is drawn on rather than
/// dropped — are testable without a vault, a window, or a clock.
enum BlockLayout {
    /// Place `rows` on a grid covering `[fromMs, toMs)`.
    ///
    /// Blocks are clamped to the window. `Query::DayBlocks` returns anything
    /// *overlapping* the day, so a block that started yesterday evening and
    /// runs into this morning arrives here with a start before the window —
    /// and must be drawn from the top of the day, not off the top of the view.
    static func place(
        rows: [BlockGridRow],
        fromMs: Int64,
        toMs: Int64,
        timeZone: String
    ) -> [PlacedBlock] {
        let resolved = rows.map { row in
            Resolved(
                item: row,
                key: row.gridKey,
                start: timeValueMs(value: row.block.startsAt, tz: timeZone),
                end: timeValueMs(value: row.block.endsAt, tz: timeZone)
            )
        }
        return pack(clamp(resolved, fromMs: fromMs, toMs: toMs)) { item, lane, count in
            PlacedBlock(row: item.item, startMs: item.start, endMs: item.end, lane: lane, laneCount: count)
        }
    }

    /// Place the timed tasks among `tasks` on a column covering
    /// `[fromMs, toMs)`, packed into lanes among themselves as blocks are.
    ///
    /// A task with no time (no `planned_at`, or an all-day one) has no place
    /// on a time grid and is left out. Its length is its estimate, or
    /// `defaultDurationS` — the `planner.default_task_duration_s` preference,
    /// the same slot the planner moves it in.
    static func placeTasks(
        _ tasks: [TaskItem],
        defaultDurationS: UInt64,
        fromMs: Int64,
        toMs: Int64,
        timeZone: String
    ) -> [PlacedTask] {
        let resolved = tasks.compactMap { task -> Resolved<TaskItem>? in
            guard let start = timedStartMs(of: task, timeZone: timeZone) else { return nil }
            let seconds = task.estimatedDurationS.flatMap { $0 > 0 ? $0 : nil } ?? defaultDurationS
            // Capped as the planner caps the slot it moves the task in; a
            // column is a day, and the part on it is clamped to it below.
            let lengthMs = Int64(min(max(1, seconds), maxTaskLengthS)) * 1000
            return Resolved(item: task, key: task.id, start: start, end: start + lengthMs)
        }
        return pack(clamp(resolved, fromMs: fromMs, toMs: toMs)) { item, lane, count in
            PlacedTask(task: item.item, startMs: item.start, endMs: item.end, lane: lane, laneCount: count)
        }
    }

    /// The longest a task is drawn: seven days, the cap the planner puts on
    /// a task's slot (`crates/sunrise-domain/src/planner/world.rs`), so the
    /// grid draws the span a planned move works with. The grid reads tasks
    /// this far back for the same reason (`CalendarModel.timedTasks()`).
    static let maxTaskLengthS: UInt64 = 7 * 86_400

    /// Every task an Upcoming report plans for a time of day, once each: a
    /// task is listed under one day, but the report's days and the grid's
    /// need not be the same days.
    static func timedTasks(in report: UpcomingReport, timeZone: String) -> [TaskItem] {
        var seen = Set<EntityRef>()
        return report.days
            .flatMap(\.planned)
            .filter { timedStartMs(of: $0, timeZone: timeZone) != nil && seen.insert($0.id).inserted }
    }

    /// When `task` is planned for, as an instant, when that is a time of day:
    /// `nil` for no plan, an all-day plan, or a kind this build cannot read.
    static func timedStartMs(of task: TaskItem, timeZone: String) -> Int64? {
        switch task.scheduledAt {
        case nil, .allDay?, .unknown?: nil
        case let value?: timeValueMs(value: value, tz: timeZone)
        }
    }

    /// The items overlapping `[fromMs, toMs)`, clamped to it, in start order.
    ///
    /// Half-open: an item that ends exactly when the window starts is not on
    /// it.
    private static func clamp<Item>(
        _ items: [Resolved<Item>],
        fromMs: Int64,
        toMs: Int64
    ) -> [Resolved<Item>] {
        items
            .compactMap { item in
                guard item.end > fromMs, item.start < toMs else { return nil }
                return Resolved(
                    item: item.item, key: item.key, start: max(item.start, fromMs), end: min(item.end, toMs)
                )
            }
            .sorted { lhs, rhs in
                lhs.start == rhs.start ? lhs.key < rhs.key : lhs.start < rhs.start
            }
    }

    /// Each item, made into `Placed` with its lane and how many lanes its
    /// cluster needs.
    private static func pack<Item, Placed>(
        _ resolved: [Resolved<Item>],
        _ make: (Resolved<Item>, Int, Int) -> Placed
    ) -> [Placed] {
        var placed: [Placed] = []
        for cluster in clusters(of: resolved) {
            // Greedy column packing: an item takes the first column whose last
            // occupant has already finished. Items that do not overlap each
            // other therefore reuse a column, so three blocks where only two
            // ever overlap cost two columns rather than three.
            var columnEnds: [Int64] = []
            var lanes: [Int] = []
            for item in cluster {
                if let free = columnEnds.firstIndex(where: { $0 <= item.start }) {
                    columnEnds[free] = item.end
                    lanes.append(free)
                } else {
                    columnEnds.append(item.end)
                    lanes.append(columnEnds.count - 1)
                }
            }
            for (item, lane) in zip(cluster, lanes) {
                placed.append(make(item, lane, columnEnds.count))
            }
        }
        return placed
    }

    /// Split into runs of blocks that transitively overlap.
    ///
    /// Transitively, not pairwise: A overlaps B and B overlaps C means all
    /// three share the width, even when A and C do not touch. Sizing each pair
    /// independently would give A and C the same column and draw them on top
    /// of one another.
    private static func clusters<Item>(of items: [Resolved<Item>]) -> [[Resolved<Item>]] {
        var out: [[Resolved<Item>]] = []
        var current: [Resolved<Item>] = []
        var clusterEnd = Int64.min
        for item in items {
            if current.isEmpty || item.start < clusterEnd {
                current.append(item)
                clusterEnd = max(clusterEnd, item.end)
            } else {
                out.append(current)
                current = [item]
                clusterEnd = item.end
            }
        }
        if !current.isEmpty { out.append(current) }
        return out
    }

    /// The conflicts that fall inside `[fromMs, toMs)`, clamped to it.
    ///
    /// The conflicts themselves come from `blockConflicts`, which is the
    /// domain's answer to what overlapping means — including that back-to-back
    /// blocks do not.
    static func shade(
        conflicts: [BlockConflict],
        fromMs: Int64,
        toMs: Int64
    ) -> [PlacedConflict] {
        conflicts
            .filter { $0.toMs > fromMs && $0.fromMs < toMs }
            .map { PlacedConflict(conflict: $0) }
    }

    /// Snap `ms` to the nearest `minutes` boundary within the day starting at
    /// `dayStartMs`.
    ///
    /// `docs/07-clients/interaction-patterns.md` §Drag-and-drop UX tokens sets
    /// the default at 15 minutes and the choices at {5, 10, 15, 30, 60}.
    /// Snapping relative to the day's start rather than to the epoch is what
    /// makes a half-hour zone (India, Newfoundland) snap to :00 and :15 on
    /// screen instead of :00 and :45.
    static func snap(ms: Int64, toMinutes minutes: Int, dayStartMs: Int64) -> Int64 {
        let step = Int64(max(1, minutes)) * 60_000
        let offset = ms - dayStartMs
        let snapped = (offset.quotientAndRemainder(dividingBy: step).quotient * step)
            + (offset % step >= step / 2 ? step : 0)
        return dayStartMs + snapped
    }
}

/// Which end of a block a drag is holding.
enum BlockDragMode: Equatable, Sendable {
    /// Both ends together — the block keeps its length and changes when it is.
    case move
    /// The bottom edge only — the block keeps its start and changes how long
    /// it is.
    case resizeEnd
}

/// A stretch of one day column, epoch ms, half-open.
struct GridSpan: Equatable, Sendable {
    let startMs: Int64
    let endMs: Int64
}

/// One item a previewed plan moves, as a day column draws it: a ghost where
/// it goes and a faint outline where it was (`docs/08-features/planner.md`
/// §Client contract).
///
/// Every position here is the core's. The client resolves the diff's
/// `TimeValue`s to instants through `timeValueMs`, the same seam the grid
/// draws blocks with, and clamps them to the column; it decides nothing.
struct PlanGhost: Equatable, Identifiable {
    let item: EntityRef
    let role: MoveRole
    /// Where it was, on this column; `nil` when that was another day, a
    /// day with no time, or nowhere.
    let before: GridSpan?
    /// Where it goes, on this column; `nil` likewise.
    let after: GridSpan?
    /// The worst violation naming it after the move, if any.
    let severity: PlanSeverity?

    var id: EntityRef { item }

    /// Whether the item is a task, drawn in the task track, rather than a
    /// block.
    var isTask: Bool { !DropPayload.taskIDs([item]).isEmpty }
}

/// A previewed plan, laid out for the grid and put into words.
enum PlanGhosts {
    /// The ghosts of `diff` that touch the column `[fromMs, toMs)`: each
    /// move whose old or new slot overlaps it.
    static func place(
        diff: PlanDiffItem,
        fromMs: Int64,
        toMs: Int64,
        timeZone: String
    ) -> [PlanGhost] {
        func onColumn(_ position: PlanPositionItem) -> GridSpan? {
            guard case let .slot(start, end) = position else { return nil }
            let from = timeValueMs(value: start, tz: timeZone)
            let to = timeValueMs(value: end, tz: timeZone)
            guard to > fromMs, from < toMs else { return nil }
            return GridSpan(startMs: max(from, fromMs), endMs: min(to, toMs))
        }
        return diff.moves.compactMap { move in
            let before = onColumn(move.before)
            let after = onColumn(move.after)
            guard before != nil || after != nil else { return nil }
            return PlanGhost(
                item: move.item,
                role: move.role,
                before: before,
                after: after,
                severity: severity(of: move.item, in: diff)
            )
        }
    }

    /// The worst violation on `item` after the diff. The core orders
    /// violations most severe first, so the first one naming it is the one.
    static func severity(of item: EntityRef, in diff: PlanDiffItem) -> PlanSeverity? {
        diff.violations.first { $0.item == item }?.severity
    }

    /// The line under a drag: "Moves 3 · 1 unscheduled · misses its hard
    /// deadline", or that the drop would be refused. The rule named is the
    /// worst one the drop introduces; one already there before it is not
    /// news about this drop.
    static func summary(of diff: PlanDiffItem) -> String {
        guard diff.committable else { return L10n.Calendar.planRefused }
        var parts = [L10n.Calendar.planMoves(count: diff.moves.count)]
        if !diff.unscheduled.isEmpty {
            parts.append(L10n.Calendar.planUnscheduled(count: diff.unscheduled.count))
        }
        if let worst = diff.violations.first(where: { $0.introduced && $0.severity != .info }) {
            parts.append(words(for: worst.rule))
        }
        return parts.joined(separator: " · ")
    }

    /// A rule, as the summary line says it. A table rather than a `switch`
    /// so each rule is one row; a rule a newer core adds and this build has
    /// no words for reads as "not a time this block can have".
    static func words(for rule: PlanRule) -> String {
        let words: [PlanRule: String] = [
            .overlapsFixed: L10n.Calendar.planRuleOverlapsFixed,
            .overlaps: L10n.Calendar.planRuleOverlaps,
            .afterHardDeadline: L10n.Calendar.planRuleAfterHardDeadline,
            .afterTarget: L10n.Calendar.planRuleAfterTarget,
            .beforeBlocker: L10n.Calendar.planRuleBeforeBlocker,
            .outsideTimeOfDay: L10n.Calendar.planRuleOutsideTimeOfDay,
            .wrongDayOfWeek: L10n.Calendar.planRuleWrongDayOfWeek,
            .outsideDateRange: L10n.Calendar.planRuleOutsideDateRange,
            .outsideDay: L10n.Calendar.planRuleOutsideDay,
            .inPast: L10n.Calendar.planRuleInPast
        ]
        return words[rule] ?? L10n.Calendar.planRuleInvalid
    }
}

/// The one-in-flight rule for previews (`docs/08-features/planner.md`
/// §Client contract): at most one `plan_preview` call runs at a time, a newer
/// position replaces the one queued behind it, and a result for a position
/// that has since been superseded is dropped rather than drawn.
///
/// Pure, so the rule is tested without a vault or a clock. The caller sends
/// what ``request(_:)`` and ``finished()`` hand back, and draws a result only
/// when ``finished()`` says it is current.
struct PlanPreviewPump<Intent> {
    private(set) var inFlight = false
    private(set) var queued: Intent?

    /// A new snapped position. Returns the intent to send now, or `nil` when
    /// one is already in flight, in which case this one waits behind it in
    /// place of any older one.
    mutating func request(_ intent: Intent) -> Intent? {
        guard inFlight else {
            inFlight = true
            return intent
        }
        queued = intent
        return nil
    }

    /// The call in flight returned. `current` is whether its result is still
    /// the newest position (nothing queued behind it); `next` is the queued
    /// position to send now, if there is one.
    mutating func finished() -> (current: Bool, next: Intent?) {
        guard let next = queued else {
            inFlight = false
            return (true, nil)
        }
        queued = nil
        return (false, next)
    }

    /// The gesture ended. Anything queued is dropped; a call still in flight
    /// finishes, and its result is not drawn, because the caller's gesture
    /// generation moved on.
    mutating func reset() {
        queued = nil
    }
}
