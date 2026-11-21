import SwiftUI
import UniformTypeIdentifiers

/// One timed task on the grid, in the column's task track (``TaskTrack``).
///
/// A circle, as a task row draws it, and a plain fill rather than a block's
/// accent, so a task never reads as a block. **Dragging it along the column
/// plans its move**: the snapped instant its start is over is a `MoveTask`
/// gesture, previewed on each new position and committed as drawn on
/// release, as a block drag is (``CalendarPlanner``).
struct TaskChip: View {
    let placed: PlacedTask
    let model: CalendarModel
    let dayOffset: Int
    let track: TaskTrack

    /// The live offset while a drag is in flight, so the chip follows the
    /// pointer instead of jumping when the write lands.
    @State private var offset: CGFloat = 0

    var body: some View {
        let dayStartMs = model.dayStartMs(offset: dayOffset)
        let top = CalendarMetrics.offset(ms: placed.startMs, dayStartMs: dayStartMs)
        let bottom = CalendarMetrics.offset(ms: placed.endMs, dayStartMs: dayStartMs)
        let laneWidth = max(24, track.taskWidth / CGFloat(placed.laneCount))
        // The plan on screen moves this task: its ghost is drawn where it
        // goes, so the chip fades to show where it was.
        let isMoving = model.planner.moves(placed.id)

        HStack(alignment: .firstTextBaseline, spacing: 3) {
            Image(systemName: "circle")
                .font(.caption2)
                .foregroundStyle(.secondary)
                .accessibilityHidden(true)
            Text(placed.task.title)
                .font(.caption.weight(.medium))
                .lineLimit(2)
            Spacer(minLength: 0)
        }
        .padding(4)
        .frame(width: laneWidth - 2, height: max(14, bottom - top), alignment: .topLeading)
        .background(
            RoundedRectangle(cornerRadius: 4)
                .fill(Color.secondary.opacity(0.14))
                .stroke(Color.secondary.opacity(0.45), lineWidth: 1)
        )
        .offset(x: track.taskX + laneWidth * CGFloat(placed.lane) + 1, y: top + offset)
        .opacity(offset == 0 && !isMoving ? 1 : DropHighlight.ghostOpacity)
        .gesture(
            DragGesture(minimumDistance: 4)
                .onChanged { value in
                    offset = value.translation.height
                    drag(byPoints: value.translation.height, ended: false)
                }
                .onEnded { value in
                    offset = 0
                    drag(byPoints: value.translation.height, ended: true)
                }
        )
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("calendar-task")
    }

    private func drag(byPoints points: CGFloat, ended: Bool) {
        let zone = model.timeZone
        guard let start = BlockLayout.timedStartMs(of: placed.task, timeZone: zone) else { return }
        let deltaMs = Int64(points / CalendarMetrics.hourHeight * 3_600_000)
        let toMs = model.snap(start + deltaMs, dayOffset: dayOffset)
        let (planner, task) = (model.planner, placed.task.id)
        guard ended else {
            Task { await planner.previewTask(task, toMs: toMs, timeZone: zone) }
            return
        }
        Task { await planner.dropTask(task, atMs: toMs, timeZone: zone) }
    }
}

/// **Task → Calendar**: a day column as the drop target for a task dragged
/// from a list.
///
/// While it hovers, each snapped position is previewed as a `MoveTask`, so
/// the ghosts show where it lands and what moves out of its way before it is
/// let go; leaving the column cancels that plan. The drop commits the plan
/// for where it was let go (``CalendarPlanner/dropTask(_:atMs:timeZone:)``).
///
/// A delegate rather than `dropDestination`, because the hover needs the
/// pointer's position and `dropDestination` reports only whether it is over
/// the column. The hover previews the task the row lent when the drag
/// started (``TaskDragLedger``); the drop reads the payload itself, the
/// task's `EntityRef` in its text form, and plans nothing else.
struct GridTaskDropTarget: ViewModifier {
    let model: CalendarModel
    let dayOffset: Int

    func body(content: Content) -> some View {
        let (model, dayOffset) = (model, dayOffset)
        content.onDrop(
            of: [.text],
            delegate: Delegate(planner: model.planner, timeZone: model.timeZone) { y in
                let instant = CalendarMetrics.instant(at: y, dayStartMs: model.dayStartMs(offset: dayOffset))
                return model.snap(instant, dayOffset: dayOffset)
            }
        )
    }

    private struct Delegate: DropDelegate {
        let planner: CalendarPlanner
        let timeZone: String
        /// The snapped instant a point down the column stands for.
        let instant: (CGFloat) -> Int64

        func validateDrop(info: DropInfo) -> Bool {
            info.hasItemsConforming(to: [.text])
        }

        /// A drag this app started with something other than a task in it — a
        /// block's grip — is refused, as `dropDestination` refused it.
        func dropUpdated(info: DropInfo) -> DropProposal? {
            guard let task = TaskDragLedger.task else {
                return DropProposal(operation: TaskDragLedger.lent == nil ? .copy : .forbidden)
            }
            let (planner, zone, at) = (planner, timeZone, instant(info.location.y))
            Task { await planner.previewTask(task, toMs: at, timeZone: zone) }
            return DropProposal(operation: .copy)
        }

        func dropExited(info: DropInfo) {
            planner.taskDragLeft()
        }

        func performDrop(info: DropInfo) -> Bool {
            guard let provider = info.itemProviders(for: [.text]).first else { return false }
            let (planner, zone, at) = (planner, timeZone, instant(info.location.y))
            planner.willDropTask()
            _ = provider.loadObject(ofClass: String.self) { text, _ in
                let task = text.flatMap { DropPayload.taskIDs([$0]).first }
                Task { @MainActor in await planner.dropTask(task, atMs: at, timeZone: zone) }
            }
            return true
        }
    }
}
