#if os(macOS)
import AppKit
#else
import UIKit
#endif
import Foundation
import WidgetKit

/// Keeps the widgets' snapshot in step with the open vault.
///
/// The widget extension cannot read the vault (see ``WidgetSnapshot``), so
/// the app is the only thing that can tell it what Today holds. This writes
/// the snapshot when the vault is attached, after every change the core
/// reports, every fifteen minutes while the app runs, and whenever the app
/// comes to the foreground — and asks WidgetKit to redraw only when the
/// snapshot would draw something different.
///
/// **What is on Today is the core's decision, not this file's.** The rows are
/// `Query::Today` in the order it returns them, each one's section is
/// `today_section`, and the counts are the same open-task counts the menu bar
/// shows. The only thing decided here is the projection: which fields, and how
/// many rows.
@MainActor
final class WidgetPublisher {
    /// The same debounce the menu bar uses, for the same reason: this is a
    /// glance, and a burst of changes is one redraw.
    static let changeDebounce: Duration = .milliseconds(500)

    /// How often the snapshot is re-read with nothing having changed.
    ///
    /// A snapshot can go stale without a single write: at midnight a task due
    /// today becomes overdue, and one scheduled for tomorrow arrives. Fifteen
    /// minutes is the cadence `docs/07-clients/mobile-ios.md` gives the
    /// background refresh, and a re-read that finds nothing new costs two
    /// queries and no redraw.
    static let tickInterval: Duration = .seconds(15 * 60)

    private let bridge: CoreBridge
    private let store: WidgetSnapshotStore
    private let reload: @MainActor () -> Void

    /// What the file holds now, as far as this process knows.
    private(set) var published: WidgetSnapshot?
    /// Why the last attempt wrote nothing, or `nil` after a good one.
    private(set) var errorMessage: String?
    /// Set by ``stop()``; no write happens after it.
    private(set) var isStopped = false

    init(
        bridge: CoreBridge,
        store: WidgetSnapshotStore,
        reload: @escaping @MainActor () -> Void = { WidgetCenter.shared.reloadAllTimelines() }
    ) {
        self.bridge = bridge
        self.store = store
        self.reload = reload
        // Seeded from disk, so a relaunch that finds Today as it left it
        // spends none of the reload budget saying so.
        published = store.read()
    }

    /// Project Today and the Inbox into what the widgets draw.
    ///
    /// Pure: `today_section`, the one seam function it calls, reads nothing
    /// but its arguments, and neither does `DeepLink.url`.
    static func snapshot(
        today: [TaskItem],
        inbox: [TaskItem],
        nowMs: UInt64,
        timeZone: String
    ) -> WidgetSnapshot {
        let open = today.filter { $0.state != .done && $0.state != .cancelled }
        var overdue = 0
        var rows: [WidgetSnapshot.Row] = []
        for task in open {
            let section = WidgetSnapshot.Section(todaySection(
                scheduledAt: task.scheduledAt,
                dueAt: task.dueAt,
                nowMs: nowMs,
                tz: timeZone
            ))
            if section == .overdue { overdue += 1 }
            guard rows.count < WidgetSnapshot.rowLimit else { continue }
            rows.append(WidgetSnapshot.Row(
                id: task.id,
                title: task.title,
                section: section,
                link: DeepLink.task(task.id, .open).url
            ))
        }
        return WidgetSnapshot(
            writtenAtMs: Int64(clamping: nowMs),
            outstanding: open.count,
            overdue: overdue,
            inbox: inbox.count { $0.state != .done && $0.state != .cancelled },
            rows: rows
        )
    }

    /// Read the vault and write the snapshot if it changed.
    func publish() async {
        let now = await bridge.nowMs()
        do {
            guard case let .tasks(today) = try await bridge.query(.today(nowMs: now, contexts: [])),
                  case let .tasks(inbox) = try await bridge.query(.inbox) else {
                errorMessage = "The core answered a task query with something else."
                return
            }
            let next = Self.snapshot(
                today: today,
                inbox: inbox,
                nowMs: now,
                timeZone: TimeZone.current.identifier
            )
            errorMessage = nil
            // Checked after the reads, which are the suspension points: a
            // lock that landed while they ran has already erased the file,
            // and writing now would put the titles back on the Home Screen.
            guard !isStopped else { return }
            if let published, published.drawsTheSameAs(next) { return }
            try store.write(next)
            published = next
            reload()
        } catch {
            // Left as it was rather than erased. A failed read is not an empty
            // Today, and blanking the Home Screen over a transient error would
            // be the louder mistake; the stamp says how old it is.
            errorMessage = error.localizedDescription
        }
    }

    /// Publish now, then on every change and every ``tickInterval`` until
    /// cancelled or the vault closes.
    func run() async {
        await publish()
        await withTaskGroup(of: Void.self) { group in
            group.addTask { await self.follow() }
            group.addTask { await self.tick() }
            // The feed ending is the vault closing; the timer has nothing
            // left to read either.
            _ = await group.next()
            group.cancelAll()
        }
    }

    /// Republish on a change, debounced. Returns when the vault closes.
    func follow(debounce: Duration = WidgetPublisher.changeDebounce) async {
        for await batch in await bridge.changes() {
            guard !batch.isClosed else { return }
            try? await _Concurrency.Task.sleep(for: debounce)
            guard !_Concurrency.Task.isCancelled else { return }
            await publish()
        }
    }

    private func tick() async {
        while !_Concurrency.Task.isCancelled {
            do {
                try await _Concurrency.Task.sleep(for: Self.tickInterval)
            } catch {
                return
            }
            await publish()
        }
    }

    /// Never write again. The vault this reads from is going away, and a
    /// publish still in flight must not outlive it on disk.
    func stop() {
        isStopped = true
    }
}

/// The widgets' side of ``AppSurfaces``: one publisher per attached vault, and
/// the snapshot's removal when no vault is open.
///
/// Its own object rather than more of `AppSurfaces`, and optional there: the
/// two app entry points pass ``appGroup()``, and everything else — every test
/// that builds `AppSurfaces()` — gets `nil`, so nothing but the app itself
/// ever writes into the container the installed widgets are reading.
@MainActor
final class WidgetFeed {
    let store: WidgetSnapshotStore
    private let reload: @MainActor () -> Void
    /// The publisher for the vault attached now, or `nil` with none.
    private(set) var publisher: WidgetPublisher?
    private var task: _Concurrency.Task<Void, Never>?
    private var terminationObserver: NSObjectProtocol?

    init(
        store: WidgetSnapshotStore,
        reload: @escaping @MainActor () -> Void = { WidgetCenter.shared.reloadAllTimelines() }
    ) {
        self.store = store
        self.reload = reload
    }

    /// The feed into this app's App Group, or `nil` where it has none.
    ///
    /// Withdraws at once, because no vault is open at launch: a snapshot on
    /// disk now was left by a process that ended without saying so (iOS
    /// killing a suspended app, a crash). And withdraws again when the app is
    /// told it is terminating, so a quit takes the titles with it.
    static func appGroup() -> WidgetFeed? {
        guard let store = WidgetSnapshotStore.appGroup() else { return nil }
        let feed = WidgetFeed(store: store)
        feed.withdraw()
        feed.withdraw(whenever: terminationNotification)
        return feed
    }

    #if os(macOS)
    static let terminationNotification = NSApplication.willTerminateNotification
    #else
    static let terminationNotification = UIApplication.willTerminateNotification
    #endif

    /// Withdraw whenever `center` posts `name`.
    ///
    /// Delivered on the posting thread rather than through a queue or a task:
    /// the termination notification is posted on the main thread and the
    /// process exits as soon as its observers return, so an erase scheduled
    /// for later would never run.
    func withdraw(whenever name: Notification.Name, from center: NotificationCenter = .default) {
        if let terminationObserver { center.removeObserver(terminationObserver) }
        terminationObserver = center.addObserver(forName: name, object: nil, queue: nil) { [weak self] _ in
            guard let self else { return }
            MainActor.assumeIsolated { self.withdraw() }
        }
    }

    /// Publish from `bridge` until the next ``start(bridge:)`` or
    /// ``withdraw()``.
    ///
    /// Replacing a vault rather than opening the first one withdraws the old
    /// snapshot first: the previous vault's titles come off the Home Screen
    /// before the next vault's go on, so a switch that fails half way leaves
    /// nothing behind that belongs to a vault that is no longer open.
    func start(bridge: CoreBridge) {
        if publisher != nil { withdraw() }
        let next = WidgetPublisher(bridge: bridge, store: store, reload: reload)
        publisher = next
        task = _Concurrency.Task { await next.run() }
    }

    /// Re-read Today now. The app coming to the foreground calls this, which
    /// is what catches a day that rolled over while it was suspended.
    func refresh() {
        guard let publisher else { return }
        _Concurrency.Task { await publisher.publish() }
    }

    /// Stop publishing, take the snapshot off disk, and redraw the widgets
    /// empty.
    ///
    /// Called whenever the session leaves `.unlocked` — a lock, a sign-out, a
    /// failure, the first beat of a switch — by `AppSurfaces.detach()`, and
    /// at launch and termination (``appGroup()``).
    /// The other vault surfaces stay bound across a lock, because the window
    /// is replaced by `LockedView` and nothing reaches them; the widgets are
    /// drawn by another process with no lock of its own, so this one acts.
    func withdraw() {
        // Stopped as well as cancelled: cancellation reaches the feed, and
        // `stop()` also reaches a foreground refresh still in flight.
        publisher?.stop()
        task?.cancel()
        task = nil
        publisher = nil
        // Best effort: there is no one to report a failure to at lock time,
        // and the widget refuses a snapshot it cannot read anyway.
        try? store.erase()
        reload()
    }
}

extension WidgetSnapshot.Section {
    init(_ section: TodaySection) {
        switch section {
        case .overdue: self = .overdue
        case .due: self = .due
        case .scheduled: self = .scheduled
        }
    }
}
