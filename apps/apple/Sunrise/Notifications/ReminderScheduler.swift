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

    /// The rest of the catalog handed to the OS — the deadline, the briefs,
    /// the triage nudge and wind-down — earliest first.
    private(set) var catalogScheduled: [PlannedNotification] = []

    /// How many requests the OS is holding for this app, for Settings.
    var heldCount: Int { scheduled.count + catalogScheduled.count }

    /// What the last reconcile did. Kept for the tests and for Settings.
    private(set) var lastPlan = ReminderReconciliation()

    /// Which kinds of the catalog are on, as the last reconcile read them.
    /// Settings → Notifications lists one switch per kind from it.
    private(set) var catalogPolicy: CatalogPolicyItem?

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
    /// The Focus filter in force. Reminders from streams outside its scope
    /// are withdrawn while it is on; see ``FocusFilter``.
    private let focusFilter: FocusFilterStore
    /// What ``observeTimeChanges()`` installed.
    private let clockChanges = ClockChangeObserver()

    init(
        bridge: CoreBridge,
        preferences: NotificationPreferences,
        center: any NotificationCenterClient = SystemNotificationCenter(),
        focusFilter: FocusFilterStore = .shared,
        route: @escaping @MainActor (DeepLink) -> Void
    ) {
        self.bridge = bridge
        self.preferences = preferences
        self.center = center
        self.focusFilter = focusFilter
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
        observeTimeChanges()
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
    ///
    /// Three passes over one read of the pending list: the rest of the catalog
    /// (the deadline, the briefs, the triage nudge and the alerts) and
    /// wind-down, both planned and reconciled by the core, then the reminders,
    /// planned here from `Query::ReminderIntents` under this device's
    /// settings, in whatever room the first two leave under the cap.
    ///
    /// One pass at a time: a zone change arrives twice (the clock observer
    /// and the change feed), and two passes that both read the ledger before
    /// either saves it would both post the same alert. A failed catalog plan
    /// still plans the reminders, and leaves the catalog as the OS holds it.
    func reconcile() async {
        let previous = reconciling
        let pass = _Concurrency.Task { @MainActor [weak self] in
            await previous?.value
            await self?.reconcileOnce()
        }
        reconciling = pass
        await pass.value
    }

    /// The pass in flight, if any; the next ``reconcile()`` waits for it.
    private var reconciling: _Concurrency.Task<Void, Never>?

    private func reconcileOnce() async {
        let policy = preferences.policy
        let delivering = authorization.allowsScheduling && policy.isEnabled
        let pending = await center.pendingRequests()
        var failure: String?
        var others: CatalogPass?
        do {
            others = try await planOthers(delivering: delivering, policy: policy, pending: pending)
            catalogPolicy = others?.policy
        } catch { failure = error.localizedDescription }

        let desired: [PlannedNotification]
        if delivering {
            // Without a catalog plan, the rest still takes what the OS holds.
            let held = others?.held.count
                ?? pending.count { !$0.identifier.hasPrefix(ReminderPlan.identifierPrefix) }
            do {
                desired = try await intents(
                    policy.settings,
                    kinds: catalogPolicy,
                    limit: max(0, ReminderPlan.maxScheduled - held)
                )
            } catch {
                errorMessage = failure ?? error.localizedDescription
                return
            }
        } else {
            desired = []
        }

        let plan = ReminderPlan.reconcile(desired: desired, pending: pending.map(\.identifier))
        await center.cancel(identifiers: plan.cancel + (others?.cancel ?? []))
        var alertsPosted = true
        for notification in plan.add + (others?.schedule ?? []) {
            do {
                try await center.schedule(notification)
            } catch {
                if notification.identifier.hasPrefix(NotificationCatalog.prefixes.alert) {
                    alertsPosted = false
                }
                failure = failure ?? error.localizedDescription
            }
        }
        // Kept only once every alert is handed over, or a failed or crashed
        // post would be remembered as shown and never raised again. An alert
        // posted beside a failed one is posted again, replacing itself.
        if alertsPosted, let others, let ledger = others.ledger {
            preferences.setLedger(ledger, for: others.device)
        }
        errorMessage = failure
        scheduled = desired
        if let others { catalogScheduled = others.held.sorted { $0.fireAtMs < $1.fireAtMs } }
        lastPlan = plan
    }

    /// The deadline, the briefs, the triage nudge, wind-down and the alerts.
    ///
    /// Every decision is the core's: what fires, when, with which words and
    /// buttons, and whether what the OS holds is still right. Two of this
    /// device's own settings are applied on top, because until #489 moves them
    /// into the core's preferences they live here: the master switch (off
    /// hands the core `authorized: false`, which plans nothing), and whether
    /// this is the primary device (a device that is not keeps only the kinds
    /// every device shows, which are alerts).
    private func planOthers(
        delivering: Bool,
        policy: ReminderPolicy,
        pending: [PendingReminder]
    ) async throws -> CatalogPass {
        let now = await bridge.nowMs()
        let device = await bridge.deviceId()
        let isPrimary = policy.settings.isPrimaryDevice
        let plan = try await bridge.scheduleNotifications(
            nowMs: now,
            authorized: delivering,
            focusScope: Array(focusFilter.streams ?? []),
            pending: pending,
            clock: ReminderPlan.clockStyle(),
            ledgerJson: preferences.ledger(for: device),
            networkAvailable: NetworkClassMonitor.shared.isOnline
        )
        var out = CatalogPass(device: device, ledger: plan.ledgerJson, policy: plan.catalogPolicy)
        if isPrimary {
            out.held = plan.catalog.map { ReminderPlan.notification(for: $0) }
            let changes = plan.catalogChanges
            out.schedule = (changes.add + changes.replace).map { ReminderPlan.notification(for: $0) }
            out.cancel = changes.cancel
        } else {
            let prefix = NotificationCatalog.prefixes.catalog
            out.cancel = pending.map(\.identifier).filter { $0.hasPrefix(prefix) }
        }
        out.schedule += ReminderPlan.audible(plan.alerts, isPrimary: isPrimary)
            .map { ReminderPlan.notification(for: $0, alert: true) }

        let windDown = try await bridge.windDownPlan(
            nowMs: now,
            authorized: delivering && isPrimary,
            pending: pending
        )
        if let desired = windDown.desired {
            out.held.append(ReminderPlan.notification(for: desired))
        }
        switch windDown.change {
        case let .add(notification), let .replace(notification):
            out.schedule.append(ReminderPlan.notification(for: notification))
        case .cancel:
            out.cancel.append(NotificationCatalog.windDown)
        case .keep:
            break
        }
        return out
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
        case let .catalog(action, delivered):
            await perform(action, on: delivered)
        case .dismissed:
            break
        }
    }

    /// Run one of a catalog kind's buttons.
    ///
    /// Three shapes. Mark Done is the reminder's own command. A snooze posts
    /// the same notification again later, under an identifier no re-plan
    /// cancels — the deadline and the briefs fire at instants the core fixes,
    /// so moving the task would not bring them back. Keep is the answer "leave
    /// it", and needs nothing. Every other button opens a screen: the one its
    /// link names, or for Open Triage the triage section.
    func perform(_ action: NotificationAction, on delivered: PlannedNotification) async {
        switch action {
        case .markDone:
            guard !delivered.entity.isEmpty else { return }
            await perform(.complete, on: delivered.entity)
        case .snoozeOneHour, .snoozeFiveMinutes, .snoozeFifteenMinutes, .snoozeTomorrow:
            await snooze(delivered, by: action)
        case .keep:
            break
        case .openTriage:
            route(.triage)
        default:
            if let link = delivered.link { route(link) }
        }
    }

    /// Post `delivered` again when `action` says.
    private func snooze(_ delivered: PlannedNotification, by action: NotificationAction) async {
        let at: UInt64
        if let minutes = action.snoozeMinutes {
            at = await bridge.nowMs() &+ UInt64(minutes) * 60_000
        } else {
            at = await snoozeTarget(.tomorrow)
        }
        do {
            try await center.schedule(ReminderPlan.snoozed(delivered, until: Int64(clamping: at)))
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// Turn one kind of the catalog on or off, then re-plan under it.
    ///
    /// Written to the vault, so the choice reaches every device, except where
    /// the key is this device's alone (sync stalled, place arrival): the
    /// key's own scope decides, read from the core's preferences.
    func setKind(_ kind: NotificationKind, enabled: Bool) async {
        guard let key = NotificationCatalog.info(kind)?.enabledKey else { return }
        do {
            var target = PreferenceTarget.vault
            if case let .preferences(items) = try await bridge.query(.preferences),
               items.first(where: { $0.key == key })?.scope == .device {
                target = .device
            }
            let command = CoreCommand.setPreference(key: key, value: .bool(value: enabled), target: target)
            _ = try await bridge.submit(command)
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
        await reconcile()
    }

    /// Re-plan when the clock or the calendar jumps; see ``ClockChangeObserver``.
    func observeTimeChanges() {
        clockChanges.start { [weak self] in await self?.reconcile() }
    }

    /// Stop re-planning on clock changes: the vault this plans for is closing.
    func stopObservingTimeChanges() { clockChanges.stop() }

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

    /// One read of `Query::ReminderIntents`, muted by the Focus filter, planned.
    ///
    /// Muted *before* planning, so the cap in ``ReminderPlan/plan(for:timeZone:limit:)``
    /// is spent on reminders that can fire: a Work Focus must not let a
    /// morning of muted Personal reminders crowd the Work ones out of the
    /// OS's sixty-four slots. Withdrawn rather than delivered silently, and
    /// re-added by the next reconcile once the Focus ends — which is the
    /// reconcile the scope changing triggers.
    ///
    /// A reminder whose kind is switched off in Settings is left out before
    /// the cap too, and `limit` is the room the rest of the catalog left.
    private func intents(
        _ settings: NotificationSettings,
        kinds: CatalogPolicyItem?,
        limit: Int
    ) async throws -> [PlannedNotification] {
        let now = await bridge.nowMs()
        let result = try await bridge.query(
            .reminderIntents(
                nowMs: now,
                horizonMs: now &+ ReminderPlan.horizonMs,
                settings: settings
            )
        )
        guard case let .reminders(rows) = result else { return [] }
        let wanted = ReminderPlan.enabled(rows, under: kinds)
        let scope = focusFilter.streams
        let streams = scope == nil ? [:] : await streams(of: wanted)
        return ReminderPlan.plan(
            for: FocusFilter.muting(wanted, to: scope, streams: streams),
            limit: limit
        )
    }

    /// The stream each reminder's entity is filed in, where it has one.
    ///
    /// A reminder carries its entity and not its stream, so a filtered
    /// reconcile reads each one — at most a day's reminders, and only while a
    /// Focus filter is on. A read that fails leaves its entity out, which
    /// ``FocusFilter/allows(stream:in:)`` lets through.
    private func streams(of reminders: [Reminder]) async -> [EntityRef: EntityRef] {
        var streams: [EntityRef: EntityRef] = [:]
        for entity in Set(reminders.map(\.entity)) {
            if let row = try? await bridge.query(.entityById(id: entity)) {
                streams[entity] = Self.stream(of: row)
            }
        }
        return streams
    }

    /// The stream a reminder's entity is filed in: a task's own, a routine's
    /// template's, and none for anything else (a time block belongs to no
    /// stream, and the Focus filter lets it through).
    ///
    /// The core emits no routine reminder yet (`ReminderKind::Routine` is
    /// constructed nowhere but its own tests), so the routine case is ready for
    /// when it does, rather than the Focus filter letting every routine through.
    nonisolated static func stream(of row: CoreQueryResult) -> EntityRef? {
        switch row {
        case let .task(item): item.streamId
        case let .routine(item): item.template.streamId
        default: nil
        }
    }
}
