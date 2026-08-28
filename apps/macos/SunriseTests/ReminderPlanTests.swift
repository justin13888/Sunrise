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
}
