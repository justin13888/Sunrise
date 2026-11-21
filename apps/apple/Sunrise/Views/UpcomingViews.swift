import SwiftUI

/// Upcoming's sections, drawn by the shared task rows.
///
/// Every heading and every row of a section is a drop target for that
/// section: a day takes a task as a reschedule onto it, the no-date lane as
/// clearing its plan. While a drag hovers one, the core's plan for dropping
/// there is previewed and its consequence shown on the heading; the drop
/// commits it (``UpcomingPlanner``).
extension TaskRows {
    @ViewBuilder
    func upcomingSection(_ section: UpcomingSection) -> some View {
        let planner = model.upcomingPlanner
        Section {
            if section.isEmpty, section.date != nil {
                Text(L10n.Upcoming.nothingPlanned)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .upcomingCell()
            }
            ForEach(section.blocks, id: \.gridKey) { block in
                UpcomingBlockLine(row: block, timeZone: model.timeZone)
                    .upcomingCell()
            }
            ForEach(section.rows) { item in
                row(item.task, day: section.target)
                    .upcomingBadge(item.badge)
                    .accessibilityIdentifier("upcoming-row-\(section.id)")
                    .upcomingCell(
                        highlighted: selection.cursor == item.task.id
                            || selection.explicit.contains(item.task.id)
                    )
                    .upcomingIdentity("\(section.id)/\(item.id)")
            }
            if section.more > 0 {
                Text(L10n.Upcoming.more(count: section.more))
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .upcomingCell()
            }
        } header: {
            UpcomingSectionHeader(
                title: heading(of: section),
                total: section.rows.count + section.more,
                summary: planner.target == section.target ? planner.summary : nil,
                identifier: "upcoming-\(section.id)"
            )
            .upcomingHeading()
            .dropDestination(for: String.self) { items, _ in
                dropUpcoming(items, on: section.target)
            } isTargeted: { hoverUpcoming(section.target, $0) }
        }
    }

    private func heading(of section: UpcomingSection) -> String {
        switch section.kind {
        case let .day(date, _):
            model.upcomingHeading(date)
        case let .noDate(stream, noStream):
            "\(L10n.Upcoming.noDate) · "
                + (noStream ? L10n.Upcoming.noStream : model.names.stream(stream) ?? L10n.Upcoming.noStream)
        }
    }

    /// Tasks dropped on an Upcoming section: reschedule them there. Marked
    /// dropping first, so the hover ending with the drop leaves its plan for
    /// the commit.
    func dropUpcoming(_ items: [String], on target: UpcomingTarget) -> Bool {
        let ids = DropPayload.taskIDs(items)
        guard !ids.isEmpty else { return false }
        model.upcomingPlanner.willDrop()
        Task { await model.reschedule(ids, to: target) }
        return true
    }

    func hoverUpcoming(_ target: UpcomingTarget, _ isOver: Bool) {
        Task {
            await model.upcomingPlanner.hover(isOver ? target : nil, leaving: isOver ? nil : target)
        }
    }

    /// What sees a key press before the list's own bindings: Upcoming's
    /// planning keys, and nothing in any other list.
    var keyIntercept: ((KeyChord) -> Bool)? {
        guard model.kind.isUpcoming else { return nil }
        return { chord in upcomingKey(chord) }
    }

    /// The planning keys on the selection: ⌥← and ⌥→ move it a day, Return
    /// commits the move and Esc drops it. Return and Esc mean what they
    /// always mean in a list while no move is on screen.
    func upcomingKey(_ chord: KeyChord) -> Bool {
        guard let key = Keymap.planKey(for: chord) else { return false }
        let planner = model.upcomingPlanner
        switch key {
        case .previousDay, .nextDay:
            let ids = selection.targets
            guard !ids.isEmpty else { return false }
            Task { await model.moveUpcoming(ids, days: key == .nextDay ? 1 : -1) }
            return true
        case .commit:
            guard planner.plan != nil else { return false }
            Task { await model.commitUpcomingPlan() }
            return true
        case .cancel:
            guard planner.plan != nil else { return false }
            planner.cancel()
            return true
        case .earlier, .later, .shorter, .longer:
            return false
        }
    }
}

/// Upcoming's days on iOS: the sections stacked in a scroll view, each
/// heading pinned while its rows scroll under it.
///
/// Not a `List`, because on iOS a `List` refuses every drop inside itself of
/// a drag one of its own rows started (see `TaskRows.container`). What the
/// `List` drew for free is drawn by ``upcomingCell(highlighted:)``,
/// ``upcomingHeading()`` and ``upcomingBadge(_:)`` instead. A row here has no
/// swipe actions, which only a `List` row has: Defer and Delete are both in
/// its context menu.
///
/// Focusable for keys only (`.edit`), so an iPad's keyboard still reaches the
/// rows' bindings and Upcoming's planning keys, and a tap on a phone does
/// what it did.
struct UpcomingStack<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 0, pinnedViews: [.sectionHeaders]) {
                content
            }
        }
        .focusable(interactions: .edit)
        .focusEffectDisabled()
    }
}

extension View {
    /// One line of an Upcoming section: what a `List` cell gives it on the
    /// Mac, drawn by hand in iOS's ``UpcomingStack``. `highlighted` is the
    /// keyboard's cursor or a ticked row, which a `List` highlights itself
    /// and `docs/10-cross-cutting/accessibility.md` requires to be visible.
    @ViewBuilder
    func upcomingCell(highlighted: Bool = false) -> some View {
        #if os(iOS)
        padding(.horizontal, 16)
            .padding(.vertical, 6)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(highlighted ? AnyShapeStyle(.tint.opacity(0.15)) : AnyShapeStyle(.clear))
        #else
        self
        #endif
    }

    /// An Upcoming section's heading: on iOS a band the rows scroll under,
    /// and a header to VoiceOver, both of which a `List` section header is.
    @ViewBuilder
    func upcomingHeading() -> some View {
        #if os(iOS)
        font(.subheadline.weight(.semibold))
            .padding(.horizontal, 16)
            .padding(.vertical, 8)
            .background(.bar)
            .accessibilityAddTraits(.isHeader)
        #else
        self
        #endif
    }

    /// A row's identity in iOS's ``UpcomingStack``: its section's and its
    /// task's together. A lazy stack keys a row by its task alone, so a task
    /// dropped on another day keeps the view it had under the old one, and
    /// that view is not drawn again: it shows the old day's "tomorrow" under
    /// the new day's heading. A `List` draws it afresh, so the Mac needs none.
    @ViewBuilder
    func upcomingIdentity(_ identity: String) -> some View {
        #if os(iOS)
        id(identity)
        #else
        self
        #endif
    }

    /// The deadline a row is listed under, beside it. A `List` row's
    /// `badge` on the Mac; outside a `List` a badge draws nothing, so on iOS
    /// it is the text itself.
    @ViewBuilder
    func upcomingBadge(_ badge: String?) -> some View {
        #if os(iOS)
        HStack(spacing: 8) {
            self
            if let badge {
                Text(badge)
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
        #else
        self.badge(badge.map { Text($0) })
        #endif
    }
}

/// Upcoming's header: the span chips, then whatever the planner has to say.
///
/// Above the rows rather than in them, so the chips stay put while the days
/// scroll and are there on an empty Upcoming too, where a longer span is the
/// one thing that might fill it.
struct UpcomingHeader: View {
    let model: TaskListModel

    var body: some View {
        VStack(spacing: 0) {
            Picker(L10n.Upcoming.span, selection: spanBinding) {
                ForEach(TaskListKind.upcomingSpans, id: \.self) { span in
                    Text(L10n.Upcoming.spanDays(count: Int(span))).tag(span)
                }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .accessibilityIdentifier("upcoming-span")
            .padding(.horizontal, 12)
            .padding(.vertical, 8)

            let planner = model.upcomingPlanner
            if let note = planner.note {
                if planner.awaitingDecision {
                    UpcomingDecisionBanner(text: note, model: model)
                } else {
                    NoteBanner(text: note) { planner.dismissNote() }
                }
            }
            if let error = planner.errorMessage {
                NoteBanner(text: error) { planner.dismissError() }
            }
            if let notice = model.upcomingNotice {
                Text(notice)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 12)
                    .padding(.bottom, 6)
            }
        }
    }

    private var spanBinding: Binding<UInt32> {
        Binding(
            get: { model.upcomingSpan },
            set: { span in Task { await model.setUpcomingSpan(span) } }
        )
    }
}

/// The note after a drop whose plan changed underneath it, with the two
/// answers it waits for. Buttons, because a phone has no Return or Esc.
private struct UpcomingDecisionBanner: View {
    let text: String
    let model: TaskListModel

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "info.circle")
            Text(text).font(.callout)
            Spacer()
            Button(L10n.Action.cancel) { model.upcomingPlanner.cancel() }
                .accessibilityIdentifier("upcoming-plan-cancel")
            Button(L10n.Calendar.planCommitChanged) { Task { await model.commitUpcomingPlan() } }
                .buttonStyle(.borderedProminent)
                .accessibilityIdentifier("upcoming-plan-commit")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .background(.quaternary.opacity(0.5))
    }
}

/// One section's heading: the day (or the stream, below the days), and, while
/// a drag or the keyboard is planning a move onto it, what dropping there
/// would do.
struct UpcomingSectionHeader: View {
    let title: String
    /// How many tasks the section holds, shown ones and "N more" alike.
    let total: Int
    /// The plan's consequence when the plan on screen moves tasks here.
    let summary: String?
    let identifier: String

    var body: some View {
        HStack(spacing: 6) {
            Text(title)
                .accessibilityIdentifier(identifier)
            if total != 0 {
                Text("\(total)")
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
            }
            Spacer(minLength: 8)
            if let summary {
                Text(summary)
                    .font(.caption.weight(.medium))
                    .padding(.horizontal, 8)
                    .padding(.vertical, 2)
                    .background(.tint.opacity(0.15), in: Capsule())
                    .accessibilityIdentifier("upcoming-plan-summary")
                    .onChange(of: summary, initial: true) { _, said in
                        AccessibilityNotification.Announcement(said).post()
                    }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(.rect)
    }
}

/// A block on a day of Upcoming: its time and its title. Not a row anything
/// can be done to — the calendar is where blocks are edited.
struct UpcomingBlockLine: View {
    let row: BlockGridRow
    let timeZone: String

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "rectangle.inset.filled")
                .foregroundStyle(.secondary)
                .accessibilityHidden(true)
            Text(span)
                .font(.callout.monospacedDigit())
                .foregroundStyle(.secondary)
            Text(row.title ?? L10n.Calendar.untitledBlock)
                .font(.callout)
        }
        .accessibilityElement(children: .combine)
    }

    private var span: String {
        let start = timeValueMs(value: row.block.startsAt, tz: timeZone)
        let end = timeValueMs(value: row.block.endsAt, tz: timeZone)
        return "\(PrintStamp.time(msSinceEpoch: start, timeZone: timeZone))–"
            + PrintStamp.time(msSinceEpoch: end, timeZone: timeZone)
    }
}
