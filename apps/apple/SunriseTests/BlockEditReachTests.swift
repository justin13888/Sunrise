import Foundation
import Testing

@testable import Sunrise

/// What a move, edit or delete of a block sends, for each answer to "which
/// events?" (`docs/02-domain/time-blocks.md` §Lifecycle).
///
/// Pure, without a vault: no command yet creates a repeating block, so a
/// vault cannot hold one for the model to read back. The core's side of each
/// scope is `crates/sunrise-core/src/engine/tests/series.rs`.
@MainActor
struct BlockEditReachTests {
    private static let tz = "UTC"
    private static let key = "2026-03-11T09:00"

    private func floating(_ civil: String) -> TimeValue {
        .floating(civil: civil)
    }

    private func ms(_ civil: String) -> Int64 {
        timeValueMs(value: floating(civil), tz: Self.tz)
    }

    /// A weekly series anchored on Wednesday 4 March, 09:00–10:00.
    private func anchor() -> BlockItem {
        item(starts: "2026-03-04T09:00:00", ends: "2026-03-04T10:00:00")
    }

    private func item(starts: String, ends: String, kind: String = "flexible") -> BlockItem {
        BlockItem(
            id: "blk_weekly",
            createdAt: 0,
            updatedAt: 0,
            streamId: inboxStreamId(),
            startsAt: floating(starts),
            endsAt: floating(ends),
            title: "Review",
            titleTrackTask: false,
            tasks: [],
            deleted: false,
            kind: kind
        )
    }

    /// Its event on 11 March, as the week query returns it: the event's times
    /// and the event's key.
    private func occurrence() -> BlockGridRow {
        BlockGridRow(
            block: item(starts: "2026-03-11T09:00:00", ends: "2026-03-11T10:00:00"),
            title: "Review",
            taskTitles: [],
            occurrenceKey: Self.key
        )
    }

    private func oneOff() -> BlockGridRow {
        BlockGridRow(
            block: item(starts: "2026-03-11T09:00:00", ends: "2026-03-11T10:00:00"),
            title: "Review",
            taskTitles: []
        )
    }

    /// The event dragged an hour later.
    private func anHourLater() -> BlockEdit {
        var edit = BlockEdit()
        edit.startsAt = floating("2026-03-11T10:00:00")
        edit.endsAt = floating("2026-03-11T11:00:00")
        return edit
    }

    private func sentEdit(_ command: CoreCommand?) -> (id: EntityRef, edit: BlockEdit)? {
        guard case let .updateBlock(id, edit)? = command else { return nil }
        return (id, edit)
    }

    private func sentScope(_ command: CoreCommand?) -> (id: EntityRef, scope: EditScopeIn?)? {
        guard case let .deleteBlock(id, scope)? = command else { return nil }
        return (id, scope)
    }

    @Test
    func aBlockThatDoesNotRecurSendsNoScope() throws {
        let sent = try #require(sentEdit(CalendarModel.updateCommand(
            row: oneOff(), edit: anHourLater(), reach: nil, anchor: nil, timeZone: Self.tz
        )))
        #expect(sent.id == "blk_weekly")
        #expect(sent.edit.scope == nil, "the whole block, as before there were repeating ones")
        #expect(sent.edit.startsAt == floating("2026-03-11T10:00:00"))
        let deleted = try #require(sentScope(CalendarModel.deleteCommand(row: oneOff(), reach: nil)))
        #expect(deleted.scope == nil)
    }

    /// The question went unanswered, so nothing is sent: a reach is never
    /// guessed for a change to a repeating block.
    @Test
    func anUnansweredEventSendsNothing() {
        #expect(CalendarModel.updateCommand(
            row: occurrence(), edit: anHourLater(), reach: nil, anchor: anchor(), timeZone: Self.tz
        ) == nil)
        #expect(CalendarModel.deleteCommand(row: occurrence(), reach: nil) == nil)
    }

    /// *This event* and *this and following* send the event's own new times
    /// with its key: the core writes them into its exception, or starts the
    /// continuing series at them.
    @Test
    func thisEventAndFollowingSendTheEventsTimesAndKey() throws {
        let cases: [(BlockEditReach, EditScopeIn)] = [
            (.thisEvent, .this(key: Self.key)),
            (.thisAndFollowing, .thisAndFuture(key: Self.key))
        ]
        for (reach, scope) in cases {
            let sent = try #require(sentEdit(CalendarModel.updateCommand(
                row: occurrence(), edit: anHourLater(), reach: reach, anchor: nil, timeZone: Self.tz
            )))
            #expect(sent.id == "blk_weekly")
            #expect(sent.edit.scope == scope)
            #expect(sent.edit.startsAt == floating("2026-03-11T10:00:00"))
            #expect(sent.edit.endsAt == floating("2026-03-11T11:00:00"))
        }
    }

    /// *All events* moves the series' anchor by as much as the event moved,
    /// keeping it on 4 March. Sending the event's own times would re-anchor
    /// the series on 11 March and drop the event before it.
    @Test
    func allEventsShiftsTheAnchorByTheEventsMove() throws {
        let sent = try #require(sentEdit(CalendarModel.updateCommand(
            row: occurrence(), edit: anHourLater(), reach: .allEvents, anchor: anchor(), timeZone: Self.tz
        )))
        #expect(sent.edit.scope == .all)
        #expect(sent.edit.startsAt == floating("2026-03-04T10:00:00"))
        #expect(sent.edit.endsAt == floating("2026-03-04T11:00:00"))
        // And no anchor means nothing correct to send.
        #expect(CalendarModel.updateCommand(
            row: occurrence(), edit: anHourLater(), reach: .allEvents, anchor: nil, timeZone: Self.tz
        ) == nil)
    }

    /// A resize moves only the end, and only the end of the anchor moves.
    @Test
    func allEventsShiftsOnlyTheBoundTheEditSets() throws {
        var longer = BlockEdit()
        longer.endsAt = floating("2026-03-11T10:30:00")
        let sent = try #require(sentEdit(CalendarModel.updateCommand(
            row: occurrence(), edit: longer, reach: .allEvents, anchor: anchor(), timeZone: Self.tz
        )))
        #expect(sent.edit.startsAt == nil)
        #expect(sent.edit.endsAt == floating("2026-03-04T10:30:00"))
        #expect(ms("2026-03-04T10:30:00") - ms("2026-03-04T10:00:00") == 1_800_000)
    }

    /// A retitle of every event needs no anchor: it moves no time.
    @Test
    func allEventsRetitleNeedsNoAnchor() throws {
        var retitle = BlockEdit()
        retitle.setTitle = "Weekly review"
        let sent = try #require(sentEdit(CalendarModel.updateCommand(
            row: occurrence(), edit: retitle, reach: .allEvents, anchor: nil, timeZone: Self.tz
        )))
        #expect(sent.edit.scope == .all)
        #expect(sent.edit.setTitle == "Weekly review")
    }

    @Test
    func eachAnswerDeletesAsFarAsItSays() throws {
        let cases: [(BlockEditReach, EditScopeIn)] = [
            (.thisEvent, .this(key: Self.key)),
            (.thisAndFollowing, .thisAndFuture(key: Self.key)),
            (.allEvents, .all)
        ]
        for (reach, scope) in cases {
            let sent = try #require(sentScope(CalendarModel.deleteCommand(row: occurrence(), reach: reach)))
            #expect(sent.id == "blk_weekly")
            #expect(sent.scope == scope)
        }
    }

    /// A conflict names two block ids; it marks only the event it falls on,
    /// not every event of a repeating block.
    @Test
    func aConflictMarksOnlyTheEventItFallsOn() {
        let placed = PlacedBlock(
            row: occurrence(),
            startMs: ms("2026-03-11T09:00:00"),
            endMs: ms("2026-03-11T10:00:00"),
            lane: 0,
            laneCount: 1
        )
        let here = BlockConflict(
            a: "blk_weekly", b: "blk_other",
            fromMs: ms("2026-03-11T09:30:00"), toMs: ms("2026-03-11T10:00:00")
        )
        let lastWeek = BlockConflict(
            a: "blk_weekly", b: "blk_other",
            fromMs: ms("2026-03-04T09:30:00"), toMs: ms("2026-03-04T10:00:00")
        )
        let otherBlocks = BlockConflict(
            a: "blk_x", b: "blk_y",
            fromMs: ms("2026-03-11T09:30:00"), toMs: ms("2026-03-11T10:00:00")
        )
        #expect(CalendarModel.isPart(placed, of: here))
        #expect(!CalendarModel.isPart(placed, of: lastWeek))
        #expect(!CalendarModel.isPart(placed, of: otherBlocks))
    }

    @Test
    func aFixedBlockReadsFixedAndEveryOtherKindFlexible() {
        let fixed = BlockGridRow(
            block: item(starts: "2026-03-11T09:00:00", ends: "2026-03-11T10:00:00", kind: "fixed"),
            title: nil,
            taskTitles: []
        )
        #expect(fixed.isFixed)
        #expect(!oneOff().isFixed)
        #expect(occurrence().recurs)
        #expect(!oneOff().recurs)
    }
}
