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
/// keeping a push and a refresh from overlapping, and cancelling the run in
/// flight when the OS ends the budget.
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

    private let scheduler: any BackgroundTaskScheduling
    private let now: () -> Date
    private let run: @MainActor () async -> BackgroundSyncResult
    /// The run in flight, if any. A second caller joins it rather than
    /// starting another: a push that lands during a refresh wants the same
    /// answer, and two syncs at once would only race each other's sessions.
    private var inFlight: Task<BackgroundSyncResult, Never>?

    init(
        scheduler: any BackgroundTaskScheduling,
        now: @escaping () -> Date = Date.init,
        run: @escaping @MainActor () async -> BackgroundSyncResult
    ) {
        self.scheduler = scheduler
        self.now = now
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
    /// second.
    func sync() async -> BackgroundSyncResult {
        if let inFlight { return await inFlight.value }
        let task = Task { await run() }
        inFlight = task
        let result = await task.value
        if inFlight == task { inFlight = nil }
        return result
    }

    /// The OS is ending the budget. Cancels the run in flight, which cancels
    /// the core's wait; what already landed stays, and nothing is half
    /// applied (`SunriseCore.syncOnce`).
    func expire() {
        inFlight?.cancel()
    }
}
