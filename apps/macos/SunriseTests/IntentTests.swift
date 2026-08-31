import AppIntents
import Foundation
import Testing

@testable import Sunrise

// MARK: - Shared helpers

/// Create a task and return its id.
private func makeTask(
    _ title: String,
    in bridge: CoreBridge,
    blockedBy: [EntityRef] = []
) async throws -> EntityRef {
    let outcome = try await bridge.submit(.createTask(draft: TaskDraftIn(
        title: title,
        body: nil,
        streamId: nil,
        contexts: [],
        priority: nil,
        energy: nil,
        estimatedDurationS: 1800,
        scheduledAt: nil,
        dueAt: nil,
        schedulingConstraints: [],
        assignee: nil,
        reminderLeadS: nil
    )))
    if !blockedBy.isEmpty {
        var edit = TaskEdit()
        edit.blockedBy = blockedBy
        _ = try await bridge.submit(.updateTask(id: outcome.entity, edit: edit))
    }
    return outcome.entity
}

private func readTask(_ id: EntityRef, in bridge: CoreBridge) async throws -> TaskItem {
    try await TaskLookup.read(id, in: bridge)
}

private func inboxRows(_ bridge: CoreBridge) async throws -> [TaskItem] {
    guard case let .tasks(rows) = try await bridge.query(.inbox) else { return [] }
    return rows
}

/// Poll rather than sleep a fixed span: the capture preview is debounced, and
/// a fixed wait is either flaky or slow.
@MainActor
private func settle(
    within limit: Duration = .seconds(2),
    until ready: () -> Bool
) async throws {
    let deadline = ContinuousClock.now.advanced(by: limit)
    while !ready(), ContinuousClock.now < deadline {
        try await Task.sleep(for: .milliseconds(10))
    }
}

// MARK: - The intents, actually invoked

/// The App Intents surface, run through `perform()`.
///
/// `perform()` is the method the App Intents runtime calls, and calling
/// anything else here would miss the one defect this whole surface was audited
/// for: an `AppIntent` that compiles, links, and has never been executed.
///
/// `.serialized` because ``IntentVault`` is process-wide by construction —
/// there is one vault per process and one slot to put it in — so two of these
/// at once would be two tests sharing one vault.
@MainActor
@Suite(.serialized)
struct IntentPerformTests {
    /// The claim `parity-matrix.md` §Capture-surface portability actually
    /// makes: a task captured from Shortcuts is *the same task* ⌘⇧N would have
    /// made. Not "similar" — every parsed facet, field for field.
    @Test
    func captureIsIdenticalToTheQuickCapturePanel() async throws {
        let vault = try await TestVault()
        defer { IntentVault.override = nil }
        IntentVault.override = vault.bridge
        let line = "Renew the passport !2 ~90m"

        // The panel's path: `CaptureModel` parses, and the draft it produced
        // is what gets submitted.
        let model = CaptureModel(bridge: vault.bridge, debounce: .milliseconds(1))
        model.text = line
        try await settle { model.preview != nil }
        let panelDraft = try #require(model.takeDraft())
        let panelOutcome = try await vault.bridge.submit(.createTask(draft: panelDraft))

        // The intent's path, end to end.
        _ = try await CaptureTaskIntent(line: line).perform()

        let fromPanel = try await readTask(panelOutcome.entity, in: vault.bridge)
        let rows = try await inboxRows(vault.bridge)
        let twin = rows.first { $0.title == panelDraft.title && $0.id != panelOutcome.entity }
        let fromIntent = try #require(twin, "the intent captured nothing")

        #expect(fromIntent.title == fromPanel.title)
        #expect(fromIntent.priority == fromPanel.priority)
        #expect(fromIntent.estimatedDurationS == fromPanel.estimatedDurationS)
        #expect(fromIntent.streamId == fromPanel.streamId)
        #expect(fromIntent.contexts == fromPanel.contexts)
        #expect(fromIntent.scheduledAt == fromPanel.scheduledAt)
        #expect(fromIntent.dueAt == fromPanel.dueAt)
        #expect(fromIntent.energy == fromPanel.energy)
        await vault.bridge.shutdown()
    }

    /// A capture naming no `#stream` lands in the Inbox, which is what
    /// `inbox-and-capture.md` requires of every surface outside the app.
    @Test
    func captureLandsInTheInbox() async throws {
        let vault = try await TestVault()
        defer { IntentVault.override = nil }
        IntentVault.override = vault.bridge

        _ = try await CaptureTaskIntent(line: "Buy stamps").perform()

        let rows = try await inboxRows(vault.bridge)
        #expect(rows.map(\.title) == ["Buy stamps"])
        await vault.bridge.shutdown()
    }

    @Test
    func completingThroughTheIntentMarksTheTaskDone() async throws {
        let vault = try await TestVault()
        defer { IntentVault.override = nil }
        IntentVault.override = vault.bridge

        let id = try await makeTask("File the receipts", in: vault.bridge)
        let entity = TaskEntity(id: id, title: "File the receipts", isCompleted: false)
        _ = try await CompleteTaskIntent(task: entity).perform()

        let item = try await readTask(id, in: vault.bridge)
        #expect(item.state == .done)
        await vault.bridge.shutdown()
    }

    @Test
    func bothListIntentsRunAndAnswer() async throws {
        let vault = try await TestVault()
        defer { IntentVault.override = nil }
        IntentVault.override = vault.bridge
        _ = try await makeTask("Still waiting", in: vault.bridge)

        _ = try await TodayTasksIntent().perform()
        _ = try await InboxTasksIntent().perform()

        let inbox = try await TaskListReader.rows(.inbox, in: vault.bridge)
        #expect(inbox.map(\.title) == ["Still waiting"])
        let today = try await TaskListReader.rows(.today, in: vault.bridge)
        #expect(today.isEmpty, "nothing was scheduled, so today is empty")
        await vault.bridge.shutdown()
    }

    @Test
    func aFocusSessionStartsAndEndsThroughTheIntents() async throws {
        let vault = try await TestVault()
        defer { IntentVault.override = nil }
        IntentVault.override = vault.bridge

        let id = try await makeTask("Write the report", in: vault.bridge)
        let entity = TaskEntity(id: id, title: "Write the report", isCompleted: false)
        _ = try await StartFocusIntent(task: entity, length: .pomodoro).perform()
        let opened = try await FocusSessions.running(in: vault.bridge)
        #expect(opened != nil)

        _ = try await EndFocusIntent(completingTask: true).perform()
        let closed = try await FocusSessions.running(in: vault.bridge)
        #expect(closed == nil)
        let finished = try await readTask(id, in: vault.bridge)
        #expect(finished.state == .done, "ending with complete finishes the task")
        await vault.bridge.shutdown()
    }

    /// Voice needs this one: without a string query Siri can only offer a list
    /// to point at, which is not something you can do hands-free.
    @Test
    func tasksAreFoundByTypedText() async throws {
        let vault = try await TestVault()
        defer { IntentVault.override = nil }
        IntentVault.override = vault.bridge

        let id = try await makeTask("Renew the passport", in: vault.bridge)
        _ = try await makeTask("Buy stamps", in: vault.bridge)

        let matches = try await TaskEntityQuery().entities(matching: "passport")
        #expect(matches.map(\.id) == [id])
        await vault.bridge.shutdown()
    }

    /// One stale entry in a shortcut's saved list must not lose the rest of it.
    @Test
    func idsRecordedByAShortcutStillResolve() async throws {
        let vault = try await TestVault()
        defer { IntentVault.override = nil }
        IntentVault.override = vault.bridge

        let live = try await makeTask("Renew the passport", in: vault.bridge)
        let gone = try await makeTask("Cancelled plan", in: vault.bridge)
        _ = try await vault.bridge.submit(.deleteTask(id: gone))

        let found = try await TaskEntityQuery().entities(for: [live, gone, "not-an-id"])
        #expect(found.map(\.title) == ["Renew the passport"])
        await vault.bridge.shutdown()
    }

    /// Suggestions never open a vault. The system asks for them speculatively,
    /// and decrypting a vault to fill a picker nobody opened is background work
    /// this surface must not cause.
    @Test
    func suggestionsAreEmptyWhenNoVaultIsOpen() async throws {
        IntentVault.override = nil
        IntentVault.adopt(nil)
        let suggestions = try await TaskEntityQuery().suggestedEntities()
        #expect(suggestions.isEmpty)
    }

    /// The hand-over path: given the app's own bridge, an intent uses it and —
    /// the half that matters — leaves it open. Closing a bridge the window is
    /// still using would take the app's vault away with it.
    @Test
    func anAdoptedVaultIsUsedAndLeftOpen() async throws {
        let vault = try await TestVault()
        defer { IntentVault.adopt(nil) }
        IntentVault.override = nil
        IntentVault.adopt(vault.bridge)

        _ = try await CaptureTaskIntent(line: "Through the app's own vault").perform()

        let rows = try await inboxRows(vault.bridge)
        #expect(rows.map(\.title) == ["Through the app's own vault"])
        #expect(IntentVault.existing() != nil, "the lease logic must not have closed it")
        await vault.bridge.shutdown()
    }
}

// MARK: - Capture

/// Capture, at the level the parity requirement is actually about.
struct CaptureIntentTests {
    /// The parsed facets survive the trip, which is how you can tell the
    /// intent is not doing a parse of its own.
    @Test
    func captureAppliesTheParsedFacets() async throws {
        let vault = try await TestVault()
        let captured = try await CaptureTaskIntent.capture(
            "Draft the letter !1 ~45m",
            into: vault.bridge
        )
        let item = try await readTask(captured.task.id, in: vault.bridge)
        #expect(item.title == "Draft the letter")
        #expect(item.priority == 1)
        #expect(item.estimatedDurationS == 45 * 60)
        #expect(captured.message == "Captured “Draft the letter”.")
        await vault.bridge.shutdown()
    }

    /// An unresolvable token is *reported*. A capture surface with no screen
    /// has no other way to say "there is no Stream called that", and filing it
    /// somewhere the user did not mean is the quiet failure this must not have.
    @Test
    func captureReportsWhatTheParserCouldNotApply() async throws {
        let vault = try await TestVault()
        let captured = try await CaptureTaskIntent.capture(
            "Ring the bank #nosuchstream",
            into: vault.bridge
        )
        #expect(captured.message.contains("nosuchstream"))
        #expect(captured.message.contains("kept in the title"))
        await vault.bridge.shutdown()
    }

    /// Beyond the first, issues are counted rather than recited: a spoken
    /// reply listing six of them is a reply nobody hears the end of.
    @Test
    func captureSummarisesMoreThanOneIssue() {
        let message = CaptureTaskIntent.message(
            title: "Ring the bank",
            issues: [.unknownStream(name: "one"), .unknownContext(name: "two")]
        )
        #expect(message.contains("And 1 more."))
    }

    @Test
    func aCleanCaptureSaysOnlyWhatItCaptured() {
        #expect(CaptureTaskIntent.message(title: "Buy milk", issues: []) == "Captured “Buy milk”.")
    }

    @Test
    func capturingNothingIsAnErrorRatherThanAnEmptyTask() async throws {
        let vault = try await TestVault()
        await #expect(throws: IntentError.nothingToCapture) {
            _ = try await CaptureTaskIntent.capture("   \n ", into: vault.bridge)
        }
        await vault.bridge.shutdown()
    }
}

// MARK: - Tasks

struct TaskIntentTests {
    /// Completing something already done is reported, not repeated: a shortcut
    /// run twice must not write a second op.
    @Test
    func completingAFinishedTaskSaysSoWithoutWritingAgain() async throws {
        let vault = try await TestVault()
        let id = try await makeTask("File the receipts", in: vault.bridge)
        _ = try await CompleteTaskIntent.complete(id, in: vault.bridge)
        let again = try await CompleteTaskIntent.complete(id, in: vault.bridge)
        #expect(again.message == "“File the receipts” was already done.")
        #expect(again.task.isCompleted)
        await vault.bridge.shutdown()
    }

    /// A task deleted between being picked in Shortcuts and the shortcut
    /// running is reported. This is the read-before-write in `complete`.
    @Test
    func completingAVanishedTaskReportsIt() async throws {
        let vault = try await TestVault()
        let id = try await makeTask("File the receipts", in: vault.bridge)
        _ = try await vault.bridge.submit(.deleteTask(id: id))
        await #expect(throws: IntentError.taskNotFound(id)) {
            _ = try await CompleteTaskIntent.complete(id, in: vault.bridge)
        }
        await vault.bridge.shutdown()
    }

    /// What the completion released, asked of the core rather than guessed at.
    @Test
    func completingSaysWhatItUnblocked() async throws {
        let vault = try await TestVault()
        let blocker = try await makeTask("Get the form", in: vault.bridge)
        _ = try await makeTask("Post the letter", in: vault.bridge, blockedBy: [blocker])

        let done = try await CompleteTaskIntent.complete(blocker, in: vault.bridge)
        #expect(done.message == "Completed “Get the form”. That unblocked 1 task.")
        await vault.bridge.shutdown()
    }

    @Test
    func theUnblockedCountIsPluralised() {
        #expect(CompleteTaskIntent.message(title: "a", released: 0) == "Completed “a”.")
        #expect(CompleteTaskIntent.message(title: "a", released: 1).contains("1 task."))
        #expect(CompleteTaskIntent.message(title: "a", released: 2).contains("2 tasks"))
    }

    /// A list of things to do that included the things already done would not
    /// be a list of things to do.
    @Test
    func theInboxIntentReturnsOpenTasksOnly() async throws {
        let vault = try await TestVault()
        let done = try await makeTask("Triaged already", in: vault.bridge)
        _ = try await makeTask("Still waiting", in: vault.bridge)
        _ = try await vault.bridge.submit(.completeTask(id: done))

        let rows = try await TaskListReader.rows(.inbox, in: vault.bridge)
        #expect(rows.map(\.title) == ["Still waiting"])
        await vault.bridge.shutdown()
    }

    @Test
    func anEmptyListStillSaysSomething() {
        #expect(TaskListReader.message([], empty: "The inbox is empty.") == "The inbox is empty.")
        let one = TaskEntity(id: "tsk_1", title: "One", isCompleted: false)
        #expect(TaskListReader.message([one], empty: "x") == "1 task: One.")
        let two = TaskEntity(id: "tsk_2", title: "Two", isCompleted: false)
        #expect(TaskListReader.message([one, two], empty: "x") == "2 tasks: One, Two.")
    }
}

// MARK: - Focus

struct FocusIntentTests {
    /// The core allows two open sessions and the focus screen shows one, so a
    /// second start is refused rather than opened where nothing can show it.
    @Test
    func aSecondSessionIsRefusedRatherThanHidden() async throws {
        let vault = try await TestVault()
        let first = try await makeTask("Write the report", in: vault.bridge)
        let second = try await makeTask("Read the brief", in: vault.bridge)
        _ = try await StartFocusIntent.start(on: first, length: .pomodoro, in: vault.bridge)

        await #expect(throws: IntentError.focusAlreadyRunning("Write the report")) {
            _ = try await StartFocusIntent.start(on: second, length: .untilDone, in: vault.bridge)
        }
        await vault.bridge.shutdown()
    }

    @Test
    func endingNothingReportsThatRatherThanSucceeding() async throws {
        let vault = try await TestVault()
        await #expect(throws: IntentError.noFocusRunning) {
            _ = try await EndFocusIntent.end(completingTask: false, in: vault.bridge)
        }
        await vault.bridge.shutdown()
    }

    /// Ending without "complete" leaves the task alone. The one automatic
    /// completion in Sunrise is the user saying they finished the work.
    @Test
    func endingWithoutCompletingLeavesTheTaskOpen() async throws {
        let vault = try await TestVault()
        let id = try await makeTask("Write the report", in: vault.bridge)
        _ = try await StartFocusIntent.start(on: id, length: .pomodoro, in: vault.bridge)
        let message = try await EndFocusIntent.end(completingTask: false, in: vault.bridge)

        #expect(message.hasPrefix("Session ended after "))
        let item = try await readTask(id, in: vault.bridge)
        #expect(item.state != .done)
        await vault.bridge.shutdown()
    }

    @Test
    func startingOnAVanishedTaskReportsIt() async throws {
        let vault = try await TestVault()
        let id = try await makeTask("Write the report", in: vault.bridge)
        _ = try await vault.bridge.submit(.deleteTask(id: id))
        await #expect(throws: IntentError.taskNotFound(id)) {
            _ = try await StartFocusIntent.start(on: id, length: .pomodoro, in: vault.bridge)
        }
        await vault.bridge.shutdown()
    }

    @Test
    func everySessionLengthMapsToTheCoresOwn() {
        #expect(FocusLength.pomodoro.sessionLength == .onePomodoro)
        #expect(FocusLength.estimate.sessionLength == .sizedToEstimate)
        #expect(FocusLength.untilDone.sessionLength == .untilDone)
        #expect(
            FocusLength.allCases.count == FocusLength.caseDisplayRepresentations.count,
            "a case with no display name is a blank row in the Shortcuts picker"
        )
    }
}

// MARK: - The surface itself

struct IntentSurfaceTests {
    /// Every failure says something. A blank sentence in Shortcuts is the
    /// silent failure `IntentError` exists to prevent.
    @Test
    func everyErrorReadsAsASentence() {
        let cases: [IntentError] = [
            .vaultUnavailable,
            .noVault,
            .vaultLocked("because"),
            .vaultHeldByThisApp,
            .vaultFailed("because"),
            .vaultBusy,
            .nothingToCapture,
            .emptyTitle,
            .taskNotFound("tsk_1"),
            .focusAlreadyRunning("Write the report"),
            .noFocusRunning,
            .core("because")
        ]
        for error in cases {
            let text = String(localized: error.localizedStringResource)
            #expect(text.count > 10, "\(error) has nothing to say")
        }
    }

    @Test
    func anythingThrownByTheSeamIsWrappedExactlyOnce() {
        let original = IntentError.noFocusRunning
        #expect(IntentError.wrapping(original) == original)
        #expect(IntentError.wrapping(CocoaError(.fileNoSuchFile)) != original)
    }

    /// The provider is what puts these in Spotlight and the Shortcuts gallery
    /// with nothing assembled by hand. Ten is the system's cap per app.
    @Test
    func everyVerbHasASpokenShortcut() {
        #expect(SunriseShortcuts.appShortcuts.count == 6)
        #expect(SunriseShortcuts.appShortcuts.count <= 10, "the system takes ten per app")
    }

    @Test
    func aTaskEntityShowsItsTitle() {
        let entity = TaskEntity(id: "tsk_1", title: "Renew the passport", isCompleted: false)
        #expect(String(describing: entity.displayRepresentation.title).contains("passport"))
    }
}

/// The App Intents *metadata bundle*, which is the difference between an
/// intent that exists and an intent the system will ever run.
///
/// `appintentsmetadataprocessor` writes `Metadata.appintents` into the app
/// while it builds. Nothing in Swift references it, so nothing in Swift would
/// notice it going missing — a project change dropping the AppIntents link
/// would leave every intent above compiling, passing, and invisible to
/// Spotlight, Shortcuts and Siri.
///
/// These tests are hosted by the app (`TEST_HOST`), so `Bundle.main` *is*
/// `Sunrise.app`.
struct AppIntentsMetadataTests {
    @Test
    func theBuiltAppCarriesItsIntentMetadata() throws {
        let bundle = Bundle.main
        try #require(
            bundle.bundleURL.pathExtension == "app",
            "expected to be hosted by Sunrise.app; got \(bundle.bundleURL.path)"
        )
        let metadata = bundle.bundleURL
            .appending(path: "Contents/Resources/Metadata.appintents")
        #expect(
            FileManager.default.fileExists(atPath: metadata.path(percentEncoded: false)),
            """
            No Metadata.appintents in the built app: the intents will not be \
            offered by Spotlight, Shortcuts or Siri. Check that project.yml \
            still links AppIntents.framework on the Sunrise target.
            """
        )
    }
}
