import Foundation
import Testing
import UserNotifications

@testable import Sunrise

/// Turning `Query::ReminderIntents` into an OS schedule.
///
/// Every one of these runs without a notification service, which is the reason
/// the planning was split out of ``ReminderScheduler``: the question that
/// matters — does a sync burst duplicate every alert — is answerable with two
/// arrays and no permission prompt.
struct ReminderPlanTests {
    private let taskID = "tsk_01ARZ3NDEKTSV4RRFFQ69G5FAV"
    private let blockID = "blk_01ARZ3NDEKTSV4RRFFQ69G5FAV"

    private func reminder(
        _ id: String? = nil,
        kind: ReminderKind = .task,
        fireAt: Timestamp = 1_700_000_000_000,
        title: String = "Renew passport",
        deferredFrom: Timestamp? = nil
    ) -> Reminder {
        Reminder(
            entity: id ?? taskID,
            kind: kind,
            fireAt: fireAt,
            title: title,
            deferredFrom: deferredFrom
        )
    }

    /// The identity that makes rescheduling idempotent. Same intent, same
    /// string — computed twice, an hour apart, on either side of a sync.
    @Test
    func theSameIntentAlwaysGetsTheSameIdentifier() {
        #expect(
            ReminderPlan.identifier(for: reminder())
                == ReminderPlan.identifier(for: reminder())
        )
    }

    /// And the three things that must change it. A task that moved is a
    /// different alert; the old one has to be withdrawn rather than left
    /// pending beside the new one.
    @Test
    func entityKindAndFireTimeAllChangeTheIdentity() {
        let base = ReminderPlan.identifier(for: reminder())
        #expect(base != ReminderPlan.identifier(for: reminder(blockID)))
        #expect(base != ReminderPlan.identifier(for: reminder(kind: .block)))
        #expect(base != ReminderPlan.identifier(for: reminder(fireAt: 1_700_000_060_000)))
    }

    /// The failure this whole design exists to prevent: a change feed that
    /// fires forty times during a sync burst must not produce forty alerts.
    @Test
    func aSecondReconcileWithTheSameIntentsDoesNothing() {
        let desired = ReminderPlan.plan(for: [reminder(), reminder(blockID, kind: .block)])
        let first = ReminderPlan.reconcile(desired: desired, pending: [])
        #expect(first.add.count == 2)
        #expect(first.cancel.isEmpty)

        let second = ReminderPlan.reconcile(
            desired: desired,
            pending: first.add.map(\.identifier)
        )
        #expect(second.isEmpty, "an unchanged schedule is not rewritten")
        #expect(second.kept.count == 2)
    }

    /// A task moved to a new time: the old alert goes, the new one arrives.
    @Test
    func aReminderThatMovedIsWithdrawnAndReplaced() {
        let before = ReminderPlan.plan(for: [reminder()])
        let after = ReminderPlan.plan(for: [reminder(fireAt: 1_700_003_600_000)])
        let plan = ReminderPlan.reconcile(
            desired: after,
            pending: before.map(\.identifier)
        )
        #expect(plan.add.map(\.identifier) == after.map(\.identifier))
        #expect(plan.cancel == before.map(\.identifier))
    }

    /// Nothing wanted means everything of ours withdrawn — which is what a
    /// device demoted from primary, or a user turning reminders off, has to do.
    @Test
    func anEmptyScheduleCancelsWhatWasPending() {
        let pending = ReminderPlan.plan(for: [reminder()]).map(\.identifier)
        let plan = ReminderPlan.reconcile(desired: [], pending: pending)
        #expect(plan.cancel == pending)
        #expect(plan.add.isEmpty)
    }

    /// Reconciliation cancels; without a namespace it would cancel other
    /// people's work.
    @Test
    func nothingOutsideTheAppsOwnNamespaceIsTouched() {
        let plan = ReminderPlan.reconcile(
            desired: [],
            pending: ["com.apple.something", "sunrise.other.thing"]
        )
        #expect(plan.cancel.isEmpty)
    }

    /// The system holds 64 scheduled local notifications and silently discards
    /// the rest, so the cap is applied here — where the *earliest* survive —
    /// rather than by the OS, which would drop an arbitrary set.
    @Test
    func theEarliestRemindersAreTheOnesThatSurviveTheCap() {
        let many = (0..<80).map { index in
            reminder(fireAt: 1_700_000_000_000 + Timestamp(index) * 60_000)
        }
        let planned = ReminderPlan.plan(for: many.shuffled())
        #expect(planned.count == ReminderPlan.maxScheduled)
        #expect(planned.first?.fireAtMs == 1_700_000_000_000)
        #expect(planned.map(\.fireAtMs) == planned.map(\.fireAtMs).sorted())
    }

    /// A Block has no `CompleteTask` and no `DeferTask` behind it, so it gets
    /// a category with no buttons that would have to fail.
    @Test
    func aBlockCarriesNoActionsAndATaskCarriesThree() {
        #expect(ReminderCategory.block.snoozeSpans.isEmpty)
        #expect(!ReminderCategory.block.offersCompletion)
        #expect(ReminderCategory.task.offersCompletion)
        #expect(ReminderCategory.task.snoozeSpans == [.oneHour, .tomorrow])
    }

    /// A routine occurrence is a Task by the time it is due, so it gets the
    /// task buttons — but not the task identity, or a routine and a task on
    /// the same minute would collide.
    @Test
    func aRoutineIsScheduledAsATaskWithItsOwnIdentity() {
        let routine = ReminderPlan.notification(for: reminder(kind: .routine))
        #expect(routine.category == .task)
        #expect(routine.identifier != ReminderPlan.identifier(for: reminder()))
    }

    /// The visible half of `quiet_hours.policy = "queue"`. Without it a 07:00
    /// alert for an 06:30 task reads as a bug rather than as the setting the
    /// user asked for.
    @Test
    func aQuietHoursDeferralIsExplainedOnTheNotification() {
        let queued = ReminderPlan.notification(
            for: reminder(deferredFrom: 1_699_980_000_000)
        )
        #expect(queued.body.contains(ReminderPlan.quietHoursNote))
        #expect(ReminderPlan.notification(for: reminder()).body.isEmpty)
    }

    /// The body tap. A task opens itself; a block opens the grid it is on.
    @Test
    func tappingTheBodyOpensWhatTheReminderIsAbout() {
        #expect(ReminderPlan.notification(for: reminder()).link == .task(taskID, .open))
        #expect(
            ReminderPlan.notification(for: reminder(blockID, kind: .block)).link
                == .entity(blockID)
        )
    }
}

/// The rest of the catalog: the kinds the core plans beside the reminders,
/// and how this client hands them to the OS
/// (`docs/08-features/notifications.md` §Catalog).
struct CatalogPlanTests {
    private let taskID = "tsk_01ARZ3NDEKTSV4RRFFQ69G5FAV"

    private func item(
        _ kind: NotificationKind,
        link: String = "sunrise://morning",
        entity: EntityRef? = nil,
        fireAtMs: Int64 = 1_700_000_000_000
    ) -> CatalogNotificationItem {
        CatalogNotificationItem(
            identifier: "sunrise.notify.\(NotificationCatalog.tag(of: kind))",
            revision: "\(fireAtMs).abc",
            kind: kind,
            title: "Good morning",
            body: "Plan your day",
            entity: entity,
            subject: nil,
            count: nil,
            link: link,
            actions: NotificationCatalog.info(kind)?.actions ?? [],
            level: NotificationCatalog.info(kind)?.level ?? .active,
            fireAtMs: fireAtMs,
            deferredFromMs: nil
        )
    }

    private func policy(off: [NotificationKind]) -> CatalogPolicyItem {
        CatalogPolicyItem(
            toggles: NotificationCatalog.kinds.map {
                CatalogToggle(kind: $0.kind, enabled: !off.contains($0.kind))
            },
            deadlineLeadS: 3600,
            morningOffsetS: 900,
            eveningOffsetS: 7200
        )
    }

    /// One category per kind, with the catalog's buttons in the catalog's
    /// order — the done-list item "`ReminderCategory` covers each one, with
    /// its actions".
    @Test
    func everyKindOfTheCatalogHasACategoryWithItsButtons() {
        let categories = ReminderCategory.all
        #expect(categories.count == 2 + NotificationCatalog.kinds.count)
        #expect(Set(categories.map(\.identifier)).count == categories.count, "no two share an identifier")
        for info in NotificationCatalog.kinds {
            let category = ReminderCategory.catalog(info.kind)
            #expect(category.catalogActions == info.actions, "\(info.tag)")
            let registered = SystemNotificationCenter.category(category)
            #expect(registered.actions.map(\.identifier) == info.actions.map(\.identifier), "\(info.tag)")
        }
        #expect(ReminderCategory.task.catalogActions.isEmpty, "a reminder's buttons are its own")
    }

    /// A button pressed comes back as the same action, and the ones that open
    /// a screen bring the app forward.
    @Test
    func everyCatalogButtonRoundTripsThroughItsIdentifier() {
        for action in NotificationAction.known {
            #expect(NotificationAction(identifier: action.identifier) == action)
            #expect(!action.buttonTitle.isEmpty)
        }
        #expect(Set(NotificationAction.known.map(\.tag)).count == NotificationAction.known.count)
        #expect(NotificationAction(identifier: "sunrise.catalog.nonsense") == nil)
        #expect(NotificationAction(identifier: ReminderAction.complete.rawValue) == nil)
        #expect(NotificationAction.openTriage.opensApp)
        #expect(!NotificationAction.markDone.opensApp)
        #expect(!NotificationAction.snoozeFifteenMinutes.opensApp)
    }

    /// The core's words, instant, revision, level and link reach the request
    /// unchanged.
    @Test
    func aCatalogItemBecomesANotificationAsTheCorePlannedIt() {
        let deadline = item(.deadline, link: "sunrise://task/\(taskID)", entity: taskID)
        let planned = ReminderPlan.notification(for: deadline)
        #expect(planned.identifier == "sunrise.notify.deadline")
        #expect(planned.category == .catalog(.deadline))
        #expect(planned.link == .task(taskID, .open))
        #expect(planned.entity == taskID)
        #expect(planned.revision == "1700000000000.abc")
        #expect(!planned.postsWhenDue)

        let brief = ReminderPlan.notification(for: item(.morningBrief))
        #expect(brief.entity.isEmpty)
        #expect(brief.link == .morningSummary)
        let paired = item(.devicePaired, link: "sunrise://devices")
        #expect(ReminderPlan.notification(for: paired, alert: true).postsWhenDue)
    }

    /// A reminder kind switched off is left out; the rest stay.
    @Test
    func aReminderWhoseKindIsOffIsLeftOut() {
        let block = "blk_01ARZ3NDEKTSV4RRFFQ69G5FAV"
        let rows = [
            Reminder(entity: taskID, kind: .task, fireAt: 1, title: "a", deferredFrom: nil),
            Reminder(entity: taskID, kind: .routine, fireAt: 2, title: "b", deferredFrom: nil),
            Reminder(entity: block, kind: .block, fireAt: 3, title: "c", deferredFrom: nil)
        ]
        let kept = ReminderPlan.enabled(rows, under: policy(off: [.taskReminder, .blockStart]))
        #expect(kept.map(\.kind) == [.routine])
        #expect(ReminderPlan.enabled(rows, under: nil).count == 3, "no policy read yet is no filter")
    }

    /// A device that is not the primary one shows only what every device
    /// shows: the security alerts, and its own sync notice.
    @Test
    func aDeviceThatIsNotPrimaryKeepsOnlyTheKindsEveryDeviceShows() {
        let items = [
            item(.morningBrief), item(.devicePaired, link: "sunrise://devices"),
            item(.timezoneChanged, link: "sunrise://upcoming"), item(.syncStalled, link: "sunrise://sync")
        ]
        #expect(ReminderPlan.audible(items, isPrimary: true).count == 4)
        #expect(ReminderPlan.audible(items, isPrimary: false).map(\.kind) == [.devicePaired, .syncStalled])
    }

    /// The request the OS gets: the revision is stored for the next re-plan
    /// to read back, the level is the catalog's, and an alert already due
    /// fires now instead of at an instant that has passed.
    @Test
    func theRequestCarriesTheRevisionTheLevelAndAnAlertsImmediacy() throws {
        let now = Date(timeIntervalSince1970: 1_700_000_100)
        let wind = SystemNotificationCenter.request(
            for: ReminderPlan.notification(for: item(.windDown)),
            now: now
        )
        #expect(wind.content.userInfo[ReminderPayload.revision] as? String == "1700000000000.abc")
        #expect(wind.content.categoryIdentifier == ReminderCategory.catalog(.windDown).identifier)

        let alert = SystemNotificationCenter.request(
            for: ReminderPlan.notification(for: item(.devicePaired, link: "sunrise://devices"), alert: true),
            now: now
        )
        #expect(alert.trigger == nil, "due already: posted now")
        #expect(alert.content.interruptionLevel == .timeSensitive)

        let later = item(.devicePaired, fireAtMs: 1_700_000_200_000)
        let held = SystemNotificationCenter.request(
            for: ReminderPlan.notification(for: later, alert: true),
            now: now
        )
        #expect(held.trigger is UNCalendarNotificationTrigger, "held by quiet hours: fires when they end")
    }

    /// An armed trigger keeps its instant across a zone change: the
    /// components carry their zone, so they do not drift to the same wall
    /// clock in the new one (#520).
    @Test
    func aTriggerKeepsItsInstantInAnotherZone() throws {
        let fire = Date(timeIntervalSince1970: 1_700_000_000)
        let toronto = try #require(TimeZone(identifier: "America/Toronto"))
        let components = SystemNotificationCenter.components(of: fire, in: toronto)
        #expect(components.timeZone == toronto)
        var london = Calendar(identifier: .gregorian)
        london.timeZone = try #require(TimeZone(identifier: "Europe/London"))
        #expect(london.date(from: components) == fire)
    }
}

/// What the user did to a notification, decoded.
///
/// Split from `UserNotifications` because a `UNNotificationResponse` cannot be
/// constructed by an app at all — so without this split the mapping from
/// button to command would be the one part of the feature no test could reach.
struct ReminderResponseTests {
    private let taskID = "tsk_01ARZ3NDEKTSV4RRFFQ69G5FAV"

    @Test
    func eachButtonDecodesToTheCommandItRuns() {
        let cases: [(ReminderAction, String)] = [
            (.complete, "complete"),
            (.snooze(.oneHour), "snooze_1h"),
            (.snooze(.tomorrow), "snooze_tomorrow")
        ]
        for (action, name) in cases {
            #expect(
                ReminderResponse.decode(
                    actionIdentifier: "sunrise.action.\(name)",
                    entity: taskID,
                    link: nil
                ) == .act(taskID, action)
            )
        }
    }

    /// The identifiers are built from the deep-link action names, so the
    /// button and the equivalent `sunrise://` URL cannot drift apart.
    @Test
    func aButtonIdentifierIsItsLinkAction() {
        #expect(ReminderAction.complete.rawValue == "sunrise.action.complete")
        #expect(ReminderAction.snooze(.tomorrow).rawValue == "sunrise.action.snooze_tomorrow")
        #expect(ReminderAction(rawValue: "sunrise.action.snooze_1h") == .snooze(.oneHour))
    }

    @Test
    func tappingTheBodyOpensTheLinkItCarries() {
        #expect(
            ReminderResponse.decode(
                actionIdentifier: UNNotificationDefaultActionIdentifier,
                entity: taskID,
                link: "sunrise://morning"
            ) == .open(.morningSummary)
        )
    }

    /// Everything unrecognised is dropped rather than guessed at: a swipe, a
    /// button from a newer build, and a payload that lost its entity.
    @Test
    func anythingUnrecognisedIsDismissed() {
        #expect(
            ReminderResponse.decode(
                actionIdentifier: UNNotificationDismissActionIdentifier,
                entity: taskID,
                link: nil
            ) == .dismissed
        )
        #expect(
            ReminderResponse.decode(
                actionIdentifier: "sunrise.action.detonate",
                entity: taskID,
                link: nil
            ) == .dismissed
        )
        #expect(
            ReminderResponse.decode(
                actionIdentifier: "sunrise.action.complete",
                entity: nil,
                link: nil
            ) == .dismissed
        )
    }

    /// A catalog button comes back with the notification it was pressed on,
    /// which a snooze posts again; without that notification it is nothing.
    @Test
    func aCatalogButtonCarriesTheNotificationItWasPressedOn() {
        let delivered = PlannedNotification(
            identifier: "sunrise.notify.morning_brief",
            title: "Good morning",
            body: "Plan your day",
            category: .catalog(.morningBrief),
            fireAtMs: 0,
            entity: "",
            link: .morningSummary
        )
        #expect(
            ReminderResponse.decode(
                actionIdentifier: NotificationAction.planMyDay.identifier,
                entity: "",
                link: "sunrise://morning",
                delivered: delivered
            ) == .catalog(.planMyDay, delivered)
        )
        #expect(
            ReminderResponse.decode(
                actionIdentifier: NotificationAction.planMyDay.identifier,
                entity: "",
                link: "sunrise://morning"
            ) == .dismissed
        )
    }
}
