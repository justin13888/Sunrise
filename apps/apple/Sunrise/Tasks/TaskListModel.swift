import Foundation

/// One list of tasks, kept current.
///
/// Every mutation goes to the core and comes back through the change stream;
/// nothing is patched locally on the way. That is slower by one round trip and
/// it is the reason a completion made on another device and a completion made
/// here look identical on screen.
/// One entry of a "move to stream" picker.
///
/// A named type rather than the tuple it wants to be, because `ForEach` needs a
/// key path to the id and Swift has none for a tuple label.
struct StreamChoice: Identifiable, Hashable, Sendable {
    let id: EntityRef
    let name: String
}

@MainActor
@Observable
final class TaskListModel {
    private(set) var kind: TaskListKind
    private(set) var tasks: [TaskItem] = []
    private(set) var groups: [TodayGroup] = []
    private(set) var names = NameBook()
    private(set) var nowMs: UInt64 = 0
    private(set) var errorMessage: String?

    /// The device's zone, resolved once per refresh so two rows in one list
    /// cannot disagree about what "today" is.
    private(set) var timeZone: String = TimeZone.current.identifier

    /// The bridge, for the panes a task editor opens over one row —
    /// attachments and the activity timeline. Exposed rather than proxied: both
    /// own their own queries and their own change subscription, and routing
    /// them through this model would make it the owner of state it does not
    /// display.
    let bridge: CoreBridge

    /// The hand-arranged row order for this list, on this device. See
    /// ``ListOrderStore`` for why it is a device fact and not a vault one.
    private let order: ListOrderStore

    init(
        bridge: CoreBridge,
        kind: TaskListKind = .todayAll,
        order: ListOrderStore = ListOrderStore()
    ) {
        self.bridge = bridge
        self.kind = kind
        self.order = order
    }

    func show(_ kind: TaskListKind) async {
        self.kind = kind
        await refresh()
    }

    /// Re-run the query behind this list.
    func refresh() async {
        timeZone = TimeZone.current.identifier
        nowMs = await bridge.nowMs()
        names = await NameBook.load(from: bridge)
        guard kind.isWorthQuerying else {
            tasks = []
            groups = []
            errorMessage = nil
            return
        }
        do {
            guard case let .tasks(rows) = try await bridge.query(kind.query(nowMs: nowMs)) else {
                tasks = []
                groups = []
                return
            }
            // The remembered arrangement, laid over the query. A list nobody
            // has dragged in is returned exactly as the core ordered it.
            tasks = order.apply(rows, for: kind)
            groups = kind.isToday
                ? TaskGrouping.today(tasks: rows, nowMs: nowMs, timeZone: timeZone)
                : []
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// Follow the change stream for as long as the view is on screen.
    ///
    /// A batch that is not complete means the feed dropped notifications, so
    /// the ids in it are not the whole story — the only safe response is the
    /// same full re-query, which is what this does either way. Keeping the two
    /// paths identical is deliberate: an optimisation that patched only the
    /// named rows would be correct until the first lag and wrong after it.
    func follow() async {
        for await batch in await bridge.changes() {
            guard !batch.isClosed else { return }
            await refresh()
        }
    }

    func facets(for task: TaskItem) -> TaskFacets {
        TaskFacets(task: task, nowMs: nowMs, timeZone: timeZone, names: names)
    }

    /// The rows in the order they are drawn.
    ///
    /// Today is sectioned, so its screen order is the sections' order and not
    /// `tasks`'s. The keyboard moves down the screen rather than down the query,
    /// which means the cursor has to walk this and not that.
    var ordered: [TaskItem] {
        kind.isToday ? groups.flatMap(\.tasks) : tasks
    }

    /// Resolve ids the selection is holding back into rows.
    ///
    /// Filtered from the list rather than looked up one at a time, so the
    /// result is in screen order and cannot contain a row that has since left.
    func rows(for ids: [EntityRef]) -> [TaskItem] {
        let wanted = Set(ids)
        return ordered.filter { wanted.contains($0.id) }
    }

    /// The streams a task can be moved into, by display name.
    ///
    /// Sorted here rather than in the picker: two pickers sorting it themselves
    /// is two places for the order to differ. Localised comparison, so a vault
    /// with an "Église" stream files it where the reader expects.
    var streamChoices: [StreamChoice] {
        names.streams
            .map { StreamChoice(id: $0.key, name: $0.value) }
            .sorted { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
    }

    // MARK: - Mutations

    func complete(_ task: TaskItem) async {
        await run(.completeTask(id: task.id))
    }

    /// Undo a completion. A mis-click on a checkbox must not be irreversible,
    /// and `Command::CompleteTask` has no inverse of its own.
    func reopen(_ task: TaskItem) async {
        var edit = TaskEdit()
        edit.state = .todo
        await run(.updateTask(id: task.id, edit: edit))
    }

    func delete(_ task: TaskItem) async {
        await run(.deleteTask(id: task.id))
    }

    /// Push a task out by whole days, keeping its time of day.
    func defer_(_ task: TaskItem, byDays days: Int) async {
        let now = await bridge.nowMs()
        let target = now &+ UInt64(max(1, days)) &* 24 &* 60 &* 60 &* 1000
        await run(.deferTask(id: task.id, toMs: target))
    }

    func apply(_ edit: TaskEdit, to task: TaskItem) async {
        await run(.updateTask(id: task.id, edit: edit))
    }

    /// Give a task a scheduled time. Distinct from ``defer_(_:byDays:)``:
    /// deferring bumps the deferral count, because being pushed out four times
    /// is a fact about a task worth surfacing, and picking a date for the first
    /// time is not a deferral.
    func schedule(_ task: TaskItem, at date: Date) async {
        var edit = TaskEdit()
        edit.setScheduledAt = .instant(at: Int64(date.timeIntervalSince1970 * 1000))
        await run(.updateTask(id: task.id, edit: edit))
    }

    func move(_ task: TaskItem, toStream stream: EntityRef) async {
        await run(.promoteToStream(id: task.id, stream: stream))
    }

    /// Bind a task to a time block, which is what a drop onto the grid does
    /// from the other direction.
    func bind(_ task: EntityRef, to block: EntityRef) async {
        await run(.bindTask(block: block, task: task))
    }

    /// **Drag-to-reorder**: put `moved` immediately before `target`.
    ///
    /// `false` when this list cannot be arranged by hand — Today is sectioned
    /// by urgency and Search is ranked by relevance, and in both a dropped row
    /// would snap back on the next refresh. Returning the refusal rather than
    /// silently doing nothing is what lets the row decline the drop, so the
    /// cursor shows "no" instead of promising a move that will not happen.
    @discardableResult
    func reorder(_ moved: [EntityRef], before target: EntityRef) -> Bool {
        guard kind.acceptsReordering else { return false }
        let changed = order.move(moved, before: target, in: kind, visible: tasks.map(\.id))
        guard changed else { return false }
        // Re-read from the store rather than patching `tasks` here, so the one
        // answer to "what order is this list in" stays `ListOrderStore`'s.
        tasks = order.apply(tasks, for: kind)
        return true
    }

    /// Whether a row in this list can be dragged onto another to move it.
    var acceptsReordering: Bool { kind.acceptsReordering }

    /// Start a focus session on a task, from the list rather than from the
    /// planner. One pomodoro, and the task's own energy facet — the two
    /// defaults `FocusView` starts with, so `F` on a row and picking that row
    /// in Focus produce the same session.
    func startFocus(_ task: TaskItem) async {
        await run(.startFocus(
            taskId: task.id,
            kind: .work,
            length: .onePomodoro,
            energy: nil
        ))
    }

    // MARK: - The same, over a selection

    /// Run a mutation over several rows.
    ///
    /// Sequential, and re-querying only once at the end. Twenty completions
    /// that each refreshed would be twenty full re-reads of a list that is
    /// changing under them — and nineteen of the answers would be thrown away.
    func complete(_ tasks: [TaskItem]) async {
        await runAll(tasks.map { .completeTask(id: $0.id) })
    }

    func delete(_ tasks: [TaskItem]) async {
        await runAll(tasks.map { .deleteTask(id: $0.id) })
    }

    func defer_(_ tasks: [TaskItem], byDays days: Int) async {
        let now = await bridge.nowMs()
        let target = now &+ UInt64(max(1, days)) &* 24 &* 60 &* 60 &* 1000
        await runAll(tasks.map { .deferTask(id: $0.id, toMs: target) })
    }

    func schedule(_ tasks: [TaskItem], at date: Date) async {
        var edit = TaskEdit()
        edit.setScheduledAt = .instant(at: Int64(date.timeIntervalSince1970 * 1000))
        await runAll(tasks.map { .updateTask(id: $0.id, edit: edit) })
    }

    func move(_ tasks: [TaskItem], toStream stream: EntityRef) async {
        await runAll(tasks.map { .promoteToStream(id: $0.id, stream: stream) })
    }

    /// Commit a parsed capture. The draft comes from `previewCapture`, so what
    /// is written is exactly what the preview showed.
    ///
    /// Captured into a stream list, an untagged line lands in *that* stream.
    /// An explicit `#stream` in the line still wins: the user said where it
    /// goes, and the list they happened to be looking at does not override
    /// what they typed.
    ///
    /// Today gets the same treatment for the field *it* filters on. A draft
    /// carries no stream, so it is written to the Inbox; `Query::Today` returns
    /// only tasks that carry a date. Without a date supplied here, a line typed
    /// into the bar at the top of Today was written correctly and then vanished
    /// off the screen it was typed on — no row, no error, nothing to say where
    /// it had gone. A list that offers capture has to show what it captured.
    func create(_ draft: TaskDraftIn) async {
        var draft = draft
        if case let .stream(id, _) = kind, draft.streamId == nil {
            draft.streamId = id
        }
        // The clock is read here rather than taken from `nowMs`, which is only
        // refreshed by a query: in a session left open overnight the cached
        // value would date this capture into yesterday and file it as overdue.
        if case .today = kind, draft.scheduledAt == nil, draft.dueAt == nil {
            draft.scheduledAt = .instant(at: Int64(await bridge.nowMs()))
        }
        await run(.createTask(draft: draft))
    }

    private func run(_ command: CoreCommand) async {
        await runAll([command])
    }

    /// Submit every command, then re-read once.
    ///
    /// The first failure stops the batch and is reported. Carrying on would
    /// leave a selection half-applied with nothing on screen to say which half
    /// — and the core takes each command in its own transaction, so what
    /// already landed has landed either way.
    private func runAll(_ commands: [CoreCommand]) async {
        guard !commands.isEmpty else { return }
        var failure: String?
        for command in commands {
            do {
                _ = try await bridge.submit(command)
            } catch {
                failure = error.localizedDescription
                break
            }
        }
        // The change stream will also fire; refreshing here means the row
        // updates on the click rather than one broadcast hop later. It runs
        // after a failure too — the commands before the failing one landed —
        // and the message is set afterwards, because a successful re-read
        // clears it.
        await refresh()
        if let failure { errorMessage = failure }
    }
}
