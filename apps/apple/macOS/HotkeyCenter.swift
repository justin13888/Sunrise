import AppKit
import Carbon.HIToolbox
import Foundation

extension Notification.Name {
    /// Posted when the global quick-capture hotkey fires.
    ///
    /// A notification rather than a callback because the receiver is a C
    /// function pointer: `InstallEventHandler` takes a `@convention(c)`
    /// closure, which can capture nothing at all. Posting is the one thing such
    /// a closure can do that reaches Swift.
    static let sunriseQuickCapture = Notification.Name("dev.sunrise.quickCapture")
}

/// Whether the system-wide hotkey is live, and why not when it is not.
///
/// Three cases and not a `Bool`, because they need three different sentences
/// in front of the user. "Another app already uses ⌘⇧N" is actionable;
/// "unavailable" is not, and telling someone to check their permissions when
/// the real problem is a conflict sends them somewhere that cannot help.
enum HotkeyStatus: Equatable {
    /// Registered; the hotkey works from any app.
    case active
    /// Something else holds this combination system-wide.
    case taken
    /// Registration failed for another reason, with the OS status code.
    case unavailable(Int32)
    /// Not registered yet.
    case idle

    var isActive: Bool { self == .active }

    var explanation: String {
        switch self {
        case .active:
            "⌘⇧N opens quick capture from anywhere."
        case .taken:
            "Another app already uses ⌘⇧N. Quick capture is still on the menu bar."
        case let .unavailable(code):
            "The system refused the shortcut (error \(code)). "
                + "Quick capture is still on the menu bar."
        case .idle:
            "Not registered."
        }
    }
}

/// The system-wide quick-capture hotkey, ⌘⇧N.
///
/// # Why Carbon
///
/// `RegisterEventHotKey` is the API that actually reserves a key combination
/// with the window server. The obvious alternative,
/// `NSEvent.addGlobalMonitorForEvents`, *observes* every keystroke in every
/// app and therefore requires the Accessibility permission — which
/// `docs/07-clients/desktop.md` names, and which this does not need. Reserving
/// one combination is both narrower and less to ask of the user; a task app
/// has no business being able to read what someone types into their bank.
///
/// [`accessibilityIsTrusted`] is still reported, because the accepted spec says
/// the feature needs it and a settings screen that silently disagreed with the
/// documentation would be worse than one that explains the difference.
///
/// # Failure is a state, not a crash
///
/// Registration fails whenever another app got there first. That is ordinary,
/// not exceptional: the menu bar item still opens capture, and
/// [`HotkeyStatus`] is what a settings screen shows instead of pretending.
@MainActor
final class HotkeyCenter {
    private(set) var status: HotkeyStatus = .idle

    private var hotKey: EventHotKeyRef?
    private var handler: EventHandlerRef?

    /// ⌘⇧N — `docs/07-clients/interaction-patterns.md` §Quick capture.
    private static let keyCode = UInt32(kVK_ANSI_N)
    private static let modifiers = UInt32(cmdKey | shiftKey)
    /// `'SUNR'`, the four-character signature Carbon identifies the hotkey by.
    private static let signature: OSType = 0x5355_4E52

    /// Register the hotkey. Idempotent: a second call while active is a no-op
    /// rather than a second registration the first would then leak.
    func register() {
        guard hotKey == nil else { return }

        var spec = EventTypeSpec(
            eventClass: OSType(kEventClassKeyboard),
            eventKind: UInt32(kEventHotKeyPressed)
        )
        // Captures nothing: `InstallEventHandler` takes a C function pointer,
        // and posting is all such a closure can do. Carbon dispatches it on the
        // main thread, which is where the notification is then observed.
        let callback: EventHandlerUPP = { _, _, _ in
            NotificationCenter.default.post(name: .sunriseQuickCapture, object: nil)
            return noErr
        }
        InstallEventHandler(GetApplicationEventTarget(), callback, 1, &spec, nil, &handler)

        var ref: EventHotKeyRef?
        let result = RegisterEventHotKey(
            Self.keyCode,
            Self.modifiers,
            EventHotKeyID(signature: Self.signature, id: 1),
            GetApplicationEventTarget(),
            0,
            &ref
        )
        if result == noErr, let ref {
            hotKey = ref
            status = .active
        } else {
            status = Self.classify(result)
        }
    }

    /// Release the hotkey, so the combination goes back to whoever wants it.
    func unregister() {
        if let hotKey { UnregisterEventHotKey(hotKey) }
        if let handler { RemoveEventHandler(handler) }
        hotKey = nil
        handler = nil
        status = .idle
    }

    // No `deinit`. The Carbon refs are main-actor isolated and a `deinit` is
    // not, which Swift 6 refuses outright — and reaching for `nonisolated(unsafe)`
    // to get around it would be trading a compile error for a data race on a
    // window-server registration. `unregister()` is the release path, and the
    // one owner of this type lives as long as the app does.

    /// Whether this process holds the Accessibility permission.
    ///
    /// Not required by the hotkey above; reported because the accepted spec
    /// names it and because a future feature that reads keystrokes would need
    /// it. Read-only — nothing here prompts for it, since asking for a
    /// permission a feature does not use is how an app teaches people to grant
    /// permissions without reading them.
    static var accessibilityIsTrusted: Bool { AXIsProcessTrusted() }

    /// Open the pane where the permission is granted.
    static func openAccessibilitySettings() {
        guard let url = URL(
            string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
        ) else { return }
        NSWorkspace.shared.open(url)
    }

    /// Which failure this OS status is.
    ///
    /// `eventHotKeyExistsErr` is the ordinary one — another app got the
    /// combination first — and is the whole reason this returns a case rather
    /// than throwing.
    nonisolated static func classify(_ status: OSStatus) -> HotkeyStatus {
        status == Self.alreadyTaken ? .taken : .unavailable(Int32(status))
    }

    /// `eventHotKeyExistsErr`, the documented value.
    ///
    /// Named rather than left as a literal in the comparison, because Carbon's
    /// own constant is not exported to Swift and the current SDK does not ship
    /// the header it lives in. If it were ever wrong, the failure is benign:
    /// the status falls through to `.unavailable`, whose sentence carries the
    /// code and still points at the menu bar.
    nonisolated static let alreadyTaken: OSStatus = -9878
}
