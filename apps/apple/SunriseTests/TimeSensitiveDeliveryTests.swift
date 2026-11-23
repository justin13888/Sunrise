import Foundation
import Testing
import UserNotifications

@testable import Sunrise

/// The time-sensitive state Settings shows beside wind-down, and when turning
/// wind-down on explains the Sleep Focus (`docs/08-features/notifications.md`
/// §Wind-down).
@MainActor
struct TimeSensitiveDeliveryTests {
    /// Each of the system's three answers is its own state, and only
    /// `.enabled` reads as allowed: a setting the build cannot use must not
    /// draw a tick.
    @Test
    func theSystemsAnswerIsLiftedOneToOne() {
        #expect(TimeSensitiveDelivery(.enabled) == .allowed)
        #expect(TimeSensitiveDelivery(.disabled) == .off)
        #expect(TimeSensitiveDelivery(.notSupported) == .unsupported)
        #expect(TimeSensitiveDelivery.allowed.isAllowed)
        #expect(!TimeSensitiveDelivery.off.isAllowed)
        #expect(!TimeSensitiveDelivery.unsupported.isAllowed)
    }

    /// Three different values, and a sentence for exactly the two that need
    /// one: what to do differs between "you turned it off" and "this build
    /// cannot".
    @Test
    func eachStateSaysSomethingDifferent() throws {
        let states: [TimeSensitiveDelivery] = [.allowed, .off, .unsupported]
        #expect(Set(states.map(\.summary)).count == 3)
        #expect(states.allSatisfy { !$0.summary.isEmpty })
        #expect(TimeSensitiveDelivery.allowed.caption == nil)
        let off = try #require(TimeSensitiveDelivery.off.caption)
        let unsupported = try #require(TimeSensitiveDelivery.unsupported.caption)
        #expect(off != unsupported)
    }

    /// Only wind-down, only off to on. Any other kind, turning it off, or a
    /// redraw that leaves it on, explains nothing.
    @Test
    func onlyTurningWindDownOnExplainsTheSleepFocus() {
        #expect(TimeSensitiveDelivery.explainsSleepFocus(.windDown, wasOn: false, isOn: true))
        #expect(!TimeSensitiveDelivery.explainsSleepFocus(.windDown, wasOn: true, isOn: false))
        #expect(!TimeSensitiveDelivery.explainsSleepFocus(.windDown, wasOn: true, isOn: true))
        #expect(!TimeSensitiveDelivery.explainsSleepFocus(.windDown, wasOn: false, isOn: false))
        for kind in NotificationCatalog.kinds.map(\.kind) where kind != .windDown {
            #expect(!TimeSensitiveDelivery.explainsSleepFocus(kind, wasOn: false, isOn: true))
        }
    }

    /// The button has somewhere to go on this platform, and it is the system's
    /// settings rather than a link back into the app.
    @Test
    func theFocusSettingsLinkLeavesTheApp() throws {
        let url = try #require(TimeSensitiveDelivery.focusSettingsURL)
        #expect(url.scheme != DeepLink.scheme)
        #if os(macOS)
        #expect(url.absoluteString == "x-apple.systempreferences:com.apple.Focus-Settings.extension")
        #endif
        #expect(!TimeSensitiveDelivery.focusSettingsLabel.isEmpty)
    }
}
