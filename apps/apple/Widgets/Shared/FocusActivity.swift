#if os(iOS)
import ActivityKit
#endif
import Foundation

/// What the focus Live Activity draws: the task's title and the timer.
///
/// `docs/07-clients/mobile-ios.md` §Live Activities. The content state is these
/// three fields and nothing else, and that is a bound on what leaves the vault:
/// a Live Activity is drawn on the Lock Screen by another process, so every
/// field here is plaintext outside SQLCipher for as long as the session runs —
/// the same trade a reminder's title and the widget snapshot make.
///
/// Compiled into both apps and both widget extensions, so — like
/// ``WidgetSnapshot`` — it imports system frameworks and nothing else. The
/// timer is carried as instants rather than as a running count: the system
/// draws `Text(timerInterval:)` itself, so the activity keeps time with the app
/// suspended and is never updated once a second.
struct FocusActivityState: Codable, Hashable, Sendable {
    let title: String
    /// When the session started, by the core's record of it.
    let startedAt: Date
    /// When a sized session runs out, or `nil` for one that runs until the task
    /// is done, which counts up instead.
    let endsAt: Date?
}

#if os(iOS)
/// The Live Activity's static half: which session it is about.
///
/// The session id is an attribute rather than content, because it never
/// changes while the activity lives and it is how the app finds the activity
/// that belongs to a session — and ends the ones that do not.
struct FocusActivityAttributes: ActivityAttributes {
    typealias ContentState = FocusActivityState

    /// The core's `EntityRef` for the session.
    let sessionID: String
}
#endif
