import SwiftUI

/// One block on the grid, or one event of a repeating block.
struct BlockChip: View {
    let placed: PlacedBlock
    let width: CGFloat
    let dayStartMs: Int64
    let conflicted: Bool
    let open: () -> Void
    let delete: () -> Void
    let resolve: [ResolveOption]
    /// Where a drag on this chip ended up, in points down the column.
    let drag: (BlockDragMode, CGFloat) -> Void

    /// The live offset while a drag is in flight, so the chip follows the
    /// pointer instead of jumping when the write lands.
    @State private var offset: CGFloat = 0
    @State private var stretch: CGFloat = 0

    var body: some View {
        let top = CalendarMetrics.offset(ms: placed.startMs, dayStartMs: dayStartMs)
        let bottom = CalendarMetrics.offset(ms: placed.endMs, dayStartMs: dayStartMs)
        let laneWidth = max(24, width / CGFloat(placed.laneCount))

        VStack(alignment: .leading, spacing: 2) {
            HStack(alignment: .firstTextBaseline, spacing: 3) {
                if placed.row.isFixed {
                    Image(systemName: "pin.fill")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
                if placed.row.recurs {
                    Image(systemName: "repeat")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
                Text(placed.row.title ?? L10n.Calendar.untitledBlock)
                    .font(.caption.weight(.medium))
                    .lineLimit(2)
            }
            if !placed.row.taskTitles.isEmpty {
                Text(placed.row.taskTitles.joined(separator: ", "))
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 0)
        }
        .padding(4)
        .frame(
            width: laneWidth - 2,
            height: max(14, bottom - top + stretch),
            alignment: .topLeading
        )
        // A fixed block is solid, a flexible one dashed: the planner moves the
        // dashed ones and never the solid ones (time-blocks.md §Kinds).
        .background(
            RoundedRectangle(cornerRadius: 4)
                .fill(Color.accentColor.opacity(placed.row.isFixed ? 0.3 : 0.14))
                .stroke(
                    conflicted ? Color.orange : Color.accentColor,
                    style: StrokeStyle(
                        lineWidth: conflicted ? 2 : 1,
                        dash: placed.row.isFixed ? [] : [4, 2]
                    )
                )
        )
        // The bottom few points resize instead of moving — the same edge every
        // calendar on this platform puts a resize on.
        .overlay(alignment: .bottom) { resizeHandle }
        .offset(x: laneWidth * CGFloat(placed.lane) + 1, y: top + offset)
        .opacity(offset == 0 && stretch == 0 ? 1 : DropHighlight.ghostOpacity)
        .onTapGesture(perform: open)
        .gesture(moveGesture)
        .accessibilityIdentifier("calendar-block")
        .accessibilityValue(kindDescription)
        .contextMenu {
            Button(L10n.Calendar.editEllipsis, action: open)
            if !resolve.isEmpty {
                Menu(L10n.Calendar.resolve) {
                    ForEach(resolve) { option in
                        Section(L10n.Calendar.overlaps(title: option.otherTitle)) {
                            Button(L10n.Calendar.keepBoth, action: option.keepBoth)
                            Button(L10n.Calendar.merge, action: option.merge)
                            Button(L10n.Calendar.adjustTimes, action: option.adjust)
                        }
                    }
                }
            }
            Divider()
            Button(L10n.Action.delete, role: .destructive, action: delete)
        }
    }

    /// What the solid or dashed edge and the glyphs say, for a reader that
    /// cannot see them.
    private var kindDescription: String {
        let kind = placed.row.isFixed ? L10n.Calendar.blockFixed : L10n.Calendar.blockFlexible
        guard placed.row.recurs else { return kind }
        return ListFormatter.localizedString(byJoining: [kind, L10n.Calendar.blockRepeats])
    }

    /// Drag the body: the block keeps its length and changes when it is.
    private var moveGesture: some Gesture {
        DragGesture(minimumDistance: 4)
            .onChanged { offset = $0.translation.height }
            .onEnded { value in
                offset = 0
                drag(.move, value.translation.height)
            }
    }

    /// Drag the bottom edge: the block keeps its start and changes how long it
    /// is. A strip rather than a corner grip, because a block can be eight
    /// points tall and a grip would not fit on one.
    private var resizeHandle: some View {
        Color.clear
            .frame(height: BlockChip.resizeGrip)
            .contentShape(.rect)
            .gesture(
                DragGesture(minimumDistance: 2)
                    .onChanged { stretch = $0.translation.height }
                    .onEnded { value in
                        stretch = 0
                        drag(.resizeEnd, value.translation.height)
                    }
            )
    }

    /// How tall the resize strip is.
    private static let resizeGrip: CGFloat = 6
}

extension View {
    /// "Which events?", asked before a change to one event of a repeating
    /// block: *this event*, *this and following events* or *all events*
    /// (`docs/02-domain/time-blocks.md` §Lifecycle). The answer runs the held
    /// change; Cancel, or dismissing the question, drops it.
    func askingBlockReach(_ pending: Binding<PendingBlockReach?>) -> some View {
        confirmationDialog(
            pending.wrappedValue?.action.title ?? "",
            isPresented: Binding(
                get: { pending.wrappedValue != nil },
                set: { if !$0 { pending.wrappedValue = nil } }
            ),
            titleVisibility: .visible,
            presenting: pending.wrappedValue
        ) { request in
            ForEach(BlockEditReach.allCases) { reach in
                Button(reach.title, role: request.action == .delete ? .destructive : nil) {
                    request.commit(reach)
                }
                .accessibilityIdentifier("block-reach.\(reach.rawValue)")
            }
            Button(L10n.Action.cancel, role: .cancel) {}
                .accessibilityIdentifier("block-reach.cancel")
        } message: { _ in
            Text(L10n.Calendar.reachMessage)
        }
    }
}
