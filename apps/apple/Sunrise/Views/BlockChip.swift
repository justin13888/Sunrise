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
    /// Whether the plan on screen moves this block: its ghost is drawn where
    /// it goes, so the chip itself fades to show where it was.
    var isMoving = false
    /// Where a drag on this chip has got to, in points down the column, and
    /// whether it has ended: each change previews, the end commits.
    let drag: (BlockDragMode, CGFloat, Bool) -> Void

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
                Spacer(minLength: 0)
                bindGrip
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
        .opacity(offset == 0 && stretch == 0 && !isMoving ? 1 : DropHighlight.ghostOpacity)
        .onTapGesture(perform: open)
        .gesture(moveGesture)
        .accessibilityIdentifier("calendar-block")
        .accessibilityValue(kindDescription)
        .accessibilityHint(planningHint)
        .help(planningHint)
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

    /// The planning keys, where they work: on the Mac, on a block that does
    /// not repeat (`docs/08-features/keyboard.md` Rule 1 — a bound action
    /// shows its key wherever it appears).
    private var planningHint: String {
        #if os(macOS)
        placed.row.recurs ? "" : L10n.Calendar.planKeysHint(keys: Keymap.planningLabel)
        #else
        ""
        #endif
    }

    /// Drag the body: the block keeps its length and changes when it is.
    private var moveGesture: some Gesture {
        DragGesture(minimumDistance: 4)
            .onChanged { value in
                offset = value.translation.height
                drag(.move, value.translation.height, false)
            }
            .onEnded { value in
                offset = 0
                drag(.move, value.translation.height, true)
            }
    }

    /// **Calendar block → Task**, on the Mac: a grip that lifts the block's
    /// id onto the pasteboard for a task row to take. A grip rather than the
    /// chip itself, because dragging the chip moves the block on the grid.
    ///
    /// The Mac only: no iPhone screen shows the grid beside a task list
    /// (`docs/07-clients/interaction-patterns.md` §Drag-and-drop matrix).
    @ViewBuilder
    private var bindGrip: some View {
        #if os(macOS)
        Image(systemName: "link")
            .font(.caption2)
            .foregroundStyle(.secondary)
            .padding(.horizontal, 2)
            .contentShape(.rect)
            .draggable(gripPayload())
            .help(L10n.Calendar.bindGrip)
            .accessibilityLabel(L10n.Calendar.bindGrip)
            .accessibilityIdentifier("calendar-block-bind-grip")
        #endif
    }

    /// The grip's payload, read when its drag starts: lent then too, so the
    /// grid the drag crosses refuses it rather than previewing the last task
    /// dragged (``TaskDragLedger``).
    private func gripPayload() -> EntityRef {
        TaskDragLedger.lend(placed.blockId)
        return placed.blockId
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
                    .onChanged { value in
                        stretch = value.translation.height
                        drag(.resizeEnd, value.translation.height, false)
                    }
                    .onEnded { value in
                        stretch = 0
                        drag(.resizeEnd, value.translation.height, true)
                    }
            )
    }

    /// How tall the resize strip is.
    private static let resizeGrip: CGFloat = 6
}

/// One item of the plan on screen: a ghost where it goes, a faint dashed
/// outline where it was, tinted by the worst rule it would break
/// (`docs/08-features/planner.md` §Client contract). Hard and Blocking take
/// the danger colour, Soft the warning colour, Info an outline badge, and
/// each is paired with a glyph so colour is not the only signal.
struct PlanGhostView: View {
    let ghost: PlanGhost
    let dayStartMs: Int64
    let width: CGFloat

    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        let palette = SunrisePalette.resolved(for: colorScheme)
        let tint = tint(in: palette)
        ZStack(alignment: .topLeading) {
            if let before = ghost.before {
                RoundedRectangle(cornerRadius: SunriseTokens.Radius.sm)
                    .stroke(tint.opacity(0.5), style: StrokeStyle(lineWidth: 1, dash: [3, 3]))
                    .frame(width: width - 2, height: height(of: before))
                    .offset(x: 1, y: top(of: before))
                    .accessibilityHidden(true)
            }
            if let after = ghost.after {
                RoundedRectangle(cornerRadius: SunriseTokens.Radius.sm)
                    .fill(tint.opacity(0.28))
                    .stroke(tint, lineWidth: 2)
                    .overlay(alignment: .topTrailing) { badge(tint) }
                    .frame(width: width - 2, height: height(of: after))
                    .offset(x: 1, y: top(of: after))
                    .accessibilityElement()
                    .accessibilityIdentifier("calendar-ghost")
                    .accessibilityValue(ghost.role == .subject ? "subject" : "rippled")
            }
        }
        .allowsHitTesting(false)
    }

    @ViewBuilder
    private func badge(_ tint: Color) -> some View {
        if let symbol {
            Image(systemName: symbol)
                .font(.caption2)
                .foregroundStyle(tint)
                .padding(2)
        }
    }

    private var symbol: String? {
        switch ghost.severity {
        case .blocking?: "nosign"
        case .hard?: "exclamationmark.octagon"
        case .soft?: "exclamationmark.triangle"
        case .info?: "info.circle"
        case nil: nil
        }
    }

    private func tint(in palette: SunrisePalette) -> Color {
        switch ghost.severity {
        case .blocking?, .hard?: palette.danger
        case .soft?: palette.warning
        case .info?, nil: Color.accentColor
        }
    }

    private func top(of span: GridSpan) -> CGFloat {
        CalendarMetrics.offset(ms: span.startMs, dayStartMs: dayStartMs)
    }

    private func height(of span: GridSpan) -> CGFloat {
        max(4, CalendarMetrics.offset(ms: span.endMs, dayStartMs: dayStartMs) - top(of: span))
    }
}

/// What dropping now would do, in words, for as long as a plan is on screen:
/// "Moves 3 · 1 unscheduled · misses its hard deadline". Spoken to VoiceOver
/// each time it changes.
struct PlanSummaryLine: View {
    let summary: String?

    var body: some View {
        if let summary {
            Text(summary)
                .font(.caption.weight(.medium))
                .padding(.horizontal, 10)
                .padding(.vertical, 4)
                .background(.regularMaterial, in: Capsule())
                .padding(.top, 6)
                .allowsHitTesting(false)
                .accessibilityIdentifier("calendar-plan-summary")
                .onChange(of: summary, initial: true) { _, said in
                    AccessibilityNotification.Announcement(said).post()
                }
        }
    }
}

/// The note after a drop whose plan changed underneath it, with the two
/// answers it waits for: commit the new plan on screen, or cancel it. Buttons
/// rather than a hint at Return and Esc, because a phone has no keys to press
/// and its ghosts would otherwise stay until the next drag.
struct PlanDecisionBanner: View {
    let text: String
    let planner: CalendarPlanner

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "info.circle")
            Text(text).font(.callout)
            Spacer()
            Button(L10n.Action.cancel) { planner.cancel() }
                .accessibilityIdentifier("calendar-plan-cancel")
            Button(L10n.Calendar.planCommitChanged) { Task { await planner.commitPlan() } }
                .buttonStyle(.borderedProminent)
                .accessibilityIdentifier("calendar-plan-commit")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .background(.quaternary.opacity(0.5))
    }
}

/// The planning keys on a block (``Keymap/planning``): the block takes
/// keyboard focus, and a press it holds is handed to `handle`.
///
/// The Mac only, as the keyboard path of a pointer drag. On iOS the chip
/// stays a plain tap target: making it focusable there changes what a tap
/// does on a phone, which has no keys to press.
struct PlanningKeys: ViewModifier {
    var focus: FocusState<String?>.Binding
    let id: String
    let handle: (KeyChord) -> Bool

    func body(content: Content) -> some View {
        #if os(macOS)
        content
            .focusable()
            .focused(focus, equals: id)
            .onKeyPress(phases: .down) { press in
                handle(KeyChord(press)) ? .handled : .ignored
            }
        #else
        content
        #endif
    }
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
