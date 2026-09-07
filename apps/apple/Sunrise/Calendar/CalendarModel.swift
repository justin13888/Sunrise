import Foundation

/// Day or week. The two queries behind them differ, so this is not a display
/// toggle over one result set.
enum CalendarSpan: String, CaseIterable, Identifiable {
    case day
    case week

    var id: Self { self }
    var title: String { self == .day ? "Day" : "Week" }
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
        case .zoned: "Local time here"
        case .floating: "Wherever I am"
        case .instant: "A fixed moment"
        }
    }

    var explanation: String {
        switch self {
        case .zoned: "09:00 in this timezone. Flying somewhere else moves it on your day."
        case .floating: "09:00 wherever you are. Flying somewhere else keeps it at 09:00."
        case .instant: "The same instant everywhere. Flying somewhere else changes the clock time."
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
    private(set) var errorMessage: String?
    /// Set after a Resolve action, so the grid can say what it did.
    private(set) var note: String?

    private let bridge: CoreBridge

    init(bridge: CoreBridge) {
        self.bridge = bridge
    }

    // MARK: - Reading

    func refresh() async {
        timeZone = TimeZone.current.identifier
        nowMs = await bridge.nowMs()
        if anchorMs == 0 { anchorMs = nowMs }
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
            // The domain's answer, not a comparison written here.
            conflicts = blockConflicts(rows: rows)
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
    /// A week starts on Monday, matching `Query::WeekBlocks`, which is
    /// Monday-first. Getting this wrong would draw the right blocks in the
    /// wrong columns — the sort of bug that looks like a sync failure.
    func dayStartMs(offset: Int) -> Int64 {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(identifier: timeZone) ?? .current
        calendar.firstWeekday = 2 // Monday
        let anchor = Date(timeIntervalSince1970: Double(anchorMs) / 1000)
        let base: Date = span == .day
            ? calendar.startOfDay(for: anchor)
            : calendar.dateInterval(of: .weekOfYear, for: anchor)?.start
                ?? calendar.startOfDay(for: anchor)
        let day = calendar.date(byAdding: .day, value: offset, to: base) ?? base
        return Int64(day.timeIntervalSince1970 * 1000)
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
        await run(.createBlock(draft: draft), label: "create a block")
    }

    /// Drop an existing task onto the grid.
    ///
    /// The block is created with the task already bound and **no title**, which
    /// is not an omission: `Command::CreateBlock` shadow-copies the single
    /// bound task's title as it is now, so the block gets the right name from
    /// the core rather than one composed here that could disagree with it.
    func dropTask(_ task: EntityRef, fromMs: Int64, toMs: Int64, kind: BlockTimeKind) async {
        await createBlock(fromMs: fromMs, toMs: toMs, title: nil, kind: kind, tasks: [task])
    }

    /// Move or resize a block by dragging it.
    ///
    /// The kind is preserved: dragging a floating block an hour later must not
    /// quietly pin it to a zone. That is why this reads the row's own kind back
    /// rather than taking one from the caller.
    func moveBlock(_ row: BlockGridRow, toStartMs: Int64, toEndMs: Int64) async {
        var edit = BlockEdit()
        edit.startsAt = sameKind(as: row.block.startsAt, ms: toStartMs)
        edit.endsAt = sameKind(as: row.block.endsAt, ms: toEndMs)
        await run(.updateBlock(id: row.block.id, edit: edit), label: "move a block")
    }

    func bind(task: EntityRef, to block: EntityRef) async {
        await run(.bindTask(block: block, task: task), label: "bind a task")
    }

    func unbind(task: EntityRef, from block: EntityRef) async {
        await run(.unbindTask(block: block, task: task), label: "unbind a task")
    }

    func delete(_ block: EntityRef) async {
        await run(.deleteBlock(id: block), label: "delete a block")
    }

    /// Apply an edit from the block editor.
    func apply(_ edit: BlockEdit, to block: EntityRef) async {
        await run(.updateBlock(id: block, edit: edit), label: "edit a block")
    }

    // MARK: - Resolving a conflict

    /// **Keep both** — the documented no-op. It exists so the menu has three
    /// entries and the user can say "yes, on purpose"; nothing is written.
    func keepBoth() {
        note = "Both blocks kept."
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
    func merge(_ conflict: BlockConflict) async {
        guard let first = row(conflict.a), let second = row(conflict.b) else { return }
        do {
            let draft = try mergedBlockDraft(a: first, b: second, tz: timeZone)
            for id in [first.block.id, second.block.id] {
                _ = try await bridge.submit(.deleteBlock(id: id))
            }
            _ = try await bridge.submit(.createBlock(draft: draft))
            note = "Merged into one block."
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
    /// already had.
    private func sameKind(as existing: TimeValue, ms: Int64) -> TimeValue {
        switch existing {
        case .instant: .instant(at: ms)
        case let .zoned(_, tz): .zoned(civil: civilText(ms: ms, tz: tz), tz: tz)
        case .floating: .floating(civil: civilText(ms: ms))
        // An all-day block dragged to a time is no longer all-day: the user
        // just gave it one. Widening to zoned is the honest reading of that.
        case .allDay: .zoned(civil: civilText(ms: ms), tz: timeZone)
        }
    }

    /// `2026-03-04T09:15:00`, the text `jiff` parses a civil date-time from.
    private func civilText(ms: Int64, tz: String? = nil) -> String {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(identifier: tz ?? timeZone) ?? .current
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
