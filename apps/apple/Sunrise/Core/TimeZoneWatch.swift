#if os(macOS)
import AppKit
#else
import UIKit
#endif
import Foundation
import UserNotifications

/// Forwards the OS time zone to the core, on both platforms
/// (`docs/10-cross-cutting/time.md` §7).
///
/// The core resolves every floating and all-day time, Today, lateness and the
/// calendar windows in the reader's zone, and the OS is the authority on what
/// that is. So the zone is reported when the vault opens, on
/// `NSSystemTimeZoneDidChange`, and on every return to the foreground — the
/// notification is not delivered to a suspended iOS app, and a laptop that
/// slept through a flight wakes into a new zone with nothing posted.
///
/// Reporting the same zone again is free: the core answers `changed: false`
/// and does nothing. When it did change, the core tells every change-feed
/// follower to re-read (`CoreChange.zoneChanged`), which is what moves Today,
/// the calendar and the reminder schedule; this only posts the one optional
/// notification the core asks for.
@MainActor
final class TimeZoneWatch {
    private let bridge: CoreBridge
    private var observers: [NSObjectProtocol] = []

    init(bridge: CoreBridge) {
        self.bridge = bridge
    }

    /// Report now, and on every zone change and foreground from here on.
    func start() async {
        stop()
        #if os(macOS)
        let foreground = NSApplication.didBecomeActiveNotification
        #else
        let foreground = UIApplication.willEnterForegroundNotification
        #endif
        for name in [Notification.Name.NSSystemTimeZoneDidChange, foreground] {
            observers.append(
                NotificationCenter.default.addObserver(
                    forName: name,
                    object: nil,
                    queue: .main
                ) { [weak self] _ in
                    Task { @MainActor in await self?.report() }
                }
            )
        }
        await report()
    }

    /// Stop observing. The vault this reports to is closing.
    func stop() {
        for observer in observers {
            NotificationCenter.default.removeObserver(observer)
        }
        observers.removeAll()
    }

    /// Report the zone the OS gives this process now.
    func report() async {
        // `TimeZone.current` is cached per process; the reset is what makes it
        // read the zone the OS has just changed to.
        NSTimeZone.resetSystemTimeZone()
        let zone = TimeZone.current.identifier
        guard let change = try? await bridge.reportTimeZone(zone), change.notify else { return }
        await Self.notify(change)
    }

    /// The one local notification `notifications.timezone_changed.enabled`
    /// asks for: the new zone and how many open tasks moved.
    private static func notify(_ change: TimeZoneChange) async {
        let content = UNMutableNotificationContent()
        content.title = "Time zone changed"
        let tasks = change.affectedTasks == 1 ? "1 task" : "\(change.affectedTasks) tasks"
        content.body = "Now in \(change.zone). \(tasks) moved with you."
        let request = UNNotificationRequest(
            identifier: "sunrise.timezone-changed",
            content: content,
            trigger: nil
        )
        // Without permission this is refused, and that is the answer: the
        // preference cannot grant what the user withheld.
        try? await UNUserNotificationCenter.current().add(request)
    }
}
