import Foundation

/// How far a change to one event of a repeating block reaches.
///
/// `docs/02-domain/time-blocks.md` §Lifecycle: a move, edit or delete of an
/// occurrence asks *this event*, *this and following events* or *all
/// events*, and each answer is one `EditScopeIn`. A block that does not
/// recur asks nothing, which is why a change carries a reach only when the
/// row it starts from has an occurrence key.
enum BlockEditReach: String, CaseIterable, Identifiable {
    case thisEvent
    case thisAndFollowing
    case allEvents

    var id: Self { self }

    var title: String {
        switch self {
        case .thisEvent: L10n.Calendar.reachThis
        case .thisAndFollowing: L10n.Calendar.reachThisAndFollowing
        case .allEvents: L10n.Calendar.reachAll
        }
    }

    /// The scope this answer sends for the occurrence named `key`.
    func scope(at key: String) -> EditScopeIn {
        switch self {
        case .thisEvent: .this(key: key)
        case .thisAndFollowing: .thisAndFuture(key: key)
        case .allEvents: .all
        }
    }
}

/// Which change is waiting on the reach question, for its heading.
enum BlockReachAction: Equatable {
    case move
    case edit
    case delete

    var title: String {
        switch self {
        case .move: L10n.Calendar.reachTitleMove
        case .edit: L10n.Calendar.reachTitleEdit
        case .delete: L10n.Calendar.reachTitleDelete
        }
    }
}

/// A change to one event of a repeating block, held until the person says
/// how far it reaches. `commit` runs with the answer; dismissing the question
/// drops the change, so nothing is written on a guess.
struct PendingBlockReach: Identifiable {
    let id = UUID()
    let action: BlockReachAction
    let commit: (BlockEditReach) -> Void
}

extension BlockGridRow {
    /// Which row of the grid this is: the block, and for one occurrence of a
    /// repeating block, which occurrence. A weekly block on a week grid is
    /// seven rows with one block id, so the id alone names none of them.
    /// A block that does not recur keeps its id as its key.
    var gridKey: String {
        guard let occurrenceKey else { return block.id }
        return "\(block.id)@\(occurrenceKey)"
    }

    /// One event of a repeating block, so a change to it asks how far it
    /// reaches.
    var recurs: Bool { occurrenceKey != nil }

    /// Planning may not move it. The core sends the effective kind, so any
    /// spelling but `fixed` is the flexible default.
    var isFixed: Bool { block.kind == "fixed" }
}

extension BlockEdit {
    /// The edit sets a start or an end.
    var movesTimes: Bool { startsAt != nil || endsAt != nil }
}

/// The commands a change to a block sends, by how far it reaches.
extension CalendarModel {
    /// The command an edit of `row` sends, or `nil` when it must send none.
    ///
    /// A block that does not recur sends no scope: the whole block, as before
    /// there were repeating ones. One event of a repeating block sends the
    /// scope `reach` names, and sends nothing without one, so a change nobody
    /// answered the question for is never applied to a guessed reach.
    ///
    /// The times on an occurrence's row are that occurrence's. *This event*
    /// and *this and following* take them as they are: the core writes them
    /// into the occurrence's exception, or starts the continuing series at
    /// them. *All events* moves the series' anchor, which `anchor` holds, by
    /// as much as the occurrence moved; sending the occurrence's own times
    /// would instead re-anchor the series on that day and drop every event
    /// before it. Each bound keeps the kind the edit gives it.
    ///
    /// Pure, so the scope each answer sends is testable without a vault —
    /// and no command yet creates a repeating block for a vault to hold.
    static func updateCommand(
        row: BlockGridRow,
        edit: BlockEdit,
        reach: BlockEditReach?,
        anchor: BlockItem?,
        timeZone: String
    ) -> CoreCommand? {
        guard let key = row.occurrenceKey else {
            return .updateBlock(id: row.block.id, edit: edit)
        }
        guard let reach else { return nil }
        var scoped = edit
        scoped.scope = reach.scope(at: key)
        if reach == .allEvents, edit.movesTimes {
            // Without the anchor there is nothing correct to send.
            guard let anchor else { return nil }
            if let start = edit.startsAt {
                scoped.startsAt = shifted(
                    start, from: row.block.startsAt, onto: anchor.startsAt, timeZone: timeZone
                )
            }
            if let end = edit.endsAt {
                scoped.endsAt = shifted(
                    end, from: row.block.endsAt, onto: anchor.endsAt, timeZone: timeZone
                )
            }
        }
        return .updateBlock(id: row.block.id, edit: scoped)
    }

    /// The command a delete of `row` sends, or `nil` when it must send none:
    /// one event of a repeating block with no answer to "which events?".
    static func deleteCommand(row: BlockGridRow, reach: BlockEditReach?) -> CoreCommand? {
        guard let key = row.occurrenceKey else {
            return .deleteBlock(id: row.block.id, scope: nil)
        }
        guard let reach else { return nil }
        return .deleteBlock(id: row.block.id, scope: reach.scope(at: key))
    }

    /// Whether `placed` is part of `conflict`: its block is one of the two,
    /// and the overlap falls on this event of it. Every event of a repeating
    /// block shares its id, so the id alone would mark them all.
    static func isPart(_ placed: PlacedBlock, of conflict: BlockConflict) -> Bool {
        (conflict.a == placed.blockId || conflict.b == placed.blockId)
            && conflict.fromMs < placed.endMs && conflict.toMs > placed.startMs
    }

    /// `value`, a new bound for an occurrence whose bound was `was`, carried
    /// over to the series anchor's bound `anchor`: the anchor moves by the
    /// same number of milliseconds, in `value`'s kind.
    private static func shifted(
        _ value: TimeValue,
        from was: TimeValue,
        onto anchor: TimeValue,
        timeZone: String
    ) -> TimeValue {
        let delta = timeValueMs(value: value, tz: timeZone) - timeValueMs(value: was, tz: timeZone)
        let moved = timeValueMs(value: anchor, tz: timeZone) + delta
        return sameKind(as: value, ms: moved, timeZone: timeZone)
    }

    // MARK: - Time values

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

    /// `2026-03-04T09:15:00`, the text `jiff` parses a civil date-time from.
    static func civilText(ms: Int64, tz: String) -> String {
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
}
