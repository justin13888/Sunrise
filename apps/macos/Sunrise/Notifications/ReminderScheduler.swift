import Foundation

/// Keeps the OS's schedule equal to what the core says it should be.
///
/// The reads are `Query::ReminderIntents`, and nothing here recomputes any
/// part of them. Lead times, the per-task → per-Stream → global hierarchy,
/// quiet hours, the four-hour queue cap and the primary-device rule are all
/// applied inside `sunrise_domain::notify` before a row reaches this file —
/// which is why a reminder fires at the same instant on this Mac as it would
/// on any other client. This object decides three things and no more: when to
/// ask, what identity a scheduled alert has, and which of them the OS is
/// already holding.
///
/// It follows the change feed the way ``MenuBarModel`` does, and for a sharper
/// reason: a sync burst is exactly when a naive implementation duplicates
/// every alert. See ``ReminderPlan/reconcile(desired:pending:)``.
@MainActor
@Observable
final class ReminderScheduler {
    /// What the system says, re-read on every reconcile.
    private(set) var authorization: NotificationAuthorization = .notDetermined

    /// What is currently handed to the OS, earliest first. Shown in Settings
    /// so "reminders are on" is a fact the user can check rather than a claim.
    private(set) var scheduled: [PlannedNotification] = []

    /// What the last reconcile did. Kept for the tests and for Settings.
    private(set) var lastPlan = ReminderReconciliation()

    private(set) var errorMessage: String?

    /// How often the *authorization status* is re-read.
    ///
    /// The only thing left on a timer, because it is the only thing the change
    /// feed cannot say: nothing tells an app that its notification permission
    /// was revoked in System Settings, so that one fact still has to be asked
    /// for. The schedule itself now follows changes — see ``follow(debounce:)``.
    static let authorizationRefreshInterval: Duration = .seconds(300)

    /// How long a change batch waits before it costs a reconcile.
    ///
    /// The feed is already coalesced on 50 ms; this adds a little on top
    /// because a reconcile is a vault query plus a round trip to
    /// `UNUserNotificationCenter`, and a user completing a run of tasks would
    /// otherwise buy one of each per 50 ms. Half a second of latency against a
    /// schedule the OS holds for four hours is not a trade worth thinking
    /// about twice.
    static let changeDebounce: Duration = .milliseconds(500)

    private let bridge: CoreBridge
    private let center: any NotificationCenterClient
    private let preferences: NotificationPreferences
    /// Where a tapped notification goes. Supplied rather than reached for: the
    /// scheduler has no view, and the surface that does own one is the app.
    private let route: @MainActor (DeepLink) -> Void
    private var responder: ReminderResponder?

    init(
        bridge: CoreBridge,
        preferences: NotificationPreferences,
        center: any NotificationCenterClient = SystemNotificationCenter(),
        route: @escaping @MainActor (DeepLink) -> Void
    ) {
        self.bridge = bridge
        self.preferences = preferences
        self.center = center
        self.route = route
    }

    /// Install the categories and the delegate, settle authorization, and
    /// schedule.
    ///
    /// The prompt happens here, once, on the first unlock — and only if the
    /// user has not already switched reminders off, because asking for a
    /// permission the app has been told not to use is how people learn to
    /// press Don't Allow without reading. A refusal is not an error path: the
    /// only consequence is that ``reconcile()`` schedules nothing.
    func start() async {
        await center.registerCategories()
        let responder = ReminderResponder { [weak self] response in
            await self?.handle(response)
        }
        // Held here, because the notification centre's `delegate` is weak.
        self.responder = responder
        await center.install(delegate: responder)

        authorization = await center.authorization()
        if authorization.canRequest, preferences.isEnabled {
            authorization = await center.requestAuthorization()
        }
        await reconcile()
    }

    /// Ask for permission, then schedule whatever was waiting on it.
    func requestAuthorization() async {
        authorization = await center.requestAuthorization()
        await reconcile()
    }

    /// Re-read the status without asking. The user can revoke in System
    /// Settings while the app is running, and Settings must not keep claiming
    /// reminders work after they have stopped.
    func refreshAuthorization() async {
        authorization = await center.authorization()
    }

    /// Make the OS's schedule match the core's answer.
    ///
    /// Safe to call as often as anything wants to. Three of the ways it ends
    /// with nothing scheduled are not failures and are not reported as such:
    /// permission was never granted, the user turned reminders off, or this is
    /// not the primary device — in which case the core hands back an empty
    /// list before it reads a row, and the reconcile below withdraws whatever
    /// this Mac was still holding. That last part is the point: a device
    /// demoted from primary must go quiet, not merely stop adding.
    func reconcile() async {
        let policy = preferences.policy
        let desired: [PlannedNotification]
        if authorization.allowsScheduling, policy.isEnabled {
            do {
                desired = try await intents(policy.settings)
                errorMessage = nil
            } catch {
                errorMessage = error.localizedDescription
                return
            }
        } else {
            desired = []
        }

        let plan = ReminderPlan.reconcile(
            desired: desired,
            pending: await center.pendingIdentifiers()
        )
        await center.cancel(identifiers: plan.cancel)
        for notification in plan.add {
            do {
                try await center.schedule(notification)
            } catch {
                errorMessage = error.localizedDescription
            }
        }
        scheduled = desired
        lastPlan = plan
    }

    /// Reconcile on every change batch, for as long as the app is open.
    ///
    /// This used to be a five-minute poll, and not by choice: `changes()` kept
    /// a single subscription and cancelled the previous one, so a scheduler
    /// that followed the feed for the life of the app would have permanently
    /// stolen it from whatever list the user was looking at. Now that the
    /// bridge fans out, following is simply correct — a reminder created on
    /// another device reaches this Mac's notification centre about half a
    /// second after the op lands, rather than up to five minutes later.
    ///
    /// **A lagged batch reconciles exactly like a complete one.** The schedule
    /// is derived from a full `Query::ReminderIntents` read either way, so
    /// there is nothing to patch — and the burst that causes a lag is a sync
    /// catch-up, which is precisely when the reminders are new.
    ///
    /// The authorization watchdog runs alongside rather than inside the loop:
    /// a permission revoked in System Settings produces no change event, and a
    /// vault that is quiet all afternoon would otherwise never notice.
    func follow(debounce: Duration = ReminderScheduler.changeDebounce) async {
        let watchdog = _Concurrency.Task { [weak self] in
            await self?.watchAuthorization()
        }
        defer { watchdog.cancel() }

        for await batch in await bridge.changes() {
            guard !batch.isClosed else { return }
            try? await _Concurrency.Task.sleep(for: debounce)
            await reconcile()
        }
    }

    /// The name ``RootView`` still calls. Prefer ``follow(debounce:)``.
    func poll() async { await follow() }

    /// Re-read the authorization status every
    /// ``authorizationRefreshInterval``, and reconcile on what it says.
    ///
    /// This is what makes ``reconcile()`` withdraw the schedule after the user
    /// revokes permission, rather than keep pushing at a service that has
    /// stopped listening.
    func watchAuthorization(
        every interval: Duration = ReminderScheduler.authorizationRefreshInterval
    ) async {
        while !_Concurrency.Task.isCancelled {
            try? await _Concurrency.Task.sleep(for: interval)
            guard !_Concurrency.Task.isCancelled else { return }
            await refreshAuthorization()
            await reconcile()
        }
    }

    /// Act on what the user did to a notification.
    func handle(_ response: ReminderResponse) async {
        switch response {
        case let .open(link):
            route(link)
        case let .act(entity, action):
            await perform(action, on: entity)
        case .dismissed:
            break
        }
    }

    /// Run one action button against the core.
    ///
    /// No window, no view: `docs/07-clients/interaction-patterns.md` says a
    /// deep link "translates to a CRDT op without opening UI when possible",
    /// and a button on a banner is exactly when it is possible.
    func perform(_ action: ReminderAction, on entity: EntityRef) async {
        do {
            switch action {
            case .complete:
                _ = try await bridge.submit(.completeTask(id: entity))
            case let .snooze(span):
                _ = try await bridge.submit(
                    .deferTask(id: entity, toMs: await snoozeTarget(span))
                )
            }
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
        await reconcile()
    }

    /// When a snooze lands.
    ///
    /// The seam's `snooze_target_ms`, never arithmetic here. "Tomorrow" is a
    /// **date**: adding 86,400,000 ms is an hour wrong twice a year, on
    /// exactly the reminders someone was relying on — which is why the domain
    /// exports a civil computation and this asks for it (commit `6be6044`).
    private func snoozeTarget(_ span: SnoozeSpan) async -> UInt64 {
        let now = await bridge.nowMs()
        let target = snoozeTargetMs(fromMs: now, span: span, tz: TimeZone.current.identifier)
        // The seam returns a signed instant because `jiff` does. A negative
        // one cannot happen for a forward span, and clamping is cheaper than
        // a trap if it ever did.
        return target <= 0 ? now : UInt64(target)
    }

    /// One read of `Query::ReminderIntents`, planned.
    private func intents(_ settings: NotificationSettings) async throws -> [PlannedNotification] {
        let now = await bridge.nowMs()
        let result = try await bridge.query(
            .reminderIntents(
                nowMs: now,
                horizonMs: now &+ ReminderPlan.horizonMs,
                settings: settings
            )
        )
        guard case let .reminders(rows) = result else { return [] }
        return ReminderPlan.plan(for: rows)
    }
}
