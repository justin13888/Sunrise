import Foundation
import Testing

@testable import Sunrise

/// Focus against a real vault: the planner, the session, interruptions and
/// the cascade.
@MainActor
struct FocusModelTests {
    @Test
    func thePlannerRanksAndExplainsItself() async throws {
        let vault = try await TestVault()
        let blocker = try await create(vault, "Draft the letter")
        _ = try await create(vault, "Post the letter", blockedBy: [blocker])
        let model = FocusModel(bridge: vault.bridge)

        await model.refresh()

        // "Post the letter" is blocked, so it is not offered; "Draft the
        // letter" unblocks it, and the seam says so in the domain's words.
        #expect(model.plan.map(\.task.title) == ["Draft the letter"])
        let row = try #require(model.plan.first)
        #expect(row.unblocks == 1)
        #expect(row.reason.contains("unblocks 1 task"), "\(row.reason)")
        await vault.bridge.shutdown()
    }

    /// The clock is derived from the core's, not from a local stopwatch. A
    /// session started "now" reads 0, and one started ten minutes ago reads
    /// ten minutes — without anything having ticked.
    @Test
    func theTimerIsDerivedFromTheCoreClock() async throws {
        let vault = try await TestVault()
        _ = try await create(vault, "Write the brief")
        let model = FocusModel(bridge: vault.bridge)
        await model.refresh()
        await model.start(try #require(model.plan.first))

        let session = try #require(model.running)
        #expect(model.progress?.running == true)
        #expect(model.progress?.overran == false)

        // Ask the seam what the same session would read later on. Nothing
        // ticks; the numbers are a function of the clock, which is the whole
        // reason there is no stopwatch here.
        let startedAt = UInt64(session.start.startedAt)
        let tenMinutes = sessionProgress(session: session, nowMs: startedAt + 600_000)
        #expect(tenMinutes.focusedMs == 600_000)
        #expect(tenMinutes.clock == "10:00")
        #expect(tenMinutes.remainingClock == "15:00", "a pomodoro is 25 minutes")
        #expect(!tenMinutes.overran)

        let past = sessionProgress(session: session, nowMs: startedAt + 1_800_000)
        #expect(past.overran, "half an hour is past the plan")
        #expect(past.remainingMs == 0)
        model.stopTicking()
        await vault.bridge.shutdown()
    }

    /// An `until done` session has no remaining time. Not zero — zero would
    /// read as "you are out of time" for a session that never had a limit.
    @Test
    func anOpenEndedSessionHasNoRemainingTime() async throws {
        let vault = try await TestVault()
        _ = try await create(vault, "Write the brief")
        let model = FocusModel(bridge: vault.bridge)
        model.length = .untilDone
        await model.refresh()
        await model.start(try #require(model.plan.first))

        let progress = try #require(model.progress)
        #expect(progress.remainingMs == nil)
        #expect(progress.remainingClock == nil)
        #expect(!progress.overran)
        model.stopTicking()
        await vault.bridge.shutdown()
    }

    @Test
    func anInterruptionIsLoggedAgainstTheRunningSession() async throws {
        let vault = try await TestVault()
        _ = try await create(vault, "Write the brief")
        let model = FocusModel(bridge: vault.bridge)
        await model.refresh()
        await model.start(try #require(model.plan.first))

        await model.logInterruption(.meeting)

        let session = try #require(model.running)
        #expect(session.interruptions.count == 1)
        #expect(session.interruptions.first?.reason == .meeting)
        #expect(interruptionLabel(reason: .meeting) == "meeting")
        model.stopTicking()
        await vault.bridge.shutdown()
    }

    /// Finishing a session with "Done" completes the task and shows what that
    /// released — including what is still waiting on something else.
    @Test
    func finishingASessionCompletesTheTaskAndShowsTheCascade() async throws {
        let vault = try await TestVault()
        let first = try await create(vault, "Draft the letter")
        let second = try await create(vault, "Sign the letter")
        _ = try await create(vault, "Post the letter", blockedBy: [first, second])
        let model = FocusModel(bridge: vault.bridge)
        await model.refresh()
        let row = try #require(model.plan.first { $0.task.title == "Draft the letter" })
        await model.start(row)

        await model.end(completingTask: true)

        #expect(model.running == nil)
        let cascade = try #require(model.cascade)
        #expect(cascade.completed == first)
        // "Post the letter" is still waiting on "Sign the letter".
        #expect(cascade.released.isEmpty)
        #expect(cascade.stillBlocked.count == 1)
        model.dismissCascade()
        #expect(model.cascade == nil)
        await vault.bridge.shutdown()
    }

    /// Stopping without completing leaves the task open and shows no cascade —
    /// nothing was released.
    @Test
    func stoppingWithoutCompletingReleasesNothing() async throws {
        let vault = try await TestVault()
        _ = try await create(vault, "Write the brief")
        let model = FocusModel(bridge: vault.bridge)
        await model.refresh()
        await model.start(try #require(model.plan.first))

        await model.end(completingTask: false)

        #expect(model.running == nil)
        #expect(model.cascade == nil)
        #expect(model.plan.map(\.task.title) == ["Write the brief"])
        await vault.bridge.shutdown()
    }

    /// Ended sessions fold into the totals the strip shows.
    @Test
    func endedSessionsFoldIntoTheStats() async throws {
        let vault = try await TestVault()
        _ = try await create(vault, "Write the brief")
        let model = FocusModel(bridge: vault.bridge)
        await model.refresh()
        #expect(model.stats?.workSessions == 0)

        await model.start(try #require(model.plan.first))
        await model.logInterruption(.selfInterrupt)
        await model.end(completingTask: false)

        let stats = try #require(model.stats)
        #expect(stats.workSessions == 1)
        #expect(stats.running == 0)
        #expect(stats.interruptions == 1)
        await vault.bridge.shutdown()
    }

    /// A session started elsewhere and synced in is the same session. Finding
    /// one running on refresh is a normal state, not an error.
    @Test
    func aSessionAlreadyRunningIsPickedUpOnRefresh() async throws {
        let vault = try await TestVault()
        let task = try await create(vault, "Write the brief")
        _ = try await vault.bridge.submit(
            .startFocus(taskId: task, kind: .work, length: .onePomodoro, energy: nil)
        )

        let model = FocusModel(bridge: vault.bridge)
        await model.refresh()

        #expect(model.isRunning)
        #expect(model.runningTitle == "Write the brief")
        model.stopTicking()
        await vault.bridge.shutdown()
    }

    /// The words on the pickers are the domain's, so a session sized "until
    /// done" reads the same here as in the explanation on the row beside it.
    @Test
    func theSessionWordsComeFromTheSeam() {
        #expect(sessionLengthLabel(length: .onePomodoro) == "one pomodoro")
        #expect(sessionLengthLabel(length: .untilDone) == "until done")
        #expect(energyFitLabel(fit: .over) == "over")
        #expect(SessionLength.offered.count == 3)
    }

    private func create(
        _ vault: borrowing TestVault,
        _ title: String,
        blockedBy: [EntityRef] = []
    ) async throws -> EntityRef {
        let outcome = try await vault.bridge.submit(.createTask(draft: TaskDraftIn(
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
            _ = try await vault.bridge.submit(.updateTask(id: outcome.entity, edit: edit))
        }
        return outcome.entity
    }
}
