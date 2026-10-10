import ActivityKit
import AppIntents
import Foundation

/// Keeps the focus Live Activity equal to the vault's running session.
///
/// `docs/07-clients/mobile-ios.md` §Live Activities: a session puts its timer
/// and its task's title in the Dynamic Island and on the Lock Screen, and
/// ending it takes them away. The decisions are ``FocusActivityPlan``'s; this
/// reads the two sides it compares and carries the steps out.
///
/// **Driven by the vault, not by the buttons.** Three things start or end a
/// session — the Focus screen, a `sunrise://focus` link, and the two focus
/// intents — and a sync can deliver one ended on another device. Hooking each
/// of them would be four places to forget; reconciling against
/// `Query::RunningFocusSessions` on every change batch is one, and it is the
/// only one that sees the sync.
///
/// Not main-actor isolated: `Activity` is not `Sendable`, and its `update` and
/// `end` are nonisolated, so an activity read on the main actor could not be
/// handed to them. Nothing here touches UI state.
enum FocusLiveActivity {
    /// Make what ActivityKit shows match the running session in `bridge`.
    ///
    /// A failed read changes nothing: an activity ended because the query
    /// failed would be a timer vanishing from a session that is still running.
    static func reconcile(in bridge: CoreBridge) async {
        let running: SessionRow?
        do {
            running = try await FocusSessions.running(in: bridge)
        } catch {
            return
        }
        var title: String?
        if let running {
            title = await FocusSessions.title(of: running.start.taskId, in: bridge)
        }
        let want = ActivityAuthorizationInfo().areActivitiesEnabled
            ? FocusActivityPlan.target(for: running, title: title)
            : nil
        await apply(FocusActivityPlan.steps(showing: shown(), want: want))
    }

    /// Reconcile now, then on every change batch until the vault closes.
    static func follow(_ bridge: CoreBridge) async {
        await reconcile(in: bridge)
        for await batch in await bridge.changes() {
            guard !batch.isClosed else { return }
            await reconcile(in: bridge)
        }
    }

    /// End every focus activity: the vault they were read from has closed.
    ///
    /// The widget snapshot's rule (`mobile-ios.md` §The snapshot is the whole
    /// contract) applied to the Lock Screen's other plaintext: a lock, a
    /// sign-out or a vault switch takes the title off it, and the next vault
    /// to open puts back whatever its own running session calls for.
    static func endAll() async {
        await apply(FocusActivityPlan.steps(showing: shown(), want: nil))
    }

    private static func shown() -> [ShownFocusActivity] {
        Activity<FocusActivityAttributes>.activities
            .filter { $0.activityState == .active || $0.activityState == .stale }
            .map {
                ShownFocusActivity(
                    activityID: $0.id,
                    sessionID: $0.attributes.sessionID,
                    state: $0.content.state
                )
            }
    }

    private static func apply(_ steps: [FocusActivityPlan.Step]) async {
        let activities = Activity<FocusActivityAttributes>.activities
        for step in steps {
            switch step {
            case let .start(target):
                // Refused when the user has turned Live Activities off for
                // Sunrise, or the system's budget is spent. The session runs
                // either way; the Focus screen still shows it.
                _ = try? Activity.request(
                    attributes: FocusActivityAttributes(sessionID: target.sessionID),
                    content: ActivityContent(state: target.state, staleDate: nil),
                    pushType: nil
                )
            case let .update(activityID, state):
                guard let activity = activities.first(where: { $0.id == activityID }) else { continue }
                await activity.update(ActivityContent(state: state, staleDate: nil))
            case let .end(activityID):
                guard let activity = activities.first(where: { $0.id == activityID }) else { continue }
                await activity.end(nil, dismissalPolicy: .immediate)
            }
        }
    }
}

// The two focus intents may run with Sunrise in the background — a Shortcut, a
// spoken phrase — and only an intent that declares itself a Live Activity
// intent may start an activity from there. Without this a session started from
// Siri would have no timer on the Lock Screen until the app was next opened.
extension StartFocusIntent: LiveActivityIntent {}
extension EndFocusIntent: LiveActivityIntent {}
