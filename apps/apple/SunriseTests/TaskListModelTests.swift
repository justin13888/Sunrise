import Foundation
import Testing

@testable import Sunrise

/// The vertical slice, against a real vault: capture through the shared
/// parser, then complete, defer, edit and delete what it produced.
@MainActor
struct TaskListModelTests {
    @Test
    func capturingThroughThePreviewProducesTheTaskThePreviewShowed() async throws {
        let vault = try await TestVault()
        let model = TaskListModel(bridge: vault.bridge, kind: .inbox)
        let capture = CaptureModel(bridge: vault.bridge, debounce: .milliseconds(1))

        capture.text = "Renew passport !1 ~1h"
        try await settle(capture)

        let preview = try #require(capture.preview)
        #expect(preview.draft.title == "Renew passport")
        let draft = try #require(capture.takeDraft())
        #expect(capture.text.isEmpty, "the field clears once the draft is taken")

        await model.create(draft)
        #expect(model.tasks.map(\.title) == ["Renew passport"])

        let facets = model.facets(for: try #require(model.tasks.first))
        #expect(facets.priority == 1)
        #expect(facets.estimate == "1h")
        await vault.bridge.shutdown()
    }

    /// The Inbox is a stream, and `Query::Inbox` returns everything in it that
    /// is not tombstoned — completed tasks included. The row strikes them
    /// through rather than the client inventing a filter the core does not
    /// have; Open/Done/All belongs to the Stream view.
    @Test
    func completingATaskMarksItDoneAndLeavesItInTheStream() async throws {
        let vault = try await TestVault()
        let model = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await model.create(draft("Book the ferry"))
        let task = try #require(model.tasks.first)

        await model.complete(task)

        let after = try #require(model.tasks.first)
        #expect(after.state == .done)
        #expect(after.completedAt != nil)
        #expect(model.facets(for: after).isDone)
        await vault.bridge.shutdown()
    }

    /// Completing is not irreversible on a mis-click.
    @Test
    func aCompletedTaskCanBeReopened() async throws {
        let vault = try await TestVault()
        let model = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await model.create(draft("Book the ferry"))
        await model.complete(try #require(model.tasks.first))

        await model.reopen(try #require(model.tasks.first))

        #expect(model.tasks.first?.state == .todo)
        await vault.bridge.shutdown()
    }

    @Test
    func deletingATaskRemovesIt() async throws {
        let vault = try await TestVault()
        let model = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await model.create(draft("Cancel the gym"))
        let task = try #require(model.tasks.first)

        await model.delete(task)

        #expect(model.tasks.isEmpty)
        #expect(model.errorMessage == nil)
        await vault.bridge.shutdown()
    }

    @Test
    func deferringPushesTheScheduledTimeOutAndCountsIt() async throws {
        let vault = try await TestVault()
        let model = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await model.create(draft("Water the plants"))
        let task = try #require(model.tasks.first)

        await model.defer_(task, byDays: 1)

        let after = try #require(model.tasks.first)
        #expect(after.scheduledAt != nil)
        #expect(after.deferredCount == 1)
        await vault.bridge.shutdown()
    }

    /// The split-optional edit shape: `set` writes, `clear` empties, and the
    /// editor has to get both directions right.
    @Test
    func anEditSetsAndClearsInTheSamePass() async throws {
        let vault = try await TestVault()
        let model = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await model.create(draft("Draft the letter", priority: 3, estimateS: 1800))
        let task = try #require(model.tasks.first)

        var edit = TaskEdit()
        edit.title = "Draft the letter properly"
        edit.setEnergy = .high
        edit.clearPriority = true
        edit.clearEstimatedDuration = true
        await model.apply(edit, to: task)

        let after = try #require(model.tasks.first)
        #expect(after.title == "Draft the letter properly")
        #expect(after.energy == .high)
        #expect(after.priority == nil)
        #expect(after.estimatedDurationS == nil)
        await vault.bridge.shutdown()
    }

    /// Today is the query, not a filter over the Inbox: a task with no time
    /// belongs to one and not the other.
    @Test
    func todayAndInboxAnswerDifferentQuestions() async throws {
        let vault = try await TestVault()
        let inbox = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await inbox.create(draft("Someday, learn Welsh"))

        let today = TaskListModel(bridge: vault.bridge, kind: .todayAll)
        await today.refresh()

        #expect(inbox.tasks.count == 1)
        #expect(today.tasks.isEmpty, "an undated task is not due today")
        #expect(today.groups.isEmpty)
        await vault.bridge.shutdown()
    }

    /// A write made anywhere reaches the list through the change stream —
    /// which is what makes a completion from another device look the same as
    /// one made here.
    @Test
    func aWriteFromElsewhereRepaintsTheList() async throws {
        let vault = try await TestVault()
        let model = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await model.refresh()
        #expect(model.tasks.isEmpty)

        let following = Task { await model.follow() }
        defer { following.cancel() }

        // Straight to the bridge, bypassing the model, the way a sync arrival
        // reaches the vault.
        _ = try await vault.bridge.submit(.createTask(draft: draft("Arrived by sync")))

        try await until { model.tasks.count == 1 }
        #expect(model.tasks.first?.title == "Arrived by sync")
        await vault.bridge.shutdown()
    }

    /// A bare line captured on Today is *on* Today.
    ///
    /// It used to not be. A draft carries no stream, so the core files it in
    /// the Inbox, and `Query::Today` only returns tasks carrying a date — so
    /// the row was written correctly and then was not on the screen that had
    /// just accepted it. Nothing said so: no error, no empty-state change,
    /// no confirmation. The list now supplies the date it selects on.
    @Test
    func capturingOnTodayLeavesTheRowOnToday() async throws {
        let vault = try await TestVault()
        let model = TaskListModel(bridge: vault.bridge, kind: .todayAll)

        await model.create(draft("Renew passport"))

        #expect(model.tasks.map(\.title) == ["Renew passport"])
        #expect(model.tasks.first?.scheduledAt != nil, "Today supplied the date")
        await vault.bridge.shutdown()
    }

    /// What the user typed still wins. `^next saturday` means next Saturday
    /// even when it is typed into Today's own bar — the same rule that lets an
    /// explicit `#stream` override the stream list it was typed into.
    @Test
    func capturingOnTodayDoesNotOverrideADateTheLineCarried() async throws {
        let vault = try await TestVault()
        let model = TaskListModel(bridge: vault.bridge, kind: .todayAll)
        let when = Int64(await vault.bridge.nowMs()) + 6 * 24 * 60 * 60 * 1000

        var dated = draft("Book the ferry")
        dated.scheduledAt = .instant(at: when)
        await model.create(dated)

        // A week out, so it is not on Today at all — which is the proof the
        // date was left alone rather than replaced with now.
        #expect(model.tasks.isEmpty)

        let inbox = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await inbox.refresh()
        #expect(inbox.tasks.map(\.title) == ["Book the ferry"])
        await vault.bridge.shutdown()
    }

    /// Only Today does this. An Inbox capture stays undated, because the Inbox
    /// shows a task whether or not it carries a date and inventing one would
    /// put a thought on a day the user never chose.
    @Test
    func capturingIntoTheInboxLeavesTheLineUndated() async throws {
        let vault = try await TestVault()
        let model = TaskListModel(bridge: vault.bridge, kind: .inbox)

        await model.create(draft("Renew passport"))

        let task = try #require(model.tasks.first)
        #expect(task.scheduledAt == nil)
        #expect(task.dueAt == nil)
        await vault.bridge.shutdown()
    }

    // MARK: - Helpers

    private func draft(
        _ title: String,
        priority: UInt8? = nil,
        estimateS: UInt64? = nil
    ) -> TaskDraftIn {
        TaskDraftIn(
            title: title,
            body: nil,
            streamId: nil,
            contexts: [],
            priority: priority,
            energy: nil,
            estimatedDurationS: estimateS,
            scheduledAt: nil,
            dueAt: nil,
            schedulingConstraints: [],
            assignee: nil,
            reminderLeadS: nil
        )
    }

    private func settle(_ capture: CaptureModel) async throws {
        try await until { capture.preview != nil }
    }

    /// Poll until `condition` holds, or fail rather than hang.
    private func until(
        _ condition: @MainActor () -> Bool,
        timeout: Duration = .seconds(3)
    ) async throws {
        let deadline = ContinuousClock.now.advanced(by: timeout)
        while ContinuousClock.now < deadline {
            if condition() { return }
            try await Task.sleep(for: .milliseconds(10))
        }
        Issue.record("condition never became true within \(timeout)")
    }
}

@MainActor
struct CaptureModelTests {
    @Test
    func anUnresolvedTokenIsExplainedAndNotLost() async throws {
        let vault = try await TestVault()
        let capture = CaptureModel(bridge: vault.bridge, debounce: .milliseconds(1))

        capture.text = "Call the vet #nosuchstream"
        try await waitForPreview(capture)

        #expect(capture.preview?.draft.title.contains("nosuchstream") == true)
        #expect(capture.issues.count == 1)
        #expect(capture.issues.first?.explanation.contains("nosuchstream") == true)
        await vault.bridge.shutdown()
    }

    @Test
    func anEmptyLineIsNotCommittable() async throws {
        let vault = try await TestVault()
        let capture = CaptureModel(bridge: vault.bridge, debounce: .milliseconds(1))

        #expect(!capture.canCommit)
        capture.text = "   "
        #expect(!capture.canCommit)
        #expect(capture.takeDraft() == nil)

        capture.text = "Something real"
        try await waitForPreview(capture)
        #expect(capture.canCommit)
        await vault.bridge.shutdown()
    }

    @Test
    func clearingCancelsAnInFlightPreview() async throws {
        let vault = try await TestVault()
        let capture = CaptureModel(bridge: vault.bridge, debounce: .milliseconds(50))

        capture.text = "Half a thought"
        capture.clear()
        try await Task.sleep(for: .milliseconds(200))

        #expect(capture.preview == nil, "a cancelled preview must not land later")
        await vault.bridge.shutdown()
    }

    private func waitForPreview(
        _ capture: CaptureModel,
        timeout: Duration = .seconds(3)
    ) async throws {
        let deadline = ContinuousClock.now.advanced(by: timeout)
        while ContinuousClock.now < deadline {
            if capture.preview != nil { return }
            try await Task.sleep(for: .milliseconds(10))
        }
        Issue.record("no preview arrived within \(timeout)")
    }
}
