import Foundation

/// Which way a movement key goes.
enum SelectionMove: Hashable, Sendable {
    case up
    case down
}

/// Which end of a list a jump goes to.
enum SelectionEdge: Hashable, Sendable {
    case first
    case last
}

/// The keyboard's place in a list of tasks.
///
/// Three pieces of state, and keeping them apart is the whole design:
///
/// - the **cursor** is the one row the keyboard is on. It always exists while
///   the list is non-empty, because every row-scoped key needs a subject.
/// - the **explicit** set is what `Space` has ticked. Empty is the normal case,
///   and it is *not* the same as "nothing is selected" — see ``targets``.
/// - the **anchor** is where a `Shift+↑/↓` range grows from. Reset by every
///   plain move, which is what makes a range restart at the row you walked to
///   rather than at the one you started the session on.
///
/// Deliberately free of SwiftUI. A list that moves, selects a range and acts on
/// what it selected is exactly the logic that is impossible to test through a
/// view body, and `docs/07-clients/parity-matrix.md` marks keyboard navigation
/// a MUST — so it is here, where a test can drive it.
@MainActor
@Observable
final class ListSelection {
    /// The rows on screen, in the order they are drawn.
    private(set) var ids: [EntityRef] = []
    private(set) var cursor: EntityRef?
    private(set) var explicit: Set<EntityRef> = []
    private(set) var anchor: EntityRef?

    init() {}

    /// What a row action applies to, in list order.
    ///
    /// The explicit set when there is one, and the cursor alone when there is
    /// not. That fallback is the reason `X` works the moment you arrow onto a
    /// row: requiring a `Space` first would make every single-task completion
    /// two keys instead of one.
    var targets: [EntityRef] {
        guard !explicit.isEmpty else { return cursor.map { [$0] } ?? [] }
        return ids.filter(explicit.contains)
    }

    var isEmpty: Bool { targets.isEmpty }

    /// Whether more than one row would be acted on — what a confirmation
    /// prompt and a plural label read.
    var isMultiple: Bool { targets.count > 1 }

    /// Whether a row draws as selected. The cursor counts even when nothing has
    /// been ticked, so the keyboard's position is always visible — which
    /// `docs/10-cross-cutting/accessibility.md` requires of every focusable
    /// thing, not just of buttons.
    func isSelected(_ id: EntityRef) -> Bool {
        explicit.isEmpty ? id == cursor : explicit.contains(id)
    }

    // MARK: - The list changed underneath us

    /// Take a new set of rows, keeping the cursor where a human would expect.
    ///
    /// The interesting case is a row that *left*: complete the task under the
    /// cursor in a Today list and it drops out of the query. Clearing the
    /// cursor there would mean pressing `X` four times took four presses and
    /// three arrow keys; landing on whatever took its index means it takes
    /// four presses. Ids that no longer exist are dropped from the explicit
    /// set rather than kept, because acting on them would be acting on nothing.
    func reconcile(with rows: [EntityRef]) {
        let previousIndex = cursor.flatMap { ids.firstIndex(of: $0) }
        ids = rows
        explicit.formIntersection(rows)
        if let anchor, !rows.contains(anchor) { self.anchor = nil }
        guard !rows.isEmpty else {
            cursor = nil
            anchor = nil
            explicit = []
            return
        }
        if let cursor, rows.contains(cursor) { return }
        guard let previousIndex else {
            cursor = nil
            return
        }
        cursor = rows[min(previousIndex, rows.count - 1)]
    }

    // MARK: - Moving

    /// Move the cursor one row, dropping any range or tick.
    ///
    /// With no cursor yet, an arrow key adopts the near end of the list rather
    /// than doing nothing: pressing ↓ into a freshly drawn list has to land
    /// somewhere, and the top is where a reader already is.
    ///
    /// At the end of the list the cursor stays put but the range still
    /// collapses onto it, which is what an unmodified arrow means everywhere
    /// else on this platform: "just this one".
    func move(_ direction: SelectionMove) {
        guard let next = index(after: direction) else {
            explicit = []
            anchor = cursor
            return
        }
        cursor = ids[next]
        explicit = []
        anchor = ids[next]
    }

    func moveToEdge(_ edge: SelectionEdge) {
        guard !ids.isEmpty else { return }
        let target = edge == .first ? ids[0] : ids[ids.count - 1]
        cursor = target
        explicit = []
        anchor = target
    }

    /// Grow (or shrink) the range from the anchor to one row further on.
    ///
    /// The range is recomputed from the anchor every time rather than
    /// accumulated, so reversing direction *un*-selects — which is what every
    /// other macOS list does, and the reason `Shift+↓ ↓ ↑` leaves two rows
    /// selected rather than three.
    func extend(_ direction: SelectionMove) {
        guard let next = index(after: direction) else { return }
        let anchorID = anchor ?? cursor ?? ids[next]
        anchor = anchorID
        cursor = ids[next]
        guard let start = ids.firstIndex(of: anchorID) else {
            explicit = [ids[next]]
            return
        }
        let range = start <= next ? start...next : next...start
        explicit = Set(ids[range])
    }

    /// `Space`: tick or untick the row under the cursor.
    ///
    /// The first tick has to add the cursor's own row, not merely start an
    /// empty set — otherwise `Space` on a row you were about to complete would
    /// *deselect* it, since ``targets`` had been answering with the cursor all
    /// along.
    func toggleAtCursor() {
        guard let cursor else { return }
        if explicit.contains(cursor) {
            explicit.remove(cursor)
        } else {
            explicit.insert(cursor)
        }
        anchor = cursor
    }

    /// A click, or a programmatic reveal: one row, nothing ticked.
    func focus(_ id: EntityRef) {
        guard ids.contains(id) else { return }
        cursor = id
        explicit = []
        anchor = id
    }

    /// Adopt a selection made by the list itself — a click, a shift-click, a
    /// drag. The cursor follows the last row the set gained so that a
    /// subsequent `Shift+↓` grows from where the mouse left off.
    func adopt(_ selected: Set<EntityRef>) {
        explicit = selected.intersection(ids)
        guard !explicit.isEmpty else { return }
        if let cursor, explicit.contains(cursor) { return }
        cursor = ids.first(where: explicit.contains)
        anchor = cursor
    }

    func selectAll() {
        explicit = Set(ids)
        anchor = ids.first
        if cursor == nil { cursor = ids.first }
    }

    /// Drop the ticks but keep the cursor. `Esc` in a list with a range on it
    /// means "never mind", not "and lose my place".
    func clearSelection() {
        explicit = []
        anchor = cursor
    }

    private func index(after direction: SelectionMove) -> Int? {
        guard !ids.isEmpty else { return nil }
        guard let cursor, let current = ids.firstIndex(of: cursor) else {
            return direction == .down ? 0 : ids.count - 1
        }
        switch direction {
        case .up: return current == 0 ? nil : current - 1
        case .down: return current == ids.count - 1 ? nil : current + 1
        }
    }
}
