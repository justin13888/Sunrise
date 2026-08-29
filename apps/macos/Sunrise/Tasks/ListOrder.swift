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

/// Moving rows around in a list, as arithmetic on ids.
///
/// Pure and separate from the store, because the interesting question — where
/// a row lands when you drop it on another one, and what happens to rows the
/// order has never seen — is exactly the part that is invisible on screen
/// until it is wrong.
enum ListOrder {
    /// Move `moved` so that it sits immediately before `target`.
    ///
    /// A multi-row drag keeps the rows in the order they were in, which is
    /// what makes dragging a ⇧-selected run of five feel like moving one
    /// thing. Ids not in `ids` are ignored; dropping onto a row that is itself
    /// being moved is a no-op rather than a rearrangement of the selection.
    static func moving(
        _ moved: [EntityRef],
        before target: EntityRef?,
        in ids: [EntityRef]
    ) -> [EntityRef] {
        let moving = ids.filter { moved.contains($0) }
        guard !moving.isEmpty else { return ids }
        guard let target, !moving.contains(target) else { return ids }
        var result = ids.filter { !moving.contains($0) }
        guard let index = result.firstIndex(of: target) else { return ids }
        result.insert(contentsOf: moving, at: index)
        return result
    }

    /// Lay a remembered order over a fresh query result.
    ///
    /// Rows the order knows about come first, in the order it remembers.
    /// Rows it has never seen — captured since, arrived by sync, or simply
    /// never dragged — follow in the order the core returned them.
    ///
    /// That is a deliberate choice and it is the one that keeps the list
    /// honest: a new task appearing in the middle of a hand-arranged run
    /// would look like the arrangement had been rewritten by somebody else.
    /// Rows the order remembers but the query no longer returns are dropped
    /// here rather than pruned from storage — the query is the truth about
    /// what exists, and a task that comes back from an undo should come back
    /// where it was.
    static func applying(_ stored: [EntityRef], to ids: [EntityRef]) -> [EntityRef] {
        guard !stored.isEmpty else { return ids }
        let present = Set(ids)
        let known = stored.filter { present.contains($0) }
        guard !known.isEmpty else { return ids }
        let placed = Set(known)
        return known + ids.filter { !placed.contains($0) }
    }
}

/// A hand-arranged row order, per list, on this device.
///
/// **Per device, and not by choice.** `docs/07-clients/interaction-patterns.md`
/// §Reorder asks for drag-within-a-list on every client, and the domain has no
/// facet to write it to: `TaskEdit` has no ordering field and neither does
/// `Task`. `docs/02-domain/streams.md` §Sort order models exactly this — a
/// fractional index, with a written warning that concurrent reorders do not
/// merge — but it models it for *Streams*, and `StreamEdit` does not carry
/// `sort_order` across the seam either. So there is no way from this client to
/// record an order the vault would keep.
///
/// `UserDefaults`, then, beside `AppSettings` and `NotificationPreferences`
/// and for the same reason those are there: it is a fact about this machine,
/// it does not sync, and a device that loses it loses an arrangement rather
/// than any of the user's data. When the seam grows an ordering field this
/// type is what gets pointed at it — `ListOrder` is already the whole of the
/// logic.
@MainActor
@Observable
final class ListOrderStore {
    /// How many ids are kept per list.
    ///
    /// A bound, because this is written on every drag and never garbage
    /// collected against the vault. Two hundred is far past the length of a
    /// list anybody hand-arranges, and the rows past it simply fall back to
    /// query order.
    static let limit = 200

    private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    /// The remembered order for a list, or empty when it has never been
    /// arranged.
    func order(for kind: TaskListKind) -> [EntityRef] {
        guard let key = kind.orderStorageKey else { return [] }
        return defaults.stringArray(forKey: key) ?? []
    }

    /// Apply what is remembered to a fresh query result.
    func apply(_ tasks: [TaskItem], for kind: TaskListKind) -> [TaskItem] {
        let stored = order(for: kind)
        guard !stored.isEmpty else { return tasks }
        let ranked = ListOrder.applying(stored, to: tasks.map(\.id))
        let byID = Dictionary(tasks.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        return ranked.compactMap { byID[$0] }
    }

    /// Record that `moved` was dropped onto `target` in a list currently
    /// showing `visible`.
    ///
    /// Returns `false` when nothing changed — a drop on a row already in that
    /// place, or on a list that cannot be arranged — so a caller can skip the
    /// re-read.
    @discardableResult
    func move(
        _ moved: [EntityRef],
        before target: EntityRef,
        in kind: TaskListKind,
        visible: [EntityRef]
    ) -> Bool {
        guard let key = kind.orderStorageKey else { return false }
        let next = ListOrder.moving(moved, before: target, in: visible)
        guard next != visible else { return false }
        defaults.set(Array(next.prefix(Self.limit)), forKey: key)
        return true
    }

    /// Forget a list's arrangement, back to whatever order the core returns.
    func reset(_ kind: TaskListKind) {
        guard let key = kind.orderStorageKey else { return }
        defaults.removeObject(forKey: key)
    }
}

extension TaskListKind {
    /// Where this list's hand-arranged order is kept, or `nil` when the list
    /// cannot be arranged by hand.
    ///
    /// Today and Search are `nil`, and both refusals are the point rather than
    /// an omission. Today is sectioned by *urgency* — the core decides which
    /// rows are overdue and which are later — so a row dragged across a
    /// section heading would either snap back on the next refresh or claim
    /// that a deadline had moved. Search is ranked by relevance to a query
    /// that changes on every keystroke, and an arrangement keyed to one query
    /// is an arrangement nobody would ever see twice.
    var orderStorageKey: String? {
        switch self {
        case .today, .search: nil
        case .inbox: "list.order.inbox"
        case let .stream(id, _): "list.order.stream.\(id)"
        case let .context(id, _): "list.order.context.\(id)"
        }
    }

    /// Whether rows in this list can be dragged into a hand-made order.
    var acceptsReordering: Bool { orderStorageKey != nil }
}
