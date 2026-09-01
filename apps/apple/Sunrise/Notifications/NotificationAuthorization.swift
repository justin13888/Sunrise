import UserNotifications

/// Whether this app may post notifications, and what to say when it may not.
///
/// Five cases rather than a `Bool`, for the same reason ``HotkeyStatus`` has
/// four: each one needs a different sentence and a different offer. "Not asked
/// yet" is a button; "denied" is a trip to System Settings; "unavailable" is
/// neither, and telling someone to open System Settings when the real problem
/// is that the notification service refused the request sends them somewhere
/// that cannot help.
///
/// **None of them stops the app.** Reminders are one surface of a local-first
/// vault, not the product: a user who says no here keeps every list, the
/// calendar, focus, review and both of the daily views the reminders would
/// have opened. That is the whole design constraint, and
/// ``allowsScheduling`` is the single place it is enforced.
enum NotificationAuthorization: Equatable, Sendable {
    /// Never asked. The only state in which asking is appropriate.
    case notDetermined
    /// The user said no, or turned it off later in System Settings.
    case denied
    /// Full permission.
    case authorized
    /// Quiet delivery, granted without a prompt.
    case provisional
    /// The request itself failed, with what the system said.
    case unavailable(String)

    /// Whether reminders can be handed to the OS at all.
    ///
    /// Provisional counts: a quietly-delivered reminder is still a reminder,
    /// and refusing to schedule under it would turn Apple's own
    /// try-before-you-allow path into a broken feature.
    var allowsScheduling: Bool {
        switch self {
        case .authorized, .provisional: true
        case .notDetermined, .denied, .unavailable: false
        }
    }

    /// Whether the Settings row draws a tick or a warning triangle.
    var isActive: Bool { allowsScheduling }

    /// The two-word answer, for a `LabeledContent` value.
    var summary: String {
        switch self {
        case .notDetermined: "Not asked"
        case .denied: "Not allowed"
        case .authorized: "Allowed"
        case .provisional: "Quiet delivery"
        case .unavailable: "Unavailable"
        }
    }

    /// The sentence under it. Every one of them says the app still works.
    var explanation: String {
        switch self {
        case .notDetermined:
            "Sunrise has not asked yet. Reminders stay off until you allow them; "
                + "everything else works either way."
        case .denied:
            "Notifications are turned off for Sunrise in System Settings. "
                + "Nothing else is affected — the morning and evening views are still in the sidebar."
        case .authorized:
            "Reminders for scheduled tasks and time blocks are delivered by this Mac."
        case .provisional:
            "Reminders arrive quietly, in Notification Centre only, until you allow them properly."
        case let .unavailable(reason):
            "The system refused the request (\(reason)). "
                + "Reminders are off; the rest of Sunrise is unaffected."
        }
    }

    /// Whether the Settings screen should offer the prompt.
    ///
    /// Only once: re-asking after a refusal is a no-op the system answers from
    /// cache, so a button that appeared to do nothing would be worse than no
    /// button. After a refusal the offer is System Settings instead.
    var canRequest: Bool { self == .notDetermined }

    /// Lift the system's own answer.
    ///
    /// `@unknown default` maps forward to ``denied`` rather than ``authorized``:
    /// a status this build has never heard of must not be read as permission.
    init(_ status: UNAuthorizationStatus) {
        switch status {
        case .notDetermined: self = .notDetermined
        case .denied: self = .denied
        case .authorized: self = .authorized
        case .provisional: self = .provisional
        #if os(iOS)
        // iOS-only, and unreachable here: `.ephemeral` is what an App Clip
        // gets, and Sunrise ships none. Mapped to ``provisional`` rather than
        // left to `@unknown default` because it is a real case this SDK
        // declares — the compiler is right to demand it — and because the two
        // mean nearly the same thing: notifications are allowed without the
        // user having been asked, and the grant does not last. Reading it as
        // ``denied`` would be the safe-looking answer and the wrong one; it
        // would silently stop scheduling for a session that is permitted.
        case .ephemeral: self = .provisional
        #endif
        @unknown default: self = .denied
        }
    }
}
