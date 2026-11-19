import Foundation

/// The triage queue (ADR-0047 §3–§4): the late and stale tasks waiting on a
/// decision, and the four actions that take one.
///
/// The core decides everything that matters here — which tasks are in the
/// queue, in what order, and why each one is — and this model renders its
/// answer: `Query::Triage` read whole, the user's selection over it, and one
/// `Command::Triage` per action however many tasks it names, so a bulk Drop
/// is one write and one Undo step, as ADR-0047 §4 asks.
///
/// Nothing is automatic. A task leaves the queue because an action was taken
/// on it, or because the core's next read no longer finds it late — never
/// because this model decided so.
@MainActor
@Observable
final class TriageModel {
    /// The queue, in the core's order: hard deadlines missed first, then soft
    /// ones, then stale tasks, oldest first within each.
    private(set) var rows: [TriageRowItem] = []
    /// The ticked rows. Narrowed to the queue on every read, so an action
    /// never names a task that has already left it — the core would refuse
    /// the whole command for one deleted id.
    var selection: Set<EntityRef> = []
    /// The core's clock at the last read: the latest moment Already done may
    /// name, since the core refuses a completion in the future.
    private(set) var nowMs: UInt64 = 0
    private(set) var errorMessage: String?

    private let bridge: CoreBridge

    init(bridge: CoreBridge) {
        self.bridge = bridge
    }

    /// The selected tasks, in queue order, which is the order the command
    /// names them in.
    var selected: [EntityRef] {
        rows.map(\.task.id).filter(selection.contains)
    }

    /// ``nowMs`` as a date, for the Already done picker's upper bound.
    var now: Date {
        Date(timeIntervalSince1970: Double(nowMs) / 1000)
    }

    func refresh() async {
        nowMs = await bridge.nowMs()
        do {
            guard case let .triage(rows) = try await bridge.query(.triage) else { return }
            self.rows = rows
            selection.formIntersection(rows.map(\.task.id))
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// Re-read on every change, so a task completed on another device or
    /// from a notification leaves the queue while it is on screen.
    func follow() async {
        for await batch in await bridge.changes() {
            guard !batch.isClosed else { return }
            await refresh()
        }
    }

    func selectAll() {
        selection = Set(rows.map(\.task.id))
    }

    /// Defer: the core moves each plan to the next free slot and counts the
    /// deferral.
    func deferToNextFreeSlot(_ ids: [EntityRef]) async {
        await run(.deferToNextFreeSlot, on: ids, label: L10n.Triage.deferAction)
    }

    /// Already done, at `date`. An instant rather than a day, because the
    /// picker names a time; the core refuses one in the future or before the
    /// task was created, and that refusal is shown rather than guessed at.
    func markDone(_ ids: [EntityRef], at date: Date) async {
        let ms = Int64((date.timeIntervalSince1970 * 1000).rounded())
        await run(.alreadyDone(completedAt: .instant(at: ms)), on: ids, label: L10n.Triage.doneConfirm)
    }

    /// Drop: cancelled, with the reason when one was typed. The core trims it
    /// and treats a blank one as none.
    func drop(_ ids: [EntityRef], reason: String) async {
        await run(.drop(reason: reason.nilIfBlank), on: ids, label: L10n.Triage.dropConfirm)
    }

    /// Keep: left as it is until its lateness next changes, when it returns.
    func keep(_ ids: [EntityRef]) async {
        await run(.keep, on: ids, label: L10n.Triage.keep)
    }

    func dismissError() { errorMessage = nil }

    private func run(_ action: TriageActionIn, on ids: [EntityRef], label: String) async {
        guard !ids.isEmpty else { return }
        do {
            _ = try await bridge.submitUndoable(.triage(ids: ids, action: action), label: label)
            selection.subtract(ids)
            await refresh()
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}
