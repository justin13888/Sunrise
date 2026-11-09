#if os(macOS)
import AppKit
#else
import UIKit
#endif
import Foundation

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
/// the calendar and the reminder schedule. The time-zone notice is not
/// posted here: it is one of the catalog's alerts, which ``ReminderScheduler``
/// posts on the re-plan that change causes, on the primary device, with its
/// switch on and through quiet hours like the rest
/// (`docs/08-features/notifications.md` #10).
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
        _ = try? await bridge.reportTimeZone(TimeZone.current.identifier)
    }
}
