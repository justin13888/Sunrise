import AppIntents
import Foundation
import Testing

@testable import Sunrise

private typealias Surface = SystemSurfaceFixture

/// The two intents `mobile-ios.md` §Shortcuts and App Intents listed and the
/// app did not have — defer and stream-summary — and the stream entity both
/// the summary and the Focus filter pick from.
///
/// These pass the vault in. The ones that go through `perform()`, and so
/// through `IntentVault.override`, are in the extension of
/// ``IntentPerformTests`` below: that slot is process-wide, and a suite of its
/// own running beside that one would clear it under the other's feet — and
/// send an intent to the developer's real Keychain.
@MainActor
struct SystemIntentTests {
    // MARK: - Defer

    /// The target is the seam's civil computation, the one a reminder's
    /// snooze buttons use, so the intent and the banner agree on "tomorrow".
    @Test
    func theTargetIsTheSnoozeButtonsOwn() {
        let now: UInt64 = 1_760_000_000_000
        let zone = TimeZone(identifier: "Europe/London") ?? .gmt
        for span in DeferSpan.allCases {
            let expected = snoozeTargetMs(fromMs: now, span: span.snoozeSpan, tz: zone.identifier)
            #expect(DeferTaskIntent.target(fromMs: now, span: span, timeZone: zone) == UInt64(expected))
        }
        #expect(
            DeferSpan.allCases.count == DeferSpan.caseDisplayRepresentations.count,
            "a case with no display name is a blank row in the Shortcuts picker"
        )
    }

    @Test
    func deferringSaysWhereTheTaskWent() async throws {
        let vault = try await TestVault()
        let id = try await Surface.task("Call the bank", in: vault.bridge)
        let deferred = try await DeferTaskIntent.deferTask(id, span: .nextWeek, in: vault.bridge)
        #expect(deferred.message == "Deferred “Call the bank” to next week.")
        #expect(deferred.task.id == id)
        await vault.bridge.shutdown()
    }

    /// A finished task is refused rather than given a date: deferring it would
    /// be the automation reopening it by the back door.
    @Test
    func aFinishedTaskIsNotDeferred() async throws {
        let vault = try await TestVault()
        let id = try await Surface.task("Already done", in: vault.bridge)
        _ = try await vault.bridge.submit(.completeTask(id: id))
        await #expect(throws: IntentError.taskAlreadyFinished("Already done")) {
            _ = try await DeferTaskIntent.deferTask(id, span: .tomorrow, in: vault.bridge)
        }
        let item = try await Surface.read(id, in: vault.bridge)
        #expect(item.deferredCount == 0)
        await vault.bridge.shutdown()
    }

    @Test
    func deferringAVanishedTaskReportsIt() async throws {
        let vault = try await TestVault()
        let id = try await Surface.task("Gone", in: vault.bridge)
        _ = try await vault.bridge.submit(.deleteTask(id: id))
        await #expect(throws: IntentError.taskNotFound(id)) {
            _ = try await DeferTaskIntent.deferTask(id, span: .oneHour, in: vault.bridge)
        }
        await vault.bridge.shutdown()
    }

    // MARK: - Stream summary

    @Test
    func aStreamSummaryCountsAndNamesTheOpenTasks() async throws {
        let vault = try await TestVault()
        let errands = try await Surface.stream("Errands", in: vault.bridge)
        let work = try await Surface.stream("Work", in: vault.bridge)
        for title in ["Post the parcel", "Buy stamps", "Return the books", "Collect the keys"] {
            _ = try await Surface.task(title, stream: errands, in: vault.bridge)
        }
        let done = try await Surface.task("Already posted", stream: errands, in: vault.bridge)
        _ = try await vault.bridge.submit(.completeTask(id: done))
        _ = try await Surface.task("Not an errand", stream: work, in: vault.bridge)

        let report = try await StreamSummaryIntent.summarise(errands, in: vault.bridge)
        #expect(report.tasks.count == 4, "open tasks of that stream only")
        #expect(!report.tasks.map(\.title).contains("Already posted"))
        #expect(report.message.hasPrefix("4 tasks open in Errands: "))
        #expect(report.message.hasSuffix(", and 1 more."))
        await vault.bridge.shutdown()
    }

    @Test
    func anEmptyStreamStillSaysSomething() {
        #expect(StreamSummaryIntent.message(stream: "Garden", open: []) == "Nothing is open in Garden.")
        let one = [TaskEntity(id: "tsk_1", title: "Mow", isCompleted: false)]
        #expect(StreamSummaryIntent.message(stream: "Garden", open: one) == "1 task open in Garden: Mow.")
    }

    @Test
    func summarisingAVanishedStreamReportsIt() async throws {
        let vault = try await TestVault()
        await #expect(throws: IntentError.streamNotFound("str_gone")) {
            _ = try await StreamSummaryIntent.summarise("str_gone", in: vault.bridge)
        }
        await vault.bridge.shutdown()
    }

    @Test
    func theNewErrorsReadAsSentences() {
        for error in [IntentError.streamNotFound("str_1"), .taskAlreadyFinished("Mow")] {
            #expect(String(localized: error.localizedStringResource).count > 10)
        }
    }
}

/// The new intents and the stream query, run through the App Intents surface
/// itself — inside ``IntentPerformTests``, whose `.serialized` trait is what
/// keeps every user of `IntentVault.override` from running at once.
extension IntentPerformTests {
    /// Run through `perform()`: the write is the same `DeferTask` the row's
    /// Defer menu sends, so the task gets the new date *and* its deferral count
    /// goes up.
    @Test
    func deferringThroughTheIntentMovesTheTaskAndCountsIt() async throws {
        let vault = try await TestVault()
        defer { IntentVault.override = nil }
        IntentVault.override = vault.bridge

        let id = try await Surface.task("Renew the passport", in: vault.bridge)
        let entity = TaskEntity(id: id, title: "Renew the passport", isCompleted: false)
        _ = try await DeferTaskIntent(task: entity, span: .tomorrow).perform()

        let item = try await Surface.read(id, in: vault.bridge)
        let now = await vault.bridge.nowMs()
        #expect(item.deferredCount == 1)
        guard case let .instant(at)? = item.scheduledAt else {
            Issue.record("not deferred to an instant: \(String(describing: item.scheduledAt))")
            return
        }
        #expect(at > Int64(now), "deferred into the future")
        await vault.bridge.shutdown()
    }

    @Test
    func aStreamSummaryAnswersThroughPerform() async throws {
        let vault = try await TestVault()
        defer { IntentVault.override = nil }
        IntentVault.override = vault.bridge

        let errands = try await Surface.stream("Errands", in: vault.bridge)
        _ = try await Surface.task("Post the parcel", stream: errands, in: vault.bridge)
        _ = try await StreamSummaryIntent(stream: StreamEntity(id: errands, name: "Errands")).perform()
        await vault.bridge.shutdown()
    }

    /// Streams are found by id, by typed name, and listed for a picker — the
    /// last one opening the vault, unlike tasks, because a Focus filter's
    /// stream picker has nothing else to offer.
    @Test
    func streamsAreFoundByIdByNameAndBySuggestion() async throws {
        let vault = try await TestVault()
        defer { IntentVault.override = nil }
        IntentVault.override = vault.bridge

        let errands = try await Surface.stream("Errands", in: vault.bridge)
        _ = try await Surface.stream("Work", in: vault.bridge)

        let query = StreamEntityQuery()
        #expect(try await query.entities(for: [errands, "str_gone"]).map(\.name) == ["Errands"])
        #expect(try await query.entities(matching: "err").map(\.id) == [errands])
        let suggested = try await query.suggestedEntities().map(\.name)
        #expect(suggested.contains("Errands") && suggested.contains("Work"))
        await vault.bridge.shutdown()
    }
}
