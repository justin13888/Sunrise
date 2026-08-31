import Foundation

/// Focus: what to work on, the session running, and what finishing it
/// released.
///
/// The core stores **no running timer**. A session is a `start` op, and its
/// elapsed time is derived from the injected clock every time it is asked
/// for. So this model keeps no stopwatch of its own either: it holds the
/// `SessionRow` the core gave it and re-reads `now` on a tick, then asks the
/// seam what that adds up to. A local `Date()` stopwatch would drift away
/// from the number the vault would report — and from the one the session
/// ends with.
@MainActor
@Observable
final class FocusModel {
    /// The planner's ranked queue.
    private(set) var plan: [PlanRow] = []
    /// The session running on this vault, if any. Not "on this device": a
    /// session started on a paired machine is the same session, and finding
    /// one running is a normal state after a sync, not an error.
    private(set) var running: SessionRow?
    /// The running session's derived numbers at the last tick.
    private(set) var progress: SessionProgress?
    /// Folded totals for the picked stream, or the whole vault.
    private(set) var stats: FocusTotals?
    /// What the last completion released. Cleared when it is dismissed.
    private(set) var cascade: Cascade?
    private(set) var names = NameBook()
    private(set) var errorMessage: String?

    /// The session budget the planner ranks against. `nil` is "any", which
    /// drops energy out of the ranking rather than meaning "no energy".
    var energy: Energy? {
        didSet { Task { await refresh() } }
    }

    /// How a session would be sized.
    var length: SessionLength = .onePomodoro {
        didSet { Task { await refresh() } }
    }

    /// Narrow the planner and the stats to one stream.
    var stream: EntityRef? {
        didSet { Task { await refresh() } }
    }

    private let bridge: CoreBridge
    private let tick: Duration
    private var ticker: _Concurrency.Task<Void, Never>?

    /// One second. The timer is a derived reading, so the tick decides only
    /// how often the reading is refreshed — not how it is computed.
    ///
    /// The ticker is stopped by the view's `onDisappear` rather than by a
    /// `deinit`: this type is `@MainActor`, and a `deinit` cannot touch
    /// isolated state.
    init(bridge: CoreBridge, tick: Duration = .seconds(1)) {
        self.bridge = bridge
        self.tick = tick
    }

    var isRunning: Bool { running != nil }

    /// The task the running session is on, if the planner is holding it.
    var runningTitle: String? {
        guard let running else { return nil }
        return plan.first { $0.task.id == running.start.taskId }?.task.title
            ?? runningTaskTitle
    }

    /// Title read directly, for a session on a task the planner does not rank
    /// — a completed or blocked one, or one from another device.
    private var runningTaskTitle: String?

    func refresh() async {
        let now = await bridge.nowMs()
        names = await NameBook.load(from: bridge)
        do {
            if case let .focusPlan(rows) = try await bridge.query(
                .focusPlan(stream: stream, energy: energy, length: length, limit: 25)
            ) {
                plan = rows
            }
            if case let .focusSessions(sessions) = try await bridge.query(.runningFocusSessions) {
                // Newest first, and only one is ever shown: two running
                // sessions is a state the core allows and a screen cannot.
                running = sessions.max { $0.start.startedAt < $1.start.startedAt }
            }
            if case let .focusStats(totals) = try await bridge.query(
                .focusStats(stream: stream, sinceMs: nil, nowMs: now)
            ) {
                stats = totals
            }
            await loadRunningTitle()
            updateProgress(now: now)
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// Re-read on every change batch, and tick the timer in between.
    ///
    /// Two loops rather than one: a change batch is other people's writes, and
    /// a tick is this screen's clock. Folding them together would either make
    /// the timer wait for a write or make every second cost four queries.
    func follow() async {
        startTicking()
        for await batch in await bridge.changes() {
            guard !batch.isClosed else { return }
            await refresh()
        }
    }

    func stopTicking() {
        ticker?.cancel()
        ticker = nil
    }

    // MARK: - The session

    func start(_ row: PlanRow) async {
        await run(.startFocus(
            taskId: row.task.id,
            kind: .work,
            length: length,
            energy: energy
        ))
    }

    /// Log an interruption against the running session. One tap, one op.
    func logInterruption(_ reason: InterruptionReason) async {
        guard let running else { return }
        await run(.logInterruption(session: running.start.id, reason: reason))
    }

    /// End the session, freezing the derived elapsed time.
    ///
    /// `actualFocusedMs` is left absent on purpose: the core derives it from
    /// its own clock, and a number sent from here would be this device's
    /// opinion of how long the session was.
    func end(completingTask: Bool) async {
        guard let running else { return }
        let task = running.start.taskId
        await run(.endFocus(
            session: running.start.id,
            actualFocusedMs: nil,
            completedTask: completingTask
        ))
        if completingTask {
            await loadCascade(for: task)
        }
    }

    func dismissCascade() { cascade = nil }

    /// What completing `task` released.
    ///
    /// Read after the fact rather than derived from the row: the cascade is a
    /// graph query, and which tasks are *still* blocked is not something the
    /// completing screen can know.
    private func loadCascade(for task: EntityRef) async {
        guard case let .unblockCascade(result)? =
            try? await bridge.query(.unblockCascade(task: task))
        else { return }
        cascade = result.released.isEmpty && result.stillBlocked.isEmpty ? nil : result
    }

    private func loadRunningTitle() async {
        guard let running else {
            runningTaskTitle = nil
            return
        }
        guard case let .task(item)? =
            try? await bridge.query(.entityById(id: running.start.taskId))
        else { return }
        runningTaskTitle = item.title
    }

    private func updateProgress(now: UInt64) {
        progress = running.map { sessionProgress(session: $0, nowMs: now) }
    }

    private func startTicking() {
        ticker?.cancel()
        ticker = _Concurrency.Task { [tick, bridge] in
            while !_Concurrency.Task.isCancelled {
                try? await _Concurrency.Task.sleep(for: tick)
                guard !_Concurrency.Task.isCancelled else { return }
                // Only the reading is refreshed. Nothing is queried, so an
                // idle Focus screen costs one clock read per second.
                let now = await bridge.nowMs()
                updateProgress(now: now)
            }
        }
    }

    private func run(_ command: CoreCommand) async {
        do {
            _ = try await bridge.submit(command)
            await refresh()
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}

extension SessionLength {
    /// In the order a picker should offer them: shortest commitment first.
    ///
    /// Spelled out rather than derived: the generated enum is not
    /// `CaseIterable`, and the order a picker wants is a presentation choice
    /// anyway.
    static let offered: [SessionLength] = [.onePomodoro, .sizedToEstimate, .untilDone]
}
