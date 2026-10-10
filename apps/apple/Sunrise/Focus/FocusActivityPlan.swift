import Foundation

/// The focus Live Activity a session should have.
struct FocusActivityTarget: Equatable, Sendable {
    let sessionID: EntityRef
    let state: FocusActivityState
}

/// One Live Activity on screen, as the plan sees it.
struct ShownFocusActivity: Equatable, Sendable {
    /// ActivityKit's id for the activity.
    let activityID: String
    let sessionID: EntityRef
    let state: FocusActivityState
}

/// Mapping the running focus session onto Live Activities, as pure decisions.
///
/// The iOS driver (`iOS/FocusLiveActivity.swift`) asks the vault for the
/// running session, asks ActivityKit what it is showing, and does what
/// ``steps(showing:want:)`` says. Everything that can be wrong — a stale
/// activity left from a session another device ended, two activities for one
/// session, a title edited mid-session — is decided here, where a test can
/// reach it without ActivityKit.
enum FocusActivityPlan {
    /// What a title-less session is called on the Lock Screen.
    static let untitled = "Focus session"

    /// The activity a session should have, or `nil` for none.
    ///
    /// Only a running session gets one. The start is the core's, so a session
    /// begun on another device shows the time it has really been running. A
    /// session sized by a planned length counts down to its end; one with no
    /// planned length — "until it is done" — counts up.
    static func target(for session: SessionRow?, title: String?) -> FocusActivityTarget? {
        guard let session, session.running, session.end == nil else { return nil }
        let started = Date(timeIntervalSince1970: Double(session.start.startedAt) / 1000)
        let ends = session.start.plannedMs
            .flatMap { $0 > 0 ? started.addingTimeInterval(Double($0) / 1000) : nil }
        let name = title?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return FocusActivityTarget(
            sessionID: session.start.id,
            state: FocusActivityState(
                title: name.isEmpty ? untitled : name,
                startedAt: started,
                endsAt: ends
            )
        )
    }

    /// One thing to do to ActivityKit.
    enum Step: Equatable, Sendable {
        case start(FocusActivityTarget)
        case update(activityID: String, FocusActivityState)
        case end(activityID: String)
    }

    /// What turns `showing` into exactly the one activity `want` describes.
    ///
    /// Every activity for another session is ended: that session finished, on
    /// this device or another, or belongs to a vault no longer open. Of the
    /// activities for the wanted session the first is kept — updated if what it
    /// draws has changed — and any others are ended, because two timers for one
    /// session is a Lock Screen that contradicts itself. Ends come first, so
    /// the system never holds a stale activity beside the new one.
    static func steps(showing: [ShownFocusActivity], want: FocusActivityTarget?) -> [Step] {
        var ends: [Step] = []
        var kept: ShownFocusActivity?
        for shown in showing {
            if shown.sessionID == want?.sessionID, kept == nil {
                kept = shown
            } else {
                ends.append(.end(activityID: shown.activityID))
            }
        }
        guard let want else { return ends }
        guard let kept else { return ends + [.start(want)] }
        guard kept.state != want.state else { return ends }
        return ends + [.update(activityID: kept.activityID, want.state)]
    }
}
