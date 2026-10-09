import Foundation
import Testing

@testable import Sunrise

/// `BGTaskScheduler`, recorded rather than reached.
@MainActor
private final class RecordingScheduler: BackgroundTaskScheduling {
    var refreshes: [Date] = []
    var maintenance: [Date] = []
    var refuses = false

    func submitRefresh(earliestBegin: Date) throws {
        if refuses { throw RefusedRequest() }
        refreshes.append(earliestBegin)
    }

    func submitMaintenance(earliestBegin: Date) throws {
        if refuses { throw RefusedRequest() }
        maintenance.append(earliestBegin)
    }
}

private struct RefusedRequest: Error {}

private let launch = Date(timeIntervalSince1970: 1_800_000_000)

/// A coordinator on a fixed clock, so the dates it asks for are exact.
@MainActor
private func background(
    _ scheduler: RecordingScheduler = RecordingScheduler(),
    run: @escaping @MainActor () async -> BackgroundSyncResult
) -> BackgroundSync {
    BackgroundSync(scheduler: scheduler, now: { launch }, run: run)
}

@MainActor
struct BackgroundSyncTests {
    /// A run every refresh re-arms: a refresh that is not followed by another
    /// request is the last one the app ever gets.
    @Test
    func aRefreshReArmsTheNextOneFifteenMinutesOut() async {
        let scheduler = RecordingScheduler()
        let sync = background(scheduler) { .noData }

        let result = await sync.refresh()

        #expect(result == .noData)
        #expect(scheduler.refreshes == [launch.addingTimeInterval(15 * 60)])
        #expect(scheduler.maintenance.isEmpty)
    }

    @Test
    func aMaintenanceWindowReArmsTheNextOne() async {
        let scheduler = RecordingScheduler()
        let sync = background(scheduler) { .newData }

        #expect(await sync.maintain() == .newData)
        #expect(scheduler.maintenance == [launch.addingTimeInterval(BackgroundSync.maintenanceInterval)])
    }

    /// Background refresh switched off for the app makes every request
    /// throw. The run still happens: a push or the foreground still syncs.
    @Test
    func aRefusedRequestDoesNotStopTheRun() async {
        let scheduler = RecordingScheduler()
        scheduler.refuses = true
        var runs = 0
        let sync = background(scheduler) {
            runs += 1
            return .newData
        }

        #expect(await sync.refresh() == .newData)
        #expect(runs == 1)
    }

    /// A push landing while a refresh runs joins it: one sync, one answer for
    /// both callers.
    @Test
    func aSecondCallerJoinsTheRunInFlight() async {
        let gate = AsyncStream<Void>.makeStream()
        var runs = 0
        let sync = background {
            runs += 1
            for await _ in gate.stream { break }
            return .newData
        }

        async let refresh = sync.refresh()
        while !sync.isRunning { await _Concurrency.Task.yield() }
        // The push arrives while the refresh's run is held open, and is
        // given every chance to start a second run before the first ends.
        let push = _Concurrency.Task { await sync.sync() }
        for _ in 0..<100 { await _Concurrency.Task.yield() }
        gate.continuation.yield()
        // A second run, had there been one, fails the count below rather
        // than waiting on the gate forever.
        gate.continuation.finish()

        #expect(await refresh == .newData)
        #expect(await push.value == .newData)
        #expect(runs == 1)
        #expect(!sync.isRunning)
    }

    /// Single-flight is about overlap, not about running once: the next call
    /// after a run finishes starts a new one.
    @Test
    func aFinishedRunDoesNotAnswerTheNextCall() async {
        var runs = 0
        let sync = background {
            runs += 1
            return runs == 1 ? .newData : .noData
        }

        #expect(await sync.sync() == .newData)
        #expect(await sync.sync() == .noData)
        #expect(runs == 2)
    }

    /// The OS ending the budget cancels the run in flight; the run sees the
    /// cancellation and reports failure promptly rather than outliving the
    /// task it belonged to.
    @Test
    func expiryCancelsTheRunInFlight() async {
        let sync = background {
            try? await _Concurrency.Task.sleep(for: .seconds(600))
            return _Concurrency.Task.isCancelled ? .failed : .newData
        }

        async let result = sync.refresh()
        while !sync.isRunning { await _Concurrency.Task.yield() }
        sync.expire()

        #expect(await result == .failed)
        #expect(!sync.isRunning)
    }

    /// What the OS is told, from what the core reports.
    @Test
    func theOutcomeMapsToTheFetchResult() {
        func outcome(completed: Bool, changes: UInt64) -> SyncOnceOutcome {
            SyncOnceOutcome(completed: completed, changes: changes, outboxPending: 0, state: .live)
        }
        #expect(BackgroundSyncResult(outcome(completed: true, changes: 3)) == .newData)
        #expect(BackgroundSyncResult(outcome(completed: true, changes: 0)) == .noData)
        #expect(BackgroundSyncResult(outcome(completed: false, changes: 0)) == .failed)
        // What landed before the budget ran out landed whole, and is news.
        #expect(BackgroundSyncResult(outcome(completed: false, changes: 2)) == .newData)
    }
}

/// The guardrails a background run keeps against the session: it opens a
/// vault nobody chose to keep closed, and nothing else.
@MainActor
struct SessionBackgroundSyncTests {
    private actor OpenCount {
        private(set) var opens = 0
        func opened() { opens += 1 }
    }

    private struct NotOpened: Error {}

    private func makeSession(root: Data?, count: OpenCount) -> SessionModel {
        SessionModel(
            location: VaultLocation(
                directory: FileManager.default.temporaryDirectory
                    .appending(path: "sunrise-tests-\(UUID().uuidString)")
            ),
            rootStore: StubRootStore(root: root),
            appVersion: "test",
            openBridge: { _, _, _, _ in
                await count.opened()
                // Long enough that a second `start()` arrives while this one
                // is still opening.
                try await _Concurrency.Task.sleep(for: .milliseconds(50))
                throw NotOpened()
            }
        )
    }

    private let root = Data(repeating: 7, count: 32)

    /// A cold background launch opens the vault itself, and a window
    /// appearing meanwhile joins that open rather than racing it into the
    /// vault lock.
    @Test
    func aColdLaunchOpensOnceHoweverManyAskAtOnce() async {
        let count = OpenCount()
        let session = makeSession(root: root, count: count)

        async let background = session.backgroundSync()
        async let window: Void = session.start()
        _ = await (background, window)

        #expect(await count.opens == 1)
    }

    /// A vault the user locked stays locked: the background run does not
    /// reach for the key.
    @Test
    func aLockedVaultIsNotOpenedInTheBackground() async {
        let count = OpenCount()
        let session = makeSession(root: root, count: count)
        await session.lock()

        #expect(await session.backgroundSync() == .noData)
        #expect(await count.opens == 0)
        #expect(session.phase == .locked(.lockedByUser))
    }

    /// A failed open waits for someone to look at it rather than being
    /// retried, silently, every fifteen minutes.
    @Test
    func aFailedOpenIsNotRetriedInTheBackground() async {
        let count = OpenCount()
        let session = makeSession(root: root, count: count)
        await session.start()
        #expect(await count.opens == 1)

        #expect(await session.backgroundSync() == .noData)
        #expect(await count.opens == 1)
    }

    /// No vault yet: nothing to sync, and nothing created.
    @Test
    func aFirstRunHasNothingToSync() async {
        let count = OpenCount()
        let session = makeSession(root: nil, count: count)

        #expect(await session.backgroundSync() == .noData)
        #expect(session.phase == .firstRun)
        #expect(await count.opens == 0)
    }
}
