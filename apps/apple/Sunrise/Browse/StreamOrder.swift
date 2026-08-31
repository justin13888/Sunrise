import Foundation

/// Turning a sidebar drag into `sort_order` writes.
///
/// **This is the vault's order, not this device's.** A Stream carries a
/// `sort_order` — a fractional index, `docs/02-domain/streams.md` §Sort order —
/// so a stream dragged here moves on every device the vault reaches. That is
/// the difference between this type and ``ListOrder``, which arranges *tasks*
/// and cannot sync because a Task has no ordering field to sync; the two are
/// side by side deliberately and the split is explained there.
///
/// Pure, and separate from the model, because the interesting part is
/// arithmetic on neighbours rather than anything to do with SwiftUI: a row
/// dropped into a list has to land strictly between the two rows it fell
/// between, and getting that wrong is invisible until somebody's sidebar
/// shuffles itself.
///
/// The keys themselves come from the core (`streamSortKeyBetween`), not from
/// here. Base-26 fraction arithmetic with a trailing-zero rule is exactly the
/// kind of thing that should exist once, in the place that has tests for it.
enum StreamOrder {
    /// One row's new position.
    struct Move: Equatable {
        /// The stream to write.
        let id: EntityRef
        /// Its new `sort_order`.
        let key: String
    }

    /// The writes that put `source` at `destination`, in the order to make
    /// them.
    ///
    /// `source` and `destination` are SwiftUI's `onMove` arguments, in terms
    /// of `rows` — which must be the *orderable* rows only. The Inbox is not a
    /// Stream, holds no key, and must never be one of these.
    ///
    /// Only the moved rows are written. Everything else keeps the key it had,
    /// which is the whole reason `sort_order` is a fractional index and not a
    /// position number: dragging one row costs one write no matter how long
    /// the list is.
    ///
    /// Empty when there is nothing to do, and also when a key could not be
    /// computed — bounds that admit nothing between them mean the list this
    /// was calculated against is stale, and the honest response is to write
    /// nothing and let the next refresh redraw.
    static func moves(
        moving source: IndexSet,
        to destination: Int,
        in rows: [StreamListRow],
        key: (String?, String?) -> String? = { streamSortKeyBetween(after: $0, before: $1) }
    ) -> [Move] {
        var placed = rows
        placed.move(fromOffsets: source, toOffset: destination)
        guard placed.map(\.id) != rows.map(\.id) else { return [] }

        let moved = Set(source.map { rows[$0].id })
        var out: [Move] = []
        // The key of the row above, which is either one just assigned or one
        // that was already there. `nil` at the top of the list.
        var above: String?
        for (index, row) in placed.enumerated() {
            guard moved.contains(row.id) else {
                above = row.sortOrder
                continue
            }
            // The row below, skipping any still-unkeyed row from this same
            // drag — a multi-row drag lands its rows one after another in the
            // gap, so they share a lower bound and take successive keys.
            let below = placed[(index + 1)...].first { !moved.contains($0.id) }?.sortOrder
            guard let next = key(above, below) else { return [] }
            out.append(Move(id: row.id, key: next))
            above = next
        }
        return out
    }
}
