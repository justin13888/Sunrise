import Foundation
import Testing

@testable import Sunrise

private typealias Surface = SystemSurfaceFixture

/// The focus Live Activity's decisions: what a running session maps to, and
/// what has to happen to ActivityKit to show exactly that
/// (`mobile-ios.md` §Live Activities). ActivityKit itself cannot run in a unit
/// test; everything it is told to do is decided here first.
struct FocusActivityPlanTests {
    private let start = Date(timeIntervalSince1970: 1_760_000_000)

    private func state(_ title: String, minutes: Double? = 25) -> FocusActivityState {
        FocusActivityState(
            title: title,
            startedAt: start,
            endsAt: minutes.map { start.addingTimeInterval($0 * 60) }
        )
    }

    private func shown(
        _ activity: String,
        _ session: String,
        _ state: FocusActivityState
    ) -> ShownFocusActivity {
        ShownFocusActivity(activityID: activity, sessionID: session, state: state)
    }

    // MARK: - Mapping a session

    /// A real session, started through the core: the activity carries its
    /// start, its planned end and the task's title, and nothing else.
    @Test
    func aRunningSessionMapsToItsTitleAndTimer() async throws {
        let vault = try await TestVault()
        let id = try await Surface.task("Write the report", in: vault.bridge)
        _ = try await StartFocusIntent.start(on: id, length: .pomodoro, in: vault.bridge)
        let session = try #require(try await FocusSessions.running(in: vault.bridge))

        let target = try #require(FocusActivityPlan.target(for: session, title: "  Write the report "))
        #expect(target.sessionID == session.start.id)
        #expect(target.state.title == "Write the report")
        // Within a millisecond: the instants are carried as `Date`, a double.
        let startedMs = target.state.startedAt.timeIntervalSince1970 * 1000
        #expect(abs(startedMs - Double(session.start.startedAt)) < 1)
        if let planned = session.start.plannedMs {
            let length = try #require(target.state.endsAt).timeIntervalSince(target.state.startedAt)
            #expect(abs(length * 1000 - Double(planned)) < 1, "a pomodoro counts down to its planned end")
        } else {
            #expect(target.state.endsAt == nil)
        }

        _ = try await EndFocusIntent.end(completingTask: false, in: vault.bridge)
        let ended = try await FocusSessions.running(in: vault.bridge)
        #expect(FocusActivityPlan.target(for: ended, title: "Write the report") == nil)
        await vault.bridge.shutdown()
    }

    /// A session sized "until it is done" has no planned end, and counts up.
    @Test
    func anOpenEndedSessionCountsUp() async throws {
        let vault = try await TestVault()
        let id = try await Surface.task("Inbox zero", in: vault.bridge)
        _ = try await StartFocusIntent.start(on: id, length: .untilDone, in: vault.bridge)
        let session = try #require(try await FocusSessions.running(in: vault.bridge))
        let target = try #require(FocusActivityPlan.target(for: session, title: nil))
        #expect(target.state.endsAt == nil)
        #expect(target.state.title == FocusActivityPlan.untitled, "no title is never a blank banner")
        await vault.bridge.shutdown()
    }

    @Test
    func noSessionIsNoActivity() {
        #expect(FocusActivityPlan.target(for: nil, title: "Anything") == nil)
    }

    // MARK: - Reconciling with what is shown

    @Test
    func aNewSessionStartsAnActivity() {
        let want = FocusActivityTarget(sessionID: "fcs_1", state: state("Report"))
        #expect(FocusActivityPlan.steps(showing: [], want: want) == [.start(want)])
    }

    @Test
    func anUnchangedActivityIsLeftAlone() {
        let want = FocusActivityTarget(sessionID: "fcs_1", state: state("Report"))
        #expect(FocusActivityPlan.steps(showing: [shown("a", "fcs_1", state("Report"))], want: want).isEmpty)
    }

    /// A task renamed mid-session updates the activity rather than replacing it.
    @Test
    func aChangedTitleUpdatesInPlace() {
        let want = FocusActivityTarget(sessionID: "fcs_1", state: state("Final report"))
        #expect(
            FocusActivityPlan.steps(showing: [shown("a", "fcs_1", state("Report"))], want: want)
                == [.update(activityID: "a", state("Final report"))]
        )
    }

    /// A session ended — here, on another device — ends its activity.
    @Test
    func anEndedSessionEndsItsActivity() {
        #expect(FocusActivityPlan.steps(showing: [shown("a", "fcs_1", state("Report"))], want: nil)
            == [.end(activityID: "a")])
    }

    /// Another session's activity is ended before the new one starts, and a
    /// second activity for one session is ended rather than left to contradict
    /// the first.
    @Test
    func strayActivitiesAreEndedFirst() {
        let want = FocusActivityTarget(sessionID: "fcs_2", state: state("Email"))
        let showing = [
            shown("old", "fcs_1", state("Report")),
            shown("keep", "fcs_2", state("Email")),
            shown("twin", "fcs_2", state("Email"))
        ]
        #expect(
            FocusActivityPlan.steps(showing: showing, want: want)
                == [.end(activityID: "old"), .end(activityID: "twin")]
        )
        #expect(
            FocusActivityPlan.steps(showing: [shown("old", "fcs_1", state("Report"))], want: want)
                == [.end(activityID: "old"), .start(want)]
        )
    }
}
