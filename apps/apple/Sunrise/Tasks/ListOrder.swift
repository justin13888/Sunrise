import Foundation

/// What a drag is carrying.
///
/// Every draggable surface in this app puts an `EntityRef`'s **own text** on
/// the pasteboard — the same string the CLI accepts — rather than a private
/// pasteboard type, so a drop target can verify what it was handed instead of
/// guessing. This is the verifying.
enum DropPayload {
    /// The task ids in a drop, in the order they arrived. Anything else is
    /// dropped rather than guessed at.
    static func taskIDs(_ items: [String]) -> [EntityRef] {
        items.filter { $0.hasPrefix("tsk_") }
    }

    /// The block ids in a drop.
    static func blockIDs(_ items: [String]) -> [EntityRef] {
        items.filter { $0.hasPrefix("blk_") }
    }
}

/// A drag within a task list, as the one `MoveTasks` command it is.
///
/// **A task's place in a list is a vault fact, and it syncs.** A Task carries
/// `sort_order`, a fractional index (`docs/02-domain/tasks.md` §Fields), and
/// `CoreCommand.moveTasks` is the only thing that writes it. The
/// core owns all of the arithmetic: the keys, and which neighbouring rows have
/// to move with the dragged ones so the drop can land where it was aimed
/// (`plan_task_move`). The Inbox, Stream and Context queries come back in
/// that order already, so there is nothing to lay over them here.
///
/// What is left is reading a SwiftUI drop: which rows, and which row they fell
/// on. ``StreamOrder`` is the same adapter for the sidebar's streams.
enum ListOrder {
    /// The command that puts `moved` immediately above `target` in `list`,
    /// the rows as the core returned them, or `nil` when the drop changes
    /// nothing.
    ///
    /// A multi-row drag keeps its rows in the order they are in `list`, which
    /// is what makes dragging a ⇧-selected run feel like moving one thing.
    /// Dropping onto a row that is itself being dragged, or naming rows the
    /// list does not hold, is a no-op rather than a guess.
    static func move(
        _ moved: [EntityRef],
        before target: EntityRef,
        in list: [TaskItem]
    ) -> CoreCommand? {
        guard let move = planTaskMove(list: list, moved: moved, to: .before(id: target)) else {
            return nil
        }
        return .moveTasks(ids: move.ids, after: move.after, before: move.before)
    }
}

/// The one-time import of the order this device kept for itself before a
/// Task had one.
///
/// Before `Task.sort_order` existed, a hand-arranged task order was a list of
/// ids per list in `UserDefaults`, under `list.order.*`, and it never left the
/// device. This reads what an earlier build left there, writes it once into
/// the vault through the core's `import_task_list_order` (which declines when
/// the list already has a synced order, because that one is newer), and then
/// deletes the key. Nothing writes these keys any more, so a device that has
/// opened each list it ever arranged holds none of them.
///
/// Per list, on the first refresh of that list, rather than all at once at
/// launch: the import needs the list as the core shows it now, and the refresh
/// has just read exactly that.
@MainActor
struct ListOrderImport {
    private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    /// The ids an earlier build stored for this list, or `nil` when there is
    /// nothing left to import.
    func stored(for kind: TaskListKind) -> [EntityRef]? {
        guard let key = kind.legacyOrderKey else { return nil }
        return defaults.stringArray(forKey: key)
    }

    /// The command that writes the stored order into `list`, or `nil` when
    /// there is nothing to write: no stored order, none of its rows still in
    /// the list, or a list another device has already arranged.
    func command(for kind: TaskListKind, in list: [TaskItem]) -> CoreCommand? {
        guard let stored = stored(for: kind),
              let move = importTaskListOrder(stored: stored, list: list) else { return nil }
        return .moveTasks(ids: move.ids, after: move.after, before: move.before)
    }

    /// Delete the stored order, once it has been written or found to have
    /// nothing to write.
    func forget(_ kind: TaskListKind) {
        guard let key = kind.legacyOrderKey else { return }
        defaults.removeObject(forKey: key)
    }
}

extension TaskListKind {
    /// Whether rows in this list can be dragged into a hand-made order.
    ///
    /// Today and Search cannot, and both refusals are the point rather than
    /// an omission. Today is sectioned by *urgency* — the core decides which
    /// rows are overdue and which are later — so a row dragged across a
    /// section heading would either snap back on the next refresh or claim
    /// that a deadline had moved. Search is ranked by relevance to a query
    /// that changes on every keystroke, and an arrangement keyed to one query
    /// is an arrangement nobody would ever see twice.
    var acceptsReordering: Bool {
        switch self {
        case .today, .search: false
        case .inbox, .stream, .context: true
        }
    }

    /// Where an earlier build kept this list's order in `UserDefaults`, read
    /// only by ``ListOrderImport``; `nil` for a list that never had one.
    var legacyOrderKey: String? {
        switch self {
        case .today, .search: nil
        case .inbox: "list.order.inbox"
        case let .stream(id, _): "list.order.stream.\(id)"
        case let .context(id, _): "list.order.context.\(id)"
        }
    }
}
