import BackgroundTasks
import UIKit

/// `BGTaskScheduler`, as ``BackgroundSync`` asks it for things.
struct SystemBackgroundScheduler: BackgroundTaskScheduling {
    func submitRefresh(earliestBegin: Date) throws {
        let request = BGAppRefreshTaskRequest(identifier: BackgroundSync.refreshTaskID)
        request.earliestBeginDate = earliestBegin
        try BGTaskScheduler.shared.submit(request)
    }

    /// On power and on a network: maintenance is the work worth waiting for a
    /// charger to do, and the sync it opens with needs the relay.
    func submitMaintenance(earliestBegin: Date) throws {
        let request = BGProcessingTaskRequest(identifier: BackgroundSync.maintenanceTaskID)
        request.requiresExternalPower = true
        request.requiresNetworkConnectivity = true
        request.earliestBeginDate = earliestBegin
        try BGTaskScheduler.shared.submit(request)
    }
}

/// The process's background work: the scheduled tasks, the silent push and
/// the push token, bound to the one session the app owns.
///
/// One per process, because the OS's entry points are: `BGTaskScheduler`
/// handlers are registered once, before launch finishes, and the app
/// delegate is created by the system rather than by the scene. The session
/// is attached by ``SunriseiOSApp`` as it builds it, which happens before
/// launch finishes too.
@MainActor
final class BackgroundHost {
    static let shared = BackgroundHost()

    /// This device's APNs token and whether the relay holds it.
    let push = PushTokenRegistrar()
    /// The refresh, maintenance and push runs, once a session is attached.
    private(set) var sync: BackgroundSync?
    private var session: SessionModel?

    /// Bind to the app's session and surfaces. The run each background task
    /// performs: sync the vault, re-plan reminders from what arrived, redraw
    /// the widgets, and file the push token if the relay does not hold it.
    func attach(session: SessionModel, surfaces: AppSurfaces) {
        self.session = session
        let push = push
        sync = BackgroundSync(scheduler: SystemBackgroundScheduler()) {
            let result = await session.backgroundSync { bridge in
                await Self.refresh(surfaces, from: bridge)
            }
            // An expired run has already been answered; the upload waits for
            // the next run rather than outliving this one's budget.
            guard !_Concurrency.Task.isCancelled else { return result }
            await push.uploadIfNeeded(to: session.pushUploadTarget())
            return result
        }
    }

    /// File the push token, if there is one and the relay lacks it. The
    /// token's arrival calls this, and so does every sync start, which is
    /// what catches a relay device id minted by re-pairing.
    func uploadPushToken() async {
        await push.uploadIfNeeded(to: session?.pushUploadTarget())
    }

    /// Register the two task handlers. Must run before launch finishes —
    /// `BGTaskScheduler` refuses a registration after that.
    func registerTasks() {
        let scheduler = BGTaskScheduler.shared
        let (refresh, maintenance) = (BackgroundSync.refreshTaskID, BackgroundSync.maintenanceTaskID)
        _ = scheduler.register(forTaskWithIdentifier: refresh, using: .main) { task in
            MainActor.assumeIsolated { BackgroundHost.shared.perform(task) { await $0.refresh() } }
        }
        _ = scheduler.register(forTaskWithIdentifier: maintenance, using: .main) { task in
            MainActor.assumeIsolated { BackgroundHost.shared.perform(task) { await $0.maintain() } }
        }
    }

    /// Run one task to completion, or to the OS's expiry. Expiry answers the
    /// run `.failed` at once, completing the task unsuccessfully inside the
    /// OS's window, and cancels the run behind that answer; the next refresh
    /// is already scheduled by then.
    private func perform(
        _ task: BGTask,
        _ body: @escaping @MainActor (BackgroundSync) async -> BackgroundSyncResult
    ) {
        guard let sync else {
            task.setTaskCompleted(success: false)
            return
        }
        task.expirationHandler = {
            _Concurrency.Task { @MainActor in sync.expire() }
        }
        _Concurrency.Task { @MainActor in
            let result = await body(sync)
            task.setTaskCompleted(success: result != .failed)
        }
    }

    /// Bring what the OS shows from outside the app up to date with the vault
    /// a background run just synced. A cold background launch has no window,
    /// so nothing has bound the surfaces to this vault yet; binding them here
    /// is what the window would have done, and it rebinds on appearing.
    private static func refresh(_ surfaces: AppSurfaces, from bridge: CoreBridge) async {
        if surfaces.vault !== bridge { surfaces.attach(bridge: bridge) }
        if let reminders = surfaces.reminders {
            await reminders.refreshAuthorization()
            await reminders.reconcile()
        }
        surfaces.widgets?.refresh()
    }
}

/// The app delegate: the three OS callbacks a SwiftUI scene has no modifier
/// for — task registration at launch, the APNs token, and the silent push.
final class SunriseAppDelegate: NSObject, UIApplicationDelegate {
    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions _: [UIApplication.LaunchOptionsKey: Any]? = nil
    ) -> Bool {
        BackgroundHost.shared.registerTasks()
        // No permission prompt: a silent push shows nothing, and APNs hands
        // out the token without one.
        application.registerForRemoteNotifications()
        return true
    }

    func application(
        _: UIApplication,
        didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data
    ) {
        BackgroundHost.shared.push.receive(deviceToken: deviceToken)
        _Concurrency.Task { await BackgroundHost.shared.uploadPushToken() }
    }

    /// A simulator without APNs, or a build without the entitlement. The app
    /// syncs on its refresh schedule and in the foreground regardless.
    func application(
        _: UIApplication,
        didFailToRegisterForRemoteNotificationsWithError _: any Error
    ) {}

    /// A content-less `{"aps":{"content-available":1}}` wake
    /// (`docs/07-clients/mobile-ios.md` §Push handling): the same sync a
    /// refresh runs, joined if one is already running.
    func application(
        _: UIApplication,
        didReceiveRemoteNotification _: [AnyHashable: Any]
    ) async -> UIBackgroundFetchResult {
        guard let sync = BackgroundHost.shared.sync else { return .failed }
        return switch await sync.sync() {
        case .newData: .newData
        case .noData: .noData
        case .failed: .failed
        }
    }
}
