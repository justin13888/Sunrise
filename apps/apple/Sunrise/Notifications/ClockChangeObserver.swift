import Foundation
#if os(iOS)
import UIKit
#endif

/// Runs an action when the clock or the calendar jumps.
///
/// The zone is ``TimeZoneWatch``'s, and a zone change reaches the scheduler
/// through the change feed. What the feed cannot say is that the clock itself
/// moved — set by hand, corrected by the network, or the day rolled over at
/// midnight — and every fire time in a plan is measured from "now". iOS folds
/// all of those into one significant-time-change notification; macOS posts
/// them separately.
@MainActor
final class ClockChangeObserver {
    private var observers: [NSObjectProtocol] = []

    /// Run `action` on every jump from now on, replacing any earlier action.
    func start(_ action: @escaping @MainActor @Sendable () async -> Void) {
        stop()
        var names: [Notification.Name] = [.NSSystemClockDidChange, .NSCalendarDayChanged]
        #if os(iOS)
        names.append(UIApplication.significantTimeChangeNotification)
        #endif
        let posts = NotificationCenter.default
        for name in names {
            observers.append(
                posts.addObserver(forName: name, object: nil, queue: .main) { _ in
                    _Concurrency.Task { @MainActor in await action() }
                }
            )
        }
    }

    /// Stop running it.
    func stop() {
        for observer in observers {
            NotificationCenter.default.removeObserver(observer)
        }
        observers.removeAll()
    }
}
