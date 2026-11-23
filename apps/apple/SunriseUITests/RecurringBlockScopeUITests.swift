import XCTest

/// A weekly block on the Mac's week grid, end to end (#1209).
///
/// `BlockEditReachTests` proves the scope each answer to "which events?"
/// *sends*, on the pure `CalendarModel.updateCommand`. These prove the rest:
/// that the grid draws every week of a series, that dragging one event asks
/// the question at all, and that what the core then *writes* for each answer
/// is what the grid draws when it reads the series back.
///
/// The series is the harness's (`-sunrise-ui-test-weekly-block`): one hour a
/// week from 04:00 on the wall clock, on today's weekday, beginning four
/// weeks back with no end, imported through the app's own `.ics` path.
///
/// **What a test reads is a block's height down its column.** The grid is
/// the core's answer drawn, re-queried after every write, so a block one
/// hour lower in a week is that week's event having moved an hour. Each week
/// is measured before the drag and compared with itself after it, never
/// with another week, so a week with a daylight-saving change in it, whose
/// 04:00 sits an hour away from every other week's, is still a fair test.
@MainActor
final class RecurringBlockScopeUITests: SunriseUITestCase {
    /// `WeeklyBlockSeed.title` in the app.
    private static let title = "Studio hours"
    /// `WeeklyBlockSeed.weeksBefore` in the app.
    private static let weeksBefore = 4
    /// `CalendarMetrics.hourHeight`: one hour down a day column, in points.
    private static let hourHeight: CGFloat = 44

    override func setUp() async throws {
        try await super.setUp()
        relaunch(adding: ["-sunrise-ui-test-weekly-block"])
        createVault()
        openWeekGrid()
    }

    /// One event a week, for every week of the series, and none the week
    /// before it began: stepped back to its first week and forward past
    /// today, through the grid's own Previous and Next.
    func testTheWeeklyBlockAppearsInEveryWeekOfItsSeries() {
        expectBlocks(1, "this week holds one event")
        XCTAssertTrue(
            app.staticTexts[Self.title].firstMatch.exists,
            "the event is the seeded block, by its title"
        )
        for back in 1...Self.weeksBefore {
            step("Previous")
            expectBlocks(1, "\(back) week(s) back holds one event")
        }
        step("Previous")
        expectBlocks(0, "the week before the series began holds none")

        for _ in 0...Self.weeksBefore { step("Next") }
        for ahead in 1...Self.weeksBefore {
            step("Next")
            expectBlocks(1, "\(ahead) week(s) ahead holds one event")
        }
    }

    /// *This event*: the dragged event moves, the weeks either side do not.
    func testThisEventMovesOnlyTheDraggedEvent() {
        dragThisWeeksEvent(
            answering: "block-reach.thisEvent",
            moves: Weeks(last: false, this: true, next: false)
        )
    }

    /// *This and following events*: the dragged event and the weeks after it
    /// move, the week before does not. The planner splits the series there
    /// (`docs/08-features/planner.md` §Recurring blocks).
    func testThisAndFollowingMovesTheDraggedEventAndTheWeeksAfter() {
        dragThisWeeksEvent(
            answering: "block-reach.thisAndFollowing",
            moves: Weeks(last: false, this: true, next: true)
        )
    }

    /// *All events*: every week moves, the ones before the dragged one too.
    func testAllEventsMovesEveryWeek() {
        dragThisWeeksEvent(
            answering: "block-reach.allEvents",
            moves: Weeks(last: true, this: true, next: true)
        )
    }

    /// Cancel writes nothing: the event the drag let go of goes back.
    func testCancellingTheQuestionMovesNothing() {
        dragThisWeeksEvent(
            answering: "block-reach.cancel",
            moves: Weeks(last: false, this: false, next: false)
        )
    }

    // MARK: - Driving the grid

    /// Which of last week's, this week's and next week's events an answer
    /// moves.
    private struct Weeks {
        let last: Bool
        let this: Bool
        let next: Bool
    }

    private func openWeekGrid(file: StaticString = #filePath, line: UInt = #line) {
        activate(
            app.descendants(matching: .any)["sidebar.calendar"].firstMatch,
            named: "the calendar row in the sidebar",
            timeout: 20,
            file: file,
            line: line
        )
        activate(
            app.radioButtons["Week"].firstMatch,
            named: "the span picker's Week",
            file: file,
            line: line
        )
    }

    private var blocks: XCUIElementQuery {
        app.descendants(matching: .any).matching(identifier: "calendar-block")
    }

    /// One frame per chip, from every `calendar-block` match.
    ///
    /// `BlockChip` sets the identifier on its container, and SwiftUI may hand
    /// it down to the glyphs and the title inside, so one chip can be several
    /// matches. Those all sit inside the chip's own frame: keep only the
    /// matches no other match contains, which is one per chip either way.
    private func chipFrames() -> [CGRect] {
        let frames = blocks.allElementsBoundByIndex.map(\.frame)
        return frames.enumerated().filter { index, frame in
            !frames.enumerated().contains { other, outer in
                other != index && outer.contains(frame) && (outer != frame || other < index)
            }
        }.map(\.element)
    }

    /// Step the grid a week, by the toolbar's chevron of that name.
    private func step(_ direction: String, file: StaticString = #filePath, line: UInt = #line) {
        let chevron = app.buttons[direction].firstMatch
        activate(chevron, named: "the toolbar's \(direction)", file: file, line: line)
    }

    /// The frames of the blocks on screen, once they have stopped changing.
    ///
    /// A step moves the grid to its new week at once and redraws its blocks
    /// only when the core's answer for that week comes back, and a write is
    /// drawn only once the grid re-reads it. Either way there is a moment in
    /// which what is on screen is the state before. So read until a whole
    /// second passes with nothing changing, which every one of those round
    /// trips beats by far, rather than taking the first look.
    private func settledBlocks(timeout: TimeInterval = 15) -> [CGRect] {
        let deadline = Date().addingTimeInterval(timeout)
        var last = chipFrames()
        var since = Date()
        while Date() < deadline {
            usleep(150_000)
            let now = chipFrames()
            if now != last {
                last = now
                since = Date()
            } else if Date().timeIntervalSince(since) >= 1 {
                return now
            }
        }
        XCTFail("the grid's blocks settled within \(Int(timeout)) s")
        return last
    }

    /// The event is an hour long, so the chip is an hour tall. A frame of
    /// any other height is not the chip (iOS reports the whole day column
    /// for this identifier), and its top would not move with the event.
    private func assertHourTall(
        _ frame: CGRect,
        in week: String,
        file: StaticString = #filePath,
        line: UInt = #line
    ) {
        XCTAssertEqual(
            frame.height,
            Self.hourHeight,
            accuracy: 4,
            "\(week)'s `calendar-block` frame is the chip's own, one hour tall",
            file: file,
            line: line
        )
    }

    /// The week on screen holds `count` events, each the chip's own frame.
    private func expectBlocks(
        _ count: Int,
        _ week: String,
        file: StaticString = #filePath,
        line: UInt = #line
    ) {
        let frames = settledBlocks()
        XCTAssertEqual(frames.count, count, week, file: file, line: line)
        for frame in frames { assertHourTall(frame, in: week, file: file, line: line) }
    }

    /// The top of the one event in the week on screen.
    private func eventTop(_ week: String, file: StaticString = #filePath, line: UInt = #line) -> CGFloat? {
        let frames = settledBlocks()
        XCTAssertEqual(frames.count, 1, "\(week) holds one event", file: file, line: line)
        guard let frame = frames.first else { return nil }
        assertHourTall(frame, in: week, file: file, line: line)
        return frame.minY
    }

    /// Measure last week, this week and next, drag this week's event an hour
    /// down, answer the question with `answer`, and check each week moved an
    /// hour or not at all, as `moves` says.
    private func dragThisWeeksEvent(
        answering answer: String,
        moves: Weeks,
        file: StaticString = #filePath,
        line: UInt = #line
    ) {
        step("Previous", file: file, line: line)
        let lastBefore = eventTop("last week", file: file, line: line)
        step("Next", file: file, line: line)
        let thisBefore = eventTop("this week", file: file, line: line)
        step("Next", file: file, line: line)
        let nextBefore = eventTop("next week", file: file, line: line)
        step("Previous", file: file, line: line)
        _ = settledBlocks()

        guard let event = chipFrames().first else {
            XCTFail("this week's event is on screen to drag", file: file, line: line)
            return
        }
        // A third of the way down the chip's own frame, rather than an offset
        // into whichever match `firstMatch` returns, clear of the resize strip
        // on its bottom edge, and pressed before it moves: the grid sits in a
        // scroll view, whose pan takes a drag that starts moving at once.
        let origin = app.frame.origin
        let grip = app.coordinate(withNormalizedOffset: .zero).withOffset(CGVector(
            dx: event.midX - origin.x,
            dy: event.minY + event.height * 0.3 - origin.y
        ))
        grip.press(forDuration: 0.4, thenDragTo: grip.withOffset(CGVector(dx: 0, dy: Self.hourHeight)))

        activate(
            app.buttons[answer].firstMatch,
            named: "the question's \(answer) — dragging one event of a repeating block asks which events",
            file: file,
            line: line
        )

        let thisAfter = eventTop("this week, after the drag", file: file, line: line)
        step("Previous", file: file, line: line)
        let lastAfter = eventTop("last week, after the drag", file: file, line: line)
        step("Next", file: file, line: line)
        step("Next", file: file, line: line)
        let nextAfter = eventTop("next week, after the drag", file: file, line: line)

        for (week, before, after, moved) in [
            ("last week", lastBefore, lastAfter, moves.last),
            ("this week", thisBefore, thisAfter, moves.this),
            ("next week", nextBefore, nextAfter, moves.next)
        ] {
            guard let before, let after else { continue }
            let expected = before + (moved ? Self.hourHeight : 0)
            XCTAssertEqual(
                after,
                expected,
                accuracy: 2,
                "\(answer): \(week)'s event \(moved ? "moved an hour later" : "stayed where it was")",
                file: file,
                line: line
            )
        }
    }
}
