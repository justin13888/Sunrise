import Foundation

/// One list of tasks, kept current.
///
/// Every mutation goes to the core and comes back through the change stream;
/// nothing is patched locally on the way. That is slower by one round trip and
/// it is the reason a completion made on another device and a completion made
/// here look identical on screen.
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

    private let bridge: CoreBridge

    init(bridge: CoreBridge, kind: TaskListKind = .today) {
        self.bridge = bridge
        self.kind = kind
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
            tasks = rows
            groups = kind == .today
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

    /// Commit a parsed capture. The draft comes from `previewCapture`, so what
    /// is written is exactly what the preview showed.
    ///
    /// Captured into a stream list, an untagged line lands in *that* stream.
    /// An explicit `#stream` in the line still wins: the user said where it
    /// goes, and the list they happened to be looking at does not override
    /// what they typed.
    func create(_ draft: TaskDraftIn) async {
        var draft = draft
        if case let .stream(id, _) = kind, draft.streamId == nil {
            draft.streamId = id
        }
        await run(.createTask(draft: draft))
    }

    private func run(_ command: CoreCommand) async {
        do {
            _ = try await bridge.submit(command)
            // The change stream will also fire; refreshing here means the row
            // updates on the click rather than one broadcast hop later.
            await refresh()
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}
