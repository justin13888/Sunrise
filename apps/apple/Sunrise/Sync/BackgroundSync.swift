import Foundation

/// What one background run tells the OS.
///
/// The three answers a silent push's fetch handler has, named after them.
/// A refresh task has only success or failure, and reads ``failed`` as the one
/// failure: a run that found nothing new still did its job.
enum BackgroundSyncResult: Equatable {
    /// The vault changed: something arrived, or something local went out.
    case newData
    /// The run finished, or had nothing it was allowed to do — the vault is
    /// locked, or sync is off — and nothing changed.
    case noData
    /// The run could not finish inside its budget, or the relay refused it.
    case failed

    /// Read a core outcome the way the OS wants it read. A change counts even
    /// from a run that did not finish: what landed, landed whole.
    init(_ outcome: SyncOnceOutcome) {
        if outcome.changes > 0 {
            self = .newData
        } else if outcome.completed {
            self = .noData
        } else {
            self = .failed
        }
    }
}

/// The two requests the app makes of the OS scheduler, behind a protocol so a
/// test can stand in for `BGTaskScheduler`, which only a device or simulator
/// process can talk to.
@MainActor
protocol BackgroundTaskScheduling {
    /// Ask for the next app refresh, not before `earliestBegin`.
    func submitRefresh(earliestBegin: Date) throws
    /// Ask for a maintenance window on external power, not before
    /// `earliestBegin`.
    func submitMaintenance(earliestBegin: Date) throws
}

/// Background sync, as `docs/07-clients/mobile-ios.md` §Background sync and
/// §Push handling describe it: an OS-scheduled refresh every fifteen minutes
/// at the soonest, a maintenance window on power, and a silent push that runs
/// the same sync — never two at once.
///
/// Holds no vault. `run` is the one sync, supplied by the app, and everything
/// here is about *when* it runs and *how many* run: re-arming the refresh,
/// keeping a push and a refresh from overlapping, and ending the run in
/// flight when the OS ends the budget or ``runDeadline`` passes.
///
/// The OS's deadline is kept here, not by `run`: whatever `run` is doing
/// when the budget ends — opening the vault, renewing a token, syncing —
/// every caller is answered `.failed` at once, so the OS is told before it
/// kills the app, and the run is cancelled to wind down behind that answer.
@MainActor
final class BackgroundSync {
    /// The refresh task's identifier. Listed in the app's
    /// `BGTaskSchedulerPermittedIdentifiers` (`project.yml`); a mismatch makes
    /// `BGTaskScheduler.submit` throw.
    static let refreshTaskID = "dev.sunrise.SunriseiOS.refresh"
    /// The maintenance task's identifier, listed beside it.
    static let maintenanceTaskID = "dev.sunrise.SunriseiOS.maintenance"
    /// The soonest the next refresh may run: the widget's refresh cadence
    /// (`mobile-ios.md` §Widget refresh cadence). The OS treats it as a floor
    /// and usually runs later.
    static let refreshInterval: TimeInterval = 15 * 60
    /// The soonest the next maintenance window may open.
    static let maintenanceInterval: TimeInterval = 24 * 60 * 60

    /// The longest a run may take before its callers are answered `.failed`
    /// regardless. A silent push has no expiration handler and gets about
    /// thirty seconds, as a refresh task does; this answers inside that with
    /// a margin for the answer itself to reach the OS.
    static let runDeadline: Duration = .seconds(28)

    private let scheduler: any BackgroundTaskScheduling
    private let now: () -> Date
    private let deadline: Duration
    private let run: @MainActor () async -> BackgroundSyncResult
    /// The run in flight, if any. A second caller joins it rather than
    /// starting another: a push that lands during a refresh wants the same
    /// answer, and two syncs at once would only race each other's sessions.
    /// Held until the run actually ends, even after an expiry has answered
    /// its callers, so a run winding down is never overlapped by a new one.
    private var inFlight: Task<BackgroundSyncResult, Never>?
    /// The callers waiting on the run in flight: answered with its result
    /// when it ends, or with `.failed` the moment the budget ends.
    private var waiters: [CheckedContinuation<BackgroundSyncResult, Never>] = []
    /// Expires the run in flight at ``runDeadline``.
    private var watchdog: Task<Void, Never>?

    init(
        scheduler: any BackgroundTaskScheduling,
        now: @escaping () -> Date = Date.init,
        deadline: Duration = BackgroundSync.runDeadline,
        run: @escaping @MainActor () async -> BackgroundSyncResult
    ) {
        self.scheduler = scheduler
        self.now = now
        self.deadline = deadline
        self.run = run
    }

    /// Whether a run is in flight.
    var isRunning: Bool { inFlight != nil }

    /// Ask the OS for the next refresh. Called when the app goes to the
    /// background and at the start of every refresh, so there is always one
    /// pending: a refresh that is not re-armed is the last one.
    ///
    /// A request the OS refuses — background refresh switched off for the
    /// app, or a simulator that does not run these tasks — is dropped: the
    /// app works without it, and the next backgrounding asks again.
    func scheduleRefresh() {
        try? scheduler.submitRefresh(earliestBegin: now().addingTimeInterval(Self.refreshInterval))
    }

    /// Ask the OS for the next maintenance window, on the same terms.
    func scheduleMaintenance() {
        try? scheduler.submitMaintenance(
            earliestBegin: now().addingTimeInterval(Self.maintenanceInterval)
        )
    }

    /// The OS ran the refresh task: re-arm the next one first, so a run the OS
    /// cuts short still leaves one pending, then sync.
    func refresh() async -> BackgroundSyncResult {
        scheduleRefresh()
        return await sync()
    }

    /// The OS opened a maintenance window: re-arm, then sync. Compaction and
    /// the attachment cache trim join this once the core has them.
    func maintain() async -> BackgroundSyncResult {
        scheduleMaintenance()
        return await sync()
    }

    /// One sync, single-flight: joins the run in flight rather than starting a
    /// second. Answers by ``runDeadline`` at the latest.
    func sync() async -> BackgroundSyncResult {
        if inFlight == nil { start() }
        return await withCheckedContinuation { waiters.append($0) }
    }

    /// The OS is ending the budget. Answers every caller `.failed` now, and
    /// cancels the run in flight, which reaches the core's wait
    /// (`CoreBridge.syncOnce`); what already landed stays, and nothing is
    /// half applied.
    func expire() {
        guard let inFlight else { return }
        inFlight.cancel()
        answer(.failed)
    }

    private func start() {
        let task = Task { await run() }
        inFlight = task
        watchdog = Task { [weak self, deadline] in
            try? await Task.sleep(for: deadline)
            guard !Task.isCancelled else { return }
            self?.expire()
        }
        Task { [weak self] in
            let result = await task.value
            self?.finish(task, with: result)
        }
    }

    private func finish(_ task: Task<BackgroundSyncResult, Never>, with result: BackgroundSyncResult) {
        guard inFlight == task else { return }
        inFlight = nil
        watchdog?.cancel()
        watchdog = nil
        answer(result)
    }

    private func answer(_ result: BackgroundSyncResult) {
        let answered = waiters
        waiters = []
        for waiter in answered { waiter.resume(returning: result) }
    }
}
