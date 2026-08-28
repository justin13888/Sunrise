import Foundation
import Testing

@testable import Sunrise

/// Grid layout, without a vault, a window or a clock.
///
/// Everything here is a decision about what the user sees when two
/// commitments collide, which is exactly the case a manual pass is worst at
/// reproducing on purpose.
struct BlockLayoutTests {
    private static let tz = "UTC"
    /// 2026-03-04T00:00:00Z.
    private static let dayStart: Int64 = 1_772_582_400_000

    private func at(_ hour: Int, _ minute: Int = 0) -> Int64 {
        Self.dayStart + Int64(hour) * 3_600_000 + Int64(minute) * 60_000
    }

    private func row(_ id: String, _ from: Int64, _ to: Int64, title: String? = nil) -> BlockGridRow {
        BlockGridRow(
            block: BlockItem(
                id: "blk_\(id)",
                createdAt: 0,
                updatedAt: 0,
                streamId: inboxStreamId(),
                startsAt: .instant(at: from),
                endsAt: .instant(at: to),
                title: title,
                titleTrackTask: false,
                tasks: [],
                deleted: false
            ),
            title: title,
            taskTitles: []
        )
    }

    private func place(_ rows: [BlockGridRow]) -> [PlacedBlock] {
        BlockLayout.place(
            rows: rows,
            fromMs: Self.dayStart,
            toMs: Self.dayStart + 86_400_000,
            timeZone: Self.tz
        )
    }

    @Test
    func aBlockInsideTheDayKeepsItsOwnTimes() {
        let placed = place([row("a", at(9), at(10))])
        #expect(placed.count == 1)
        #expect(placed[0].startMs == at(9))
        #expect(placed[0].endMs == at(10))
        #expect(placed[0].laneCount == 1)
    }

    /// `Query::DayBlocks` returns anything *overlapping* the day — the doc is
    /// explicit that a block running past midnight is on both days. Drawn
    /// unclamped it would start above the top of the view and be invisible.
    @Test
    func aBlockRunningInFromYesterdayIsDrawnFromTheTopOfTheDay() {
        let placed = place([row("a", at(-3), at(2))])
        #expect(placed.count == 1)
        #expect(placed[0].startMs == Self.dayStart)
        #expect(placed[0].endMs == at(2))
    }

    @Test
    func aBlockRunningPastMidnightIsClampedToTheBottom() {
        let placed = place([row("a", at(22), at(26))])
        #expect(placed[0].endMs == Self.dayStart + 86_400_000)
    }

    /// A block that ends exactly when the window opens shares no time with it.
    @Test
    func aBlockEndingAtTheStartOfTheDayIsNotOnIt() {
        #expect(place([row("a", at(-2), Self.dayStart)]).isEmpty)
    }

    @Test
    func overlappingBlocksShareTheWidthRatherThanHidingEachOther() {
        let placed = place([row("a", at(9), at(11)), row("b", at(10), at(12))])
        #expect(placed.count == 2)
        #expect(placed.allSatisfy { $0.laneCount == 2 })
        #expect(Set(placed.map(\.lane)) == [0, 1])
    }

    /// Back-to-back blocks are not a conflict, and they must not cost a column
    /// either — a day of hourly meetings would otherwise draw as a row of
    /// slivers.
    @Test
    func consecutiveBlocksReuseTheSameColumn() {
        let placed = place([row("a", at(9), at(10)), row("b", at(10), at(11))])
        #expect(placed.allSatisfy { $0.laneCount == 1 })
    }

    /// A overlaps B and B overlaps C, but A and C do not touch. Sizing pairs
    /// independently would give A and C the same column and draw them on top
    /// of one another.
    @Test
    func transitivelyOverlappingBlocksAllShareTheWidth() {
        let placed = place([
            row("a", at(9), at(11)),
            row("b", at(10), at(13)),
            row("c", at(12), at(14))
        ])
        #expect(placed.count == 3)
        #expect(placed.allSatisfy { $0.laneCount == 2 })
        let byId = Dictionary(uniqueKeysWithValues: placed.map { ($0.id, $0.lane) })
        #expect(byId["blk_a"] != byId["blk_b"])
        #expect(byId["blk_c"] == byId["blk_a"], "c reuses a's column; they do not overlap")
    }

    @Test
    func placementIsOrderedByStartAndStableForATie() {
        let placed = place([row("b", at(9), at(10)), row("a", at(9), at(10))])
        #expect(placed.map(\.id) == ["blk_a", "blk_b"])
    }

    /// The conflicts come from the domain; layout only decides which of them
    /// are on the day being drawn.
    @Test
    func onlyConflictsTouchingTheWindowAreShaded() {
        let onDay = BlockConflict(a: "blk_a", b: "blk_b", fromMs: at(10), toMs: at(11))
        let elsewhere = BlockConflict(
            a: "blk_c",
            b: "blk_d",
            fromMs: at(-10),
            toMs: at(-9)
        )
        let shaded = BlockLayout.shade(
            conflicts: [onDay, elsewhere],
            fromMs: Self.dayStart,
            toMs: Self.dayStart + 86_400_000
        )
        #expect(shaded.map(\.id) == ["blk_a|blk_b"])
    }

    @Test
    func snappingRoundsToTheNearestSlot() {
        #expect(BlockLayout.snap(ms: at(9, 7), toMinutes: 15, dayStartMs: Self.dayStart) == at(9))
        #expect(BlockLayout.snap(ms: at(9, 8), toMinutes: 15, dayStartMs: Self.dayStart) == at(9, 15))
        #expect(BlockLayout.snap(ms: at(9, 20), toMinutes: 30, dayStartMs: Self.dayStart) == at(9, 30))
        #expect(BlockLayout.snap(ms: at(9, 4), toMinutes: 5, dayStartMs: Self.dayStart) == at(9, 5))
    }

    /// Snapping is relative to the day, not the epoch. India is UTC+5:30, so
    /// epoch-relative snapping would offer :00 and :45 on a 15-minute grid.
    @Test
    func snappingIsRelativeToTheDayNotTheEpoch() {
        let halfHourZone = Self.dayStart - 30 * 60_000
        #expect(
            BlockLayout.snap(ms: halfHourZone + 9 * 3_600_000 + 60_000,
                             toMinutes: 15,
                             dayStartMs: halfHourZone)
                == halfHourZone + 9 * 3_600_000
        )
    }
}
