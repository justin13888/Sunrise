#if os(iOS)
import UIKit
#endif
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
        case .notDetermined: L10n.Notifications.authNotAsked
        case .denied: L10n.Notifications.authDenied
        case .authorized: L10n.Notifications.authAllowed
        case .provisional: L10n.Notifications.authProvisional
        case .unavailable: L10n.Notifications.authUnavailable
        }
    }

    /// The sentence under it. Every one of them says the app still works.
    var explanation: String {
        switch self {
        case .notDetermined: L10n.Notifications.explainNotAsked
        case .denied: L10n.Notifications.explainDenied
        case .authorized: L10n.Notifications.explainAllowed
        case .provisional: L10n.Notifications.explainProvisional
        case let .unavailable(reason): L10n.Notifications.explainUnavailable(reason: reason)
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

/// Whether this device lets Sunrise send at the time-sensitive level, the one
/// wind-down asks for (`docs/08-features/notifications.md` §Wind-down).
///
/// Separate from ``NotificationAuthorization`` because it answers a separate
/// question: a user can allow notifications and still turn time-sensitive
/// delivery off, and then a Focus holds wind-down however Sunrise is
/// configured. Settings shows it beside the wind-down row, the one switch it
/// affects.
///
/// Even ``allowed`` does not carry wind-down through a Sleep Focus: a Focus
/// that silences time-sensitive notifications still holds it unless Sunrise is
/// one of its allowed apps, which only the user can set. That is what the
/// explanation shown on turning wind-down on is for.
enum TimeSensitiveDelivery: Equatable, Sendable {
    /// The system delivers a `.timeSensitive` request as one.
    case allowed
    /// The user turned it off for Sunrise; requests arrive as active.
    case off
    /// This build cannot ask for it: the system reports the setting as not
    /// supported, which is what a binary without the Time Sensitive
    /// Notifications entitlement sees.
    case unsupported

    /// Lift the system's own answer.
    ///
    /// `@unknown default` maps to ``off`` rather than ``allowed``, for the
    /// reason ``NotificationAuthorization/init(_:)`` gives: a value this build
    /// has never heard of must not be read as permission.
    init(_ setting: UNNotificationSetting) {
        switch setting {
        case .enabled: self = .allowed
        case .disabled: self = .off
        case .notSupported: self = .unsupported
        @unknown default: self = .off
        }
    }

    /// Whether the row draws a tick or a warning triangle.
    var isAllowed: Bool { self == .allowed }

    /// The short value beside the row's label.
    var summary: String {
        switch self {
        case .allowed: L10n.Account.NotificationKinds.timeSensitiveAllowed
        case .off: L10n.Account.NotificationKinds.timeSensitiveOff
        case .unsupported: L10n.Account.NotificationKinds.timeSensitiveUnsupported
        }
    }

    /// The sentence under the row, where there is something to say. `nil`
    /// when delivery is allowed: the wind-down caption above already says
    /// what a Sleep Focus does, and a second line repeating it is noise.
    var caption: String? {
        switch self {
        case .allowed: nil
        case .off: L10n.Account.NotificationKinds.timeSensitiveOffCaption
        case .unsupported: L10n.Account.NotificationKinds.timeSensitiveUnsupportedCaption
        }
    }

    /// Where the user lets wind-down through a Sleep Focus.
    ///
    /// On the Mac, the Focus pane of System Settings, where Sleep's allowed
    /// apps are listed. iOS publishes no URL for the Focus settings, and the
    /// private `App-prefs:` scheme is grounds for App Review to refuse the
    /// app, so on iPhone and iPad it is Sunrise's own notification settings:
    /// the public page nearest to it, and the one that holds the
    /// time-sensitive switch this row reports.
    @MainActor
    static var focusSettingsURL: URL? {
        #if os(macOS)
        URL(string: "x-apple.systempreferences:com.apple.Focus-Settings.extension")
        #else
        URL(string: UIApplication.openNotificationSettingsURLString)
        #endif
    }

    /// The label of the button that opens ``focusSettingsURL``, naming the
    /// page it really opens.
    static var focusSettingsLabel: String {
        #if os(macOS)
        L10n.Account.NotificationKinds.openFocusSettings
        #else
        L10n.Account.NotificationKinds.openNotificationSettings
        #endif
    }

    /// Whether flipping a kind's switch should explain the Sleep Focus.
    ///
    /// Only wind-down, and only on the way from off to on: that is the moment
    /// the user has asked for something a Sleep Focus will quietly hold, and
    /// explaining it on turning it off, or again while it stays on, would
    /// teach them to dismiss it unread.
    static func explainsSleepFocus(_ kind: NotificationKind, wasOn: Bool, isOn: Bool) -> Bool {
        kind == .windDown && !wasOn && isOn
    }
}
