import Foundation
import Testing

@testable import Sunrise

/// Dragging one event of a repeating block through the planner
/// (`docs/08-features/planner.md` §Recurring blocks): the drag previews the
/// event it names, with a ghost on that event and on no other, and the drop
/// commits it as far as "which events?" said.
///
/// The series is daily from tomorrow at 09:00, imported as a calendar file is,
/// since no command makes a block repeat. Every test plans on tomorrow, so
/// nothing it moves is in the past.
@MainActor
struct PlannedRepeatingDragTests {
    private static let title = "Daily review"

    /// A model on tomorrow, holding the daily series.
    private func model(_ vault: borrowing TestVault) async throws -> CalendarModel {
        let model = CalendarModel(bridge: vault.bridge)
        await model.refresh()
        await model.step(1)
        let text = document(startMs: hour(model, 9), timeZone: model.timeZone)
        let report = try await vault.bridge.importIcal(text: text)
        try #require(report.blocks.count == 1 && report.failed == 0 && report.notices.isEmpty)
        await model.refresh()
        return model
    }

    /// One daily `VEVENT`, an hour long from `startMs`, in floating time on
    /// the grid's wall clock, so every day's event is at 09:00 whatever a
    /// daylight-saving change between them does.
    private func document(startMs: Int64, timeZone: String) -> String {
        let formatter = DateFormatter()
        formatter.calendar = Calendar(identifier: .gregorian)
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.timeZone = TimeZone(identifier: timeZone)
        formatter.dateFormat = "yyyyMMdd'T'HHmmss"
        let at = { (ms: Int64) in formatter.string(from: Date(timeIntervalSince1970: Double(ms) / 1000)) }
        return [
            "BEGIN:VCALENDAR",
            "VERSION:2.0",
            "PRODID:-//Sunrise//planned repeating drag test//EN",
            "BEGIN:VEVENT",
            "UID:daily-review@test.sunrise.invalid",
            "DTSTAMP:20260101T000000Z",
            "DTSTART:\(at(startMs))",
            "DTEND:\(at(startMs + 3_600_000))",
            "RRULE:FREQ=DAILY",
            "SUMMARY:\(Self.title)",
            "END:VEVENT",
            "END:VCALENDAR",
            ""
        ].joined(separator: "\r\n")
    }

    private func hour(_ model: CalendarModel, _ hours: Double) -> Int64 {
        model.windowStartMs + Int64(hours * 3_600_000)
    }

    /// The day on screen's event of the series.
    private func event(_ model: CalendarModel) throws -> BlockGridRow {
        try #require(model.rows.first { $0.title == Self.title })
    }

    private func start(_ model: CalendarModel) throws -> Int64 {
        timeValueMs(value: try event(model).block.startsAt, tz: model.timeZone)
    }

    /// Where the event the day after the one on screen starts, in hours into
    /// that day. The grid is left on the day it was on.
    private func nextDaysStart(_ model: CalendarModel) async throws -> Double {
        await model.step(1)
        let hours = Double(try start(model) - model.windowStartMs) / 3_600_000
        await model.step(-1)
        return hours
    }

    /// **This event.** The drag previews only the event it names: one move,
    /// one ghost, and the other events of the series not drawn as moving.
    /// The drop moves that event and leaves the next day's where it was, and
    /// one undo puts it back.
    @Test
    func aDragOfThisEventMovesOnlyThatEventAndOneUndoRestoresIt() async throws {
        let vault = try await TestVault()
        let model = try await model(vault)
        let row = try event(model)
        #expect(row.recurs)
        let zone = model.timeZone

        await model.planner.previewDrag(row, mode: .move, toMs: hour(model, 11), timeZone: zone)

        let plan = try #require(model.planner.plan)
        #expect(plan.committable)
        #expect(plan.moves.map(\.item) == [row.block.id])
        #expect(model.planner.moves(row), "the dragged event is drawn as moving")
        let other = BlockGridRow(block: row.block, title: row.title, taskTitles: [], occurrenceKey: "another")
        #expect(!model.planner.moves(other), "another event of the series is not")
        let ghosts = model.planner.ghosts(
            fromMs: model.dayStartMs(offset: 0), toMs: model.dayStartMs(offset: 1), timeZone: zone
        )
        #expect(ghosts.count == 1)
        #expect(ghosts.first?.after == GridSpan(startMs: hour(model, 11), endMs: hour(model, 12)))
        #expect(try start(model) == hour(model, 9), "a preview writes nothing")

        await model.planner.dropDrag(row, mode: .move, toMs: hour(model, 11), timeZone: zone, scope: .this)

        #expect(model.planner.plan == nil)
        #expect(try start(model) == hour(model, 11))
        #expect(try await nextDaysStart(model) == 9, "the rest of the series stays")

        let undo = UndoModel(bridge: vault.bridge)
        await undo.refresh()
        let label = L10n.Calendar.undoPlanMove(title: Self.title, count: 1)
        #expect(undo.undoTitle == L10n.Undo.undoNamed(action: label))
        await undo.undo()
        await model.refresh()
        #expect(try start(model) == hour(model, 9))
        await vault.bridge.shutdown()
    }

    /// **All events.** The answer re-solves the drag that previewed as this
    /// event, and the commit moves the series: the next day's event moves by
    /// as much as the dragged one.
    @Test
    func aDragOfAllEventsMovesTheSeries() async throws {
        let vault = try await TestVault()
        let model = try await model(vault)
        let row = try event(model)
        let zone = model.timeZone

        await model.planner.previewDrag(row, mode: .move, toMs: hour(model, 11), timeZone: zone)
        await model.planner.dropDrag(row, mode: .move, toMs: hour(model, 11), timeZone: zone, scope: .all)

        #expect(model.planner.plan == nil)
        #expect(try start(model) == hour(model, 11))
        #expect(try await nextDaysStart(model) == 11)
        await vault.bridge.shutdown()
    }

    /// **The planning keys work on one event.** A nudge previews it as this
    /// event; its commit waits for the reach, and the answer commits it.
    @Test
    func aNudgeOfOneEventPreviewsAndCommitsAtTheAnsweredReach() async throws {
        let vault = try await TestVault()
        let model = try await model(vault)
        model.snapMinutes = 30
        let row = try event(model)

        let pressed = await model.planner.press(.later, on: row, snapMinutes: 30, timeZone: model.timeZone)
        #expect(pressed)
        #expect(model.planner.plansOccurrence, "Return asks which events first")
        #expect(model.planner.moves(row))

        await model.planner.commitPlan(reaching: .all)

        #expect(model.planner.plan == nil)
        #expect(try start(model) == hour(model, 9.5))
        #expect(try await nextDaysStart(model) == 9.5)
        await vault.bridge.shutdown()
    }

    /// A drag's "which events?" offers *this event*, *this and following*
    /// and *all events*, each sending its scope, and no answer dismisses it.
    @Test
    func aDragsQuestionOffersEveryReachThePlannerPlans() {
        var sent: [OccurrenceScope] = []
        var dismissals = 0
        let question = PendingBlockReach.planned(dismissed: { dismissals += 1 }, commit: { sent.append($0) })

        #expect(question.reaches == [.thisEvent, .thisAndFollowing, .allEvents])
        #expect(question.action == .move)
        question.commit(.thisEvent)
        question.commit(.thisAndFollowing)
        question.commit(.allEvents)
        #expect(sent == [.this, .thisAndFollowing, .all])
        #expect(dismissals == 0)
    }
}
