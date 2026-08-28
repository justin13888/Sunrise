import Foundation

/// One notification from the core's change stream.
///
/// `lagged` is not telemetry. The channel behind it is a bounded
/// `tokio::sync::broadcast`, and it drops rather than blocks: a spike pushing
/// 5000 events past a slow consumer measured 257 seen and 4743 lost. That is
/// what an offline device catching up looks like, so a consumer that ignores
/// this case shows stale data precisely when it matters most.
enum CoreChange: Equatable, Sendable {
    /// This entity was created, updated, deleted or forgotten.
    case entity(EntityRef)
    /// `skipped` notifications were dropped. Re-read everything; do not try to
    /// reconstruct what was missed.
    case lagged(skipped: UInt64)
    /// The vault closed. Nothing further will arrive.
    case closed
}

extension CoreChange {
    /// Every `ChangeEvent` case carries an entity and nothing else — the core
    /// sends a prompt to re-read, never a payload — so the four collapse to
    /// one here.
    init(_ event: ChangeEvent) {
        switch event {
        case let .created(entity), let .updated(entity),
             let .deleted(entity), let .forgotten(entity):
            self = .entity(entity)
        }
    }
}

/// What one repaint has to cover.
struct ChangeBatch: Equatable, Sendable {
    /// Entities known to have changed since the last repaint.
    let touched: Set<EntityRef>
    /// Whether `touched` is the whole story. `false` after a lag: the feed
    /// dropped notifications, so anything on screen may be stale and every
    /// query has to run again.
    let isComplete: Bool
    /// The vault closed during this window.
    let isClosed: Bool
}

/// Folds a burst of notifications into the one repaint they are worth.
///
/// Pure and synchronous on purpose: the interesting decision — that a lag
/// invalidates the id list rather than adding to it — is then testable without
/// a clock, a vault, or a task.
struct ChangeAccumulator {
    private var touched: Set<EntityRef> = []
    private var lostNotifications = false
    private var closed = false

    /// Whether a drain would produce anything.
    var hasPending: Bool { !touched.isEmpty || lostNotifications || closed }

    mutating func record(_ change: CoreChange) {
        switch change {
        case let .entity(id):
            touched.insert(id)
        case .lagged:
            // The ids collected so far are still true, but they are no longer
            // *all* of the truth. Keeping them and clearing the flag would be
            // the bug: a partial list that looks complete.
            lostNotifications = true
        case .closed:
            closed = true
        }
    }

    /// Take the pending batch, if there is one, and reset.
    mutating func drain() -> ChangeBatch? {
        guard hasPending else { return nil }
        let batch = ChangeBatch(
            touched: touched,
            isComplete: !lostNotifications,
            isClosed: closed
        )
        touched.removeAll(keepingCapacity: true)
        lostNotifications = false
        // `closed` is deliberately sticky: a stream that has ended has not
        // un-ended, and the consumer may drain again before it notices.
        return batch
    }
}

/// The foreign half of `SunriseCore::subscribe_changes`, as an `AsyncStream`.
///
/// Implements every method of the trait. Omitting `onLagged` compiles and then
/// loses data silently, which is why it is a stored requirement rather than a
/// defaulted one.
final class ChangeFeed: ChangeListener {
    private let continuation: AsyncStream<CoreChange>.Continuation

    /// The stream, and the listener that feeds it.
    static func make() -> (stream: AsyncStream<CoreChange>, listener: ChangeFeed) {
        let (stream, continuation) = AsyncStream<CoreChange>.makeStream(
            // Unbounded: dropping here would re-create, in Swift, exactly the
            // loss `onLagged` exists to report. The consumer coalesces
            // instead, so a burst costs memory for one window and no more.
            bufferingPolicy: .unbounded
        )
        return (stream, ChangeFeed(continuation: continuation))
    }

    private init(continuation: AsyncStream<CoreChange>.Continuation) {
        self.continuation = continuation
    }

    // Called from a tokio worker thread, never the main thread.
    func onChange(event: ChangeEvent) {
        continuation.yield(CoreChange(event))
    }

    func onLagged(skipped: UInt64) {
        continuation.yield(.lagged(skipped: skipped))
    }

    func onClosed() {
        continuation.yield(.closed)
        continuation.finish()
    }
}

/// Holds the accumulator and the one fact the async side needs: whether a
/// flush is already scheduled.
private actor Coalescer {
    private var accumulator = ChangeAccumulator()
    private var windowIsOpen = false

    /// Record a notification. Returns `true` when this one **opened** the
    /// window, which is the caller's cue to schedule the flush — so a burst of
    /// 5000 schedules exactly one.
    func record(_ change: CoreChange) -> Bool {
        accumulator.record(change)
        guard !windowIsOpen else { return false }
        windowIsOpen = true
        return true
    }

    /// Close the window and take everything that landed in it.
    func closeWindow() -> ChangeBatch? {
        windowIsOpen = false
        return accumulator.drain()
    }
}

extension AsyncStream where Element == CoreChange {
    /// Collapse notifications arriving inside `window` into one batch.
    ///
    /// 50 ms is the window the TUI settled on: long enough that an N-op sync
    /// catch-up repaints once, short enough that a keystroke-driven edit still
    /// feels immediate.
    ///
    /// The window opens on the *first* notification rather than running on a
    /// timer, so an idle vault costs nothing and a lone change waits 50 ms
    /// rather than up to a tick.
    func coalesced(window: Duration = .milliseconds(50)) -> AsyncStream<ChangeBatch> {
        AsyncStream<ChangeBatch> { continuation in
            let coalescer = Coalescer()
            let pump = Task {
                await withTaskGroup(of: Void.self) { group in
                    for await change in self {
                        guard await coalescer.record(change) else { continue }
                        group.addTask {
                            try? await Task.sleep(for: window)
                            if let batch = await coalescer.closeWindow() {
                                continuation.yield(batch)
                            }
                        }
                    }
                }
                // Upstream ended mid-window; do not swallow what it left.
                if let batch = await coalescer.closeWindow() {
                    continuation.yield(batch)
                }
                continuation.finish()
            }
            continuation.onTermination = { _ in pump.cancel() }
        }
    }
}
