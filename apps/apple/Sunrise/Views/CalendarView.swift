import SwiftUI
import UniformTypeIdentifiers

/// The calendar grid: a day or a week of time blocks.
struct CalendarView: View {
    @Bindable var model: CalendarModel

    @State private var draft: BlockDraftSheet?
    @State private var editing: BlockGridRow?
    @State private var adjusting: BlockConflict?

    var body: some View {
        VStack(spacing: 0) {
            toolbar
            if let note = model.note {
                NoteBanner(text: note) { model.dismissNote() }
            }
            if let error = model.errorMessage {
                NoteBanner(text: error) { model.dismissError() }
            }
            Divider()
            grid
        }
        .task { await model.refresh() }
        .task { await model.follow() }
        .sheet(item: $draft) { pending in
            BlockDraftSheetView(model: model, draft: pending)
        }
        .sheet(item: $editing) { row in
            BlockEditorView(model: model, row: row)
        }
        .sheet(item: $adjusting) { conflict in
            AdjustBlocksView(model: model, conflict: conflict)
        }
    }

    /// Span, the date stepper and the snap size.
    ///
    /// Two layouts, chosen by whether the first one fits. Everything in the
    /// wide row has a real intrinsic width — two pickers, three controls and a
    /// date that runs to "Monday 8 September" — and their sum is well over an
    /// iPhone's 402 points, so on a phone this row overflowed: the stepper ran
    /// off the trailing edge and the date between the chevrons was squeezed
    /// into a column of single characters.
    ///
    /// `ViewThatFits` rather than a size class or a width test, because the
    /// question really is "does this row fit", and the answer depends on the
    /// date being drawn as much as on the device. A Mac window and an iPad
    /// take the wide row; an iPhone takes the stacked one; a Mac window
    /// dragged narrow takes the stacked one too, which is the right answer
    /// there for the same reason.
    private var toolbar: some View {
        ViewThatFits(in: .horizontal) {
            wideToolbar
            stackedToolbar
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .accessibilityIdentifier("calendar-toolbar")
    }

    private var wideToolbar: some View {
        HStack(spacing: 12) {
            spanPicker.frame(width: 140)
            stepper
            dateLabel
            Spacer()
            snapPicker.frame(width: 130)
        }
    }

    private var stackedToolbar: some View {
        VStack(spacing: 8) {
            HStack(spacing: 12) {
                spanPicker
                snapPicker
                    .labelsHidden()
                    .fixedSize()
            }
            HStack(spacing: 12) {
                stepper
                dateLabel
                Spacer(minLength: 0)
            }
        }
    }

    private var spanPicker: some View {
        Picker("Span", selection: $model.span) {
            ForEach(CalendarSpan.allCases) { Text($0.title).tag($0) }
        }
        .pickerStyle(.segmented)
        .labelsHidden()
    }

    private var snapPicker: some View {
        Picker("Snap", selection: $model.snapMinutes) {
            ForEach(CalendarModel.snapChoices, id: \.self) { Text("\($0) min").tag($0) }
        }
    }

    @ViewBuilder
    private var stepper: some View {
        Button("Previous", systemImage: "chevron.left") {
            Task { await model.step(-1) }
        }
        .labelStyle(.iconOnly)
        Button("Today") { Task { await model.goToToday() } }
        Button("Next", systemImage: "chevron.right") {
            Task { await model.step(1) }
        }
        .labelStyle(.iconOnly)
    }

    /// The date, on one line whatever happens.
    ///
    /// `lineLimit(1)` is the half that matters: without it a `Text` given less
    /// width than one word wraps per character, which is what turned this into
    /// a vertical column of letters on an iPhone rather than merely truncating
    /// it.
    private var dateLabel: some View {
        Text(rangeTitle)
            .font(.headline)
            .lineLimit(1)
            .minimumScaleFactor(0.8)
    }

    private var rangeTitle: String {
        let formatter = DateFormatter()
        formatter.timeZone = TimeZone(identifier: model.timeZone)
        formatter.dateFormat = model.span == .day ? "EEEE d MMMM" : "d MMM"
        let start = Date(timeIntervalSince1970: Double(model.windowStartMs) / 1000)
        guard model.span == .week else { return formatter.string(from: start) }
        let end = Date(timeIntervalSince1970: Double(model.dayStartMs(offset: 6)) / 1000)
        return "\(formatter.string(from: start)) – \(formatter.string(from: end))"
    }

    private var grid: some View {
        ScrollView {
            HStack(alignment: .top, spacing: 0) {
                HourGutter(timeZone: model.timeZone)
                ForEach(0..<model.span.dayCount, id: \.self) { offset in
                    DayColumn(
                        model: model,
                        dayOffset: offset,
                        showsHeader: model.span == .week,
                        beginDraft: { from, to in
                            draft = BlockDraftSheet(fromMs: from, toMs: to)
                        },
                        edit: { editing = $0 },
                        adjust: { adjusting = $0 }
                    )
                }
            }
            .padding(.bottom, 24)
        }
    }
}

/// A drag on the grid that has not been committed yet.
///
/// A struct rather than two `@State` numbers so the sheet is `item:`-driven:
/// a `isPresented:` sheet reading two separate pieces of state can be shown
/// before both have been set, which is how a "new block" sheet ends up
/// proposing midnight to midnight.
struct BlockDraftSheet: Identifiable {
    let fromMs: Int64
    let toMs: Int64
    var id: String { "\(fromMs)-\(toMs)" }
}

/// Times down the left-hand side.
private struct HourGutter: View {
    let timeZone: String

    var body: some View {
        VStack(alignment: .trailing, spacing: 0) {
            Color.clear.frame(height: CalendarMetrics.headerHeight)
            ForEach(0..<24, id: \.self) { hour in
                Text(label(hour))
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .frame(height: CalendarMetrics.hourHeight, alignment: .top)
            }
        }
        .frame(width: 52)
        .padding(.trailing, 4)
    }

    private func label(_ hour: Int) -> String {
        let formatter = DateFormatter()
        formatter.timeZone = TimeZone(identifier: timeZone)
        formatter.dateFormat = "ha"
        let midnight = Calendar(identifier: .gregorian).startOfDay(for: Date())
        let at = midnight.addingTimeInterval(Double(hour) * 3600)
        return formatter.string(from: at).lowercased()
    }
}

/// Fixed geometry, in one place so the gutter and the columns cannot disagree.
enum CalendarMetrics {
    static let hourHeight: CGFloat = 44
    static let headerHeight: CGFloat = 24
    static var dayHeight: CGFloat { hourHeight * 24 }

    /// Where an instant sits in a day column.
    static func offset(ms: Int64, dayStartMs: Int64) -> CGFloat {
        CGFloat(ms - dayStartMs) / 3_600_000 * hourHeight
    }

    /// The instant at a vertical position.
    static func instant(at y: CGFloat, dayStartMs: Int64) -> Int64 {
        dayStartMs + Int64(max(0, min(dayHeight, y)) / hourHeight * 3_600_000)
    }
}

/// One day's column of the grid.
private struct DayColumn: View {
    let model: CalendarModel
    let dayOffset: Int
    let showsHeader: Bool
    let beginDraft: (Int64, Int64) -> Void
    let edit: (BlockGridRow) -> Void
    let adjust: (BlockConflict) -> Void

    @State private var dragFrom: CGFloat?
    @State private var dragTo: CGFloat?

    private var dayStartMs: Int64 { model.dayStartMs(offset: dayOffset) }

    var body: some View {
        VStack(spacing: 0) {
            if showsHeader {
                Text(header)
                    .font(.caption)
                    .frame(height: CalendarMetrics.headerHeight)
            } else {
                Color.clear.frame(height: CalendarMetrics.headerHeight)
            }
            canvas
        }
        .frame(maxWidth: .infinity)
    }

    private var header: String {
        let formatter = DateFormatter()
        formatter.timeZone = TimeZone(identifier: model.timeZone)
        formatter.dateFormat = "EEE d"
        return formatter.string(from: Date(timeIntervalSince1970: Double(dayStartMs) / 1000))
    }

    private var canvas: some View {
        GeometryReader { proxy in
            ZStack(alignment: .topLeading) {
                hourLines
                shading
                ForEach(model.placed(dayOffset: dayOffset)) { placed in
                    BlockChip(
                        placed: placed,
                        width: proxy.size.width,
                        dayStartMs: dayStartMs,
                        conflicted: isConflicted(placed),
                        open: { edit(placed.row) },
                        delete: { Task { await model.delete(placed.row.block.id) } },
                        resolve: resolveMenu(for: placed),
                        drag: { mode, points in dragBlock(placed, mode: mode, byPoints: points) }
                    )
                }
                if let ghost = ghostRect {
                    RoundedRectangle(cornerRadius: 4)
                        .fill(Color.accentColor.opacity(0.65))
                        .frame(height: max(4, ghost.height))
                        .offset(y: ghost.minY)
                        .allowsHitTesting(false)
                }
            }
            .frame(height: CalendarMetrics.dayHeight)
            .contentShape(Rectangle())
            .gesture(dragToCreate)
            .dropDestination(for: String.self) { items, location in
                accept(items: items, at: location.y)
            }
        }
        .frame(height: CalendarMetrics.dayHeight)
        .border(.quaternary, width: 0.5)
    }

    private var hourLines: some View {
        VStack(spacing: 0) {
            ForEach(0..<24, id: \.self) { _ in
                Divider()
                Spacer(minLength: 0).frame(height: CalendarMetrics.hourHeight - 1)
            }
        }
    }

    /// The overlap regions, shaded. `docs/02-domain/time-blocks.md` §Conflicts
    /// asks for exactly this and for the Resolve menu on top of it.
    private var shading: some View {
        ForEach(model.shading(dayOffset: dayOffset)) { placed in
            let top = CalendarMetrics.offset(ms: placed.conflict.fromMs, dayStartMs: dayStartMs)
            let bottom = CalendarMetrics.offset(ms: placed.conflict.toMs, dayStartMs: dayStartMs)
            Rectangle()
                .fill(Color.orange.opacity(0.22))
                .frame(height: max(2, bottom - top))
                .offset(y: top)
                .allowsHitTesting(false)
                .accessibilityIdentifier("calendar-conflict")
        }
    }

    private var ghostRect: (minY: CGFloat, height: CGFloat)? {
        guard let from = dragFrom, let to = dragTo else { return nil }
        return (min(from, to), abs(to - from))
    }

    private var dragToCreate: some Gesture {
        DragGesture(minimumDistance: 4)
            .onChanged { value in
                if dragFrom == nil { dragFrom = value.startLocation.y }
                dragTo = value.location.y
            }
            .onEnded { value in
                defer {
                    dragFrom = nil
                    dragTo = nil
                }
                let start = model.snap(
                    CalendarMetrics.instant(at: value.startLocation.y, dayStartMs: dayStartMs),
                    dayOffset: dayOffset
                )
                let end = model.snap(
                    CalendarMetrics.instant(at: value.location.y, dayStartMs: dayStartMs),
                    dayOffset: dayOffset
                )
                let from = min(start, end)
                // A block must end after it starts; a flick that snapped to one
                // slot becomes one slot long rather than being rejected.
                let to = max(end, from + Int64(model.snapMinutes) * 60_000)
                beginDraft(from, to)
            }
    }

    /// **Moving or resizing a block by dragging it.**
    ///
    /// The translation is turned into milliseconds here — points are the
    /// view's unit and the grid's scale is the view's business — and every
    /// decision after that is `BlockDrag`'s, so what happens at midnight and
    /// what happens to a block dragged shorter than a snap step are both
    /// testable without a window.
    private func dragBlock(_ placed: PlacedBlock, mode: BlockDragMode, byPoints points: CGFloat) {
        let deltaMs = Int64(points / CalendarMetrics.hourHeight * 3_600_000)
        let bounds = BlockDrag.apply(
            mode: mode,
            startMs: placed.startMs,
            endMs: placed.endMs,
            deltaMs: deltaMs,
            snapMinutes: model.snapMinutes,
            dayStartMs: dayStartMs,
            dayEndMs: dayStartMs + 24 * 3_600_000
        )
        guard bounds.startMs != placed.startMs || bounds.endMs != placed.endMs else { return }
        Task { await model.moveBlock(placed.row, toStartMs: bounds.startMs, toEndMs: bounds.endMs) }
    }

    /// A task dragged from a list onto the grid.
    ///
    /// The payload is the task's `EntityRef`, which is its text form — the same
    /// string the CLI accepts. A drop of anything else is ignored rather than
    /// guessed at.
    private func accept(items: [String], at y: CGFloat) -> Bool {
        guard let raw = items.first, raw.hasPrefix("tsk_") else { return false }
        let start = model.snap(
            CalendarMetrics.instant(at: y, dayStartMs: dayStartMs),
            dayOffset: dayOffset
        )
        let end = start + Int64(max(model.snapMinutes, 30)) * 60_000
        Task { await model.dropTask(raw, fromMs: start, toMs: end, kind: .zoned) }
        return true
    }

    private func isConflicted(_ placed: PlacedBlock) -> Bool {
        model.conflicts.contains { $0.a == placed.id || $0.b == placed.id }
    }

    /// The conflicts this block is part of, for its Resolve menu.
    private func resolveMenu(for placed: PlacedBlock) -> [ResolveOption] {
        model.conflicts
            .filter { $0.a == placed.id || $0.b == placed.id }
            .compactMap { conflict in
                let otherId = conflict.a == placed.id ? conflict.b : conflict.a
                guard let other = model.row(otherId) else { return nil }
                return ResolveOption(
                    conflict: conflict,
                    otherTitle: other.title ?? "Untitled block",
                    keepBoth: { model.keepBoth() },
                    merge: { Task { await model.merge(conflict) } },
                    adjust: { adjust(conflict) }
                )
            }
    }
}

/// One entry of a block's Resolve menu: the other block, and the three
/// documented actions against it.
struct ResolveOption: Identifiable {
    let conflict: BlockConflict
    let otherTitle: String
    let keepBoth: () -> Void
    let merge: () -> Void
    let adjust: () -> Void

    var id: String { "\(conflict.a)|\(conflict.b)" }
}

/// One block on the grid.
private struct BlockChip: View {
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
            Text(placed.row.title ?? "Untitled block")
                .font(.caption.weight(.medium))
                .lineLimit(2)
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
        .background(
            RoundedRectangle(cornerRadius: 4)
                .fill(Color.accentColor.opacity(0.18))
                .stroke(conflicted ? Color.orange : Color.accentColor, lineWidth: conflicted ? 2 : 1)
        )
        // The bottom few points resize instead of moving — the same edge every
        // calendar on this platform puts a resize on.
        .overlay(alignment: .bottom) { resizeHandle }
        .offset(x: laneWidth * CGFloat(placed.lane) + 1, y: top + offset)
        .opacity(offset == 0 && stretch == 0 ? 1 : DropHighlight.ghostOpacity)
        .onTapGesture(perform: open)
        .gesture(moveGesture)
        .accessibilityIdentifier("calendar-block")
        .contextMenu {
            Button("Edit…", action: open)
            if !resolve.isEmpty {
                Menu("Resolve") {
                    ForEach(resolve) { option in
                        Section("Overlaps \(option.otherTitle)") {
                            Button("Keep both", action: option.keepBoth)
                            Button("Merge", action: option.merge)
                            Button("Adjust times…", action: option.adjust)
                        }
                    }
                }
            }
            Divider()
            Button("Delete", role: .destructive, action: delete)
        }
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
