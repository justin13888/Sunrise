import Foundation
import Testing

@testable import Sunrise

/// Routines against a real vault, with the cadence read and described by the
/// domain in both directions.
@MainActor
struct RoutineModelTests {
    @Test
    func creatingARoutineFromAPhraseStoresTheRuleThePhraseMeant() async throws {
        let vault = try await TestVault()
        let model = RoutineModel(bridge: vault.bridge)
        let field = RecurrenceField(text: "every 2 weeks on tue")
        let rule = try #require(field.rule)

        await model.create(draft("Water the plants", rule: rule))

        let routine = try #require(model.visible.first)
        #expect(routine.template.title == "Water the plants")
        #expect(model.cadence(of: routine) == "every 2 weeks on Tu")
        await vault.bridge.shutdown()
    }

    /// The phrase and its description are inverses, and both are the domain's.
    /// A Swift parser would be a second answer to the same question.
    @Test
    func theFieldEchoesBackWhatTheParserUnderstood() throws {
        let field = RecurrenceField(text: "weekdays")
        #expect(field.isValid)
        #expect(field.summary == "every week on Mo, Tu, We, Th, Fr")

        field.text = "every blue moon"
        #expect(!field.isValid)
        let problem: String = try #require(field.problem)
        #expect(problem.contains("blue"), "\(problem)")

        field.text = "   "
        #expect(field.problem == nil, "an empty field is not yet a mistake")
        #expect(!field.isValid)
    }

    /// Editing a routine starts from its rule described back, and saving that
    /// unchanged must not rewrite the schedule.
    @Test
    func reSavingAnUneditedCadenceKeepsTheSameRule() async throws {
        let vault = try await TestVault()
        let model = RoutineModel(bridge: vault.bridge)
        let rule = try #require(RecurrenceField(text: "every mon, wed").rule)
        await model.create(draft("Stand-up", rule: rule))
        let before = try #require(model.visible.first)

        // What the editor puts in the field, parsed back the way Save would.
        let field = RecurrenceField(text: model.cadence(of: before))
        let reparsed: Recurrence = try #require(field.rule)
        var edit = RoutineEdit()
        edit.rrule = reparsed
        await model.update(before, edit)

        let after = try #require(model.visible.first)
        #expect(model.cadence(of: after) == "every week on Mo, We")
        await vault.bridge.shutdown()
    }

    @Test
    func aRoutineReportsWhenItNextFires() async throws {
        let vault = try await TestVault()
        let model = RoutineModel(bridge: vault.bridge)
        let rule = try #require(RecurrenceField(text: "every day").rule)
        await model.create(draft("Water the plants", rule: rule))
        let routine = try #require(model.visible.first)

        let next = try #require(model.nextLabel(for: routine))
        #expect(["today", "tomorrow"].contains(next.text), "\(next.text)")
        #expect(!next.isPast)
        await vault.bridge.shutdown()
    }

    @Test
    func pausingAndArchivingAreSeparateAndReversible() async throws {
        let vault = try await TestVault()
        let model = RoutineModel(bridge: vault.bridge)
        let rule = try #require(RecurrenceField(text: "every day").rule)
        await model.create(draft("Water the plants", rule: rule))
        let routine = try #require(model.visible.first)

        await model.setPaused(routine, true)
        #expect(model.visible.first?.paused == true)

        await model.setArchived(try #require(model.visible.first), true)
        #expect(model.visible.isEmpty, "archived routines leave the list")
        model.showsArchived = true
        await model.refresh()
        #expect(model.routines.first?.archived == true)
        await vault.bridge.shutdown()
    }

    /// Skipping uses an occurrence key in the **routine's** zone, not the
    /// device's — that is what makes a skip survive a tzdb change.
    @Test
    func skippingRecordsAnOccurrenceKeyInTheRoutinesZone() async throws {
        let vault = try await TestVault()
        let model = RoutineModel(bridge: vault.bridge)
        let rule = try #require(RecurrenceField(text: "every day").rule)
        await model.create(draft("Water the plants", rule: rule, timezone: "Asia/Tokyo"))
        let routine = try #require(model.visible.first)

        await model.skipNext(routine)

        let after = try #require(model.visible.first)
        #expect(after.skippedKeys.count == 1)
        let key = try #require(after.skippedKeys.first)
        #expect(key.count == 16, "\(key)")
        // The same instant in the device's zone would produce a different key
        // whenever the two zones disagree about the date or the hour.
        let atMs = try #require(nextOccurrenceMs(routine: routine, nowMs: model.nowMs))
        #expect(RoutineModel.occurrenceKey(atMs: atMs, timeZone: "Asia/Tokyo") == key)
        await vault.bridge.shutdown()
    }

    /// Deleting a routine cannot be undone; the model says so rather than
    /// leaving an Undo item that would do nothing.
    @Test
    func deletingARoutineSaysItCannotBeUndone() async throws {
        let vault = try await TestVault()
        let model = RoutineModel(bridge: vault.bridge)
        let rule = try #require(RecurrenceField(text: "every day").rule)
        await model.create(draft("Water the plants", rule: rule))

        await model.delete(try #require(model.visible.first))

        #expect(model.visible.isEmpty)
        let note = try #require(model.undoNote)
        #expect(note.lowercased().contains("tombstone"), "\(note)")
        await vault.bridge.shutdown()
    }

    private func draft(
        _ title: String,
        rule: Recurrence,
        timezone: String = "UTC"
    ) -> RoutineDraftIn {
        RoutineDraftIn(
            template: Template(
                title: title,
                streamId: inboxStreamId(),
                contexts: [],
                energy: nil,
                priority: nil,
                estimatedDurationS: nil,
                body: nil
            ),
            rrule: rule,
            timezone: timezone,
            startsAt: Timestamp(Date().timeIntervalSince1970 * 1000),
            endsAt: nil,
            schedulingConstraints: [],
            catchupPolicy: .skip
        )
    }
}
