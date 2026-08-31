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

    var id: EntityRef { row.block.id }
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
private struct Resolved {
    let row: BlockGridRow
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
        let resolved: [Resolved] = rows
            .compactMap { row in
                let start = timeValueMs(value: row.block.startsAt, tz: timeZone)
                let end = timeValueMs(value: row.block.endsAt, tz: timeZone)
                // Half-open: a block that ends exactly when the window starts
                // is not on it.
                guard end > fromMs, start < toMs else { return nil }
                return Resolved(row: row, start: max(start, fromMs), end: min(end, toMs))
            }
            .sorted { lhs, rhs in
                lhs.start == rhs.start
                    ? lhs.row.block.id < rhs.row.block.id
                    : lhs.start < rhs.start
            }

        var placed: [PlacedBlock] = []
        for cluster in clusters(of: resolved) {
            // Greedy column packing: a block takes the first column whose last
            // occupant has already finished. Blocks that do not overlap each
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
                placed.append(
                    PlacedBlock(
                        row: item.row,
                        startMs: item.start,
                        endMs: item.end,
                        lane: lane,
                        laneCount: columnEnds.count
                    )
                )
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
    private static func clusters(of items: [Resolved]) -> [[Resolved]] {
        var out: [[Resolved]] = []
        var current: [Resolved] = []
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

/// Dragging a block that already exists.
///
/// `docs/07-clients/parity-matrix.md` names "no drag to move or resize an
/// existing block" as one of this client's drag-and-drop gaps. This is the
/// arithmetic behind closing it, pure and separate from the gesture, because
/// the interesting parts — that a block keeps its length when it is moved, and
/// that neither end may be dragged out of the day it is drawn on — are
/// invisible on screen until they are wrong.
enum BlockDrag {
    /// Where a block ends up after being dragged `deltaMs` in `mode`.
    ///
    /// Both results are snapped, and both are clamped inside
    /// `[dayStartMs, dayEndMs]`: a block dragged off the bottom of a column
    /// must stop at midnight rather than silently become tomorrow's, which is
    /// what a raw translation would do on a grid that only draws one day.
    ///
    /// A resize never shortens a block past one snap unit — a zero-length
    /// block is one the core would refuse and the grid could not draw.
    static func apply(
        mode: BlockDragMode,
        startMs: Int64,
        endMs: Int64,
        deltaMs: Int64,
        snapMinutes: Int,
        dayStartMs: Int64,
        dayEndMs: Int64
    ) -> (startMs: Int64, endMs: Int64) {
        let step = Int64(max(1, snapMinutes)) * 60_000
        func snapped(_ ms: Int64) -> Int64 {
            BlockLayout.snap(ms: ms, toMinutes: snapMinutes, dayStartMs: dayStartMs)
        }
        switch mode {
        case .move:
            let length = max(step, endMs - startMs)
            // Clamped by the *start*, with the length preserved: clamping each
            // end on its own is how a block dragged past midnight arrives
            // three minutes long.
            let latest = max(dayStartMs, dayEndMs - length)
            let start = min(max(snapped(startMs + deltaMs), dayStartMs), latest)
            return (start, start + length)
        case .resizeEnd:
            let lowest = min(startMs + step, dayEndMs)
            let end = min(max(snapped(endMs + deltaMs), lowest), dayEndMs)
            return (startMs, end)
        }
    }
}
