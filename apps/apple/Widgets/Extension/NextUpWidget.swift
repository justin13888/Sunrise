import SwiftUI
import WidgetKit

/// "Next up": how much of Today is left, and what comes first.
///
/// `docs/07-clients/mobile-ios.md` §Widgets is the specification. One widget
/// in every size rather than a widget per size, because the question is the
/// same at every size — only how many rows answer it changes.
struct NextUpWidget: Widget {
    static let kind = "dev.sunrise.widget.next-up"

    var body: some WidgetConfiguration {
        StaticConfiguration(kind: Self.kind, provider: NextUpProvider()) { entry in
            NextUpView(entry: entry)
        }
        .configurationDisplayName(L10n.Widget.displayName)
        .description(L10n.Widget.description)
        .supportedFamilies(Self.families)
    }

    /// The Lock Screen's accessory families exist on iOS only; a Mac offers
    /// the Notification Centre and desktop sizes.
    static var families: [WidgetFamily] {
        #if os(iOS)
        [.systemSmall, .systemMedium, .systemLarge,
         .accessoryInline, .accessoryCircular, .accessoryRectangular]
        #else
        [.systemSmall, .systemMedium, .systemLarge]
        #endif
    }
}

struct NextUpView: View {
    let entry: NextUpEntry
    @Environment(\.widgetFamily) private var family

    var body: some View {
        content
            .containerBackground(.fill.tertiary, for: .widget)
            // The whole widget opens the first task. A row on the medium and
            // large sizes opens its own — see `RowView` — and a tap anywhere
            // else lands here.
            .widgetURL(entry.snapshot?.rows.first?.link)
    }

    @ViewBuilder
    private var content: some View {
        if let snapshot = entry.snapshot {
            switch family {
            #if os(iOS)
            case .accessoryInline:
                InlineView(snapshot: snapshot)
            case .accessoryCircular:
                CircularView(snapshot: snapshot)
            case .accessoryRectangular:
                RectangularView(snapshot: snapshot)
            #endif
            case .systemSmall:
                SmallView(snapshot: snapshot)
            case .systemLarge, .systemExtraLarge:
                ListView(snapshot: snapshot, rows: 8)
            default:
                ListView(snapshot: snapshot, rows: 3)
            }
        } else {
            NoVaultView(family: family)
        }
    }
}

// MARK: - Sizes

/// A number and the first title. The glance at its smallest.
private struct SmallView: View {
    let snapshot: WidgetSnapshot

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Header(snapshot: snapshot)
            Spacer(minLength: 0)
            if let first = snapshot.rows.first {
                SectionLabel(section: first.section)
                Text(first.title)
                    .font(.subheadline.weight(.medium))
                    .lineLimit(3)
            } else {
                AllClear()
            }
            Spacer(minLength: 0)
            Stamp(snapshot: snapshot)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// The count, then as many rows as the size has room for, each one a link.
private struct ListView: View {
    let snapshot: WidgetSnapshot
    let rows: Int

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .firstTextBaseline) {
                Header(snapshot: snapshot)
                Spacer()
                Stamp(snapshot: snapshot)
            }
            if snapshot.rows.isEmpty {
                Spacer(minLength: 0)
                AllClear()
            } else {
                ForEach(snapshot.rows.prefix(rows)) { row in
                    RowView(row: row)
                }
                let hidden = snapshot.outstanding - min(rows, snapshot.rows.count)
                if hidden > 0 {
                    Text(L10n.Widget.more(count: hidden))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
            Spacer(minLength: 0)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

#if os(iOS)
/// One line above the clock.
private struct InlineView: View {
    let snapshot: WidgetSnapshot

    var body: some View {
        if let first = snapshot.rows.first {
            Text(verbatim: "\(L10n.Widget.left(count: snapshot.outstanding)) · \(first.title)")
        } else {
            Text(L10n.Widget.allClear)
        }
    }
}

/// The count in a ring.
private struct CircularView: View {
    let snapshot: WidgetSnapshot

    var body: some View {
        ZStack {
            AccessoryWidgetBackground()
            VStack(spacing: 0) {
                Text("\(snapshot.outstanding)")
                    .font(.title2.weight(.semibold))
                    .widgetAccentable()
                Text(L10n.Widget.circularToday)
                    .font(.caption2)
            }
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(L10n.Widget.circularAccessibility(count: snapshot.outstanding))
    }
}

/// The count and the first two titles.
private struct RectangularView: View {
    let snapshot: WidgetSnapshot

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(verbatim: snapshot.overdue > 0
                ? "\(L10n.Widget.left(count: snapshot.outstanding)) · \(L10n.Widget.overdue(count: snapshot.overdue))"
                : L10n.Widget.leftToday(count: snapshot.outstanding))
                .font(.headline)
                .widgetAccentable()
            if snapshot.rows.isEmpty {
                Text(L10n.Widget.allClear)
            }
            ForEach(snapshot.rows.prefix(2)) { row in
                Text(row.title).lineLimit(1)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}
#endif

// MARK: - Pieces

private struct Header: View {
    let snapshot: WidgetSnapshot

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 4) {
            Text("\(snapshot.outstanding)")
                .font(.title.weight(.semibold))
                .widgetAccentable()
            Text(L10n.Widget.leftTodayLabel(count: snapshot.outstanding))
                .font(.caption)
                .foregroundStyle(.secondary)
            if snapshot.overdue > 0 {
                Text(L10n.Widget.overdue(count: snapshot.overdue))
                    .font(.caption.weight(.medium))
                    .foregroundStyle(.red)
            }
        }
        .accessibilityElement(children: .combine)
    }
}

private struct RowView: View {
    let row: WidgetSnapshot.Row

    var body: some View {
        let label = HStack(spacing: 6) {
            Circle()
                .fill(row.section.tint)
                .frame(width: 6, height: 6)
                .accessibilityHidden(true)
            Text(row.title)
                .font(.subheadline)
                .lineLimit(1)
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(Text(verbatim: "\(row.title), \(row.section.spoken)"))
        if let link = row.link {
            Link(destination: link) { label }
        } else {
            label
        }
    }
}

private struct SectionLabel: View {
    let section: WidgetSnapshot.Section

    var body: some View {
        Text(section.spoken.uppercased())
            .font(.caption2.weight(.semibold))
            .foregroundStyle(section.tint)
    }
}

private struct AllClear: View {
    var body: some View {
        Label(L10n.Widget.allClear, systemImage: "checkmark.circle")
            .font(.subheadline)
            .foregroundStyle(.secondary)
    }
}

/// "5 min" since the snapshot was written. The only way a widget can say how
/// stale it is.
///
/// VoiceOver reads it as the label "Updated" with the live elapsed time as its
/// value. Not one formatted sentence: the timeline never redraws on its own
/// (see ``NextUpProvider``), so an age baked into a string would stay at
/// whatever it was when the entry was made, while the relative `Text` is kept
/// current by the system.
private struct Stamp: View {
    let snapshot: WidgetSnapshot

    var body: some View {
        let written = Date(timeIntervalSince1970: Double(snapshot.writtenAtMs) / 1000)
        Text(written, style: .relative)
            .font(.caption2)
            .foregroundStyle(.tertiary)
            .accessibilityLabel(L10n.Widget.updated)
            .accessibilityValue(Text(written, style: .relative))
    }
}

/// No snapshot: Sunrise has not been opened, or its vault is locked.
///
/// Says what to do rather than showing an empty list, which would read as
/// "nothing to do today" — the one thing it must not say when the truth is
/// that it cannot see.
private struct NoVaultView: View {
    let family: WidgetFamily

    var body: some View {
        switch family {
        #if os(iOS)
        case .accessoryInline:
            Text(L10n.Widget.openApp)
        case .accessoryCircular:
            ZStack {
                AccessoryWidgetBackground()
                Image(systemName: "sunrise")
            }
            .accessibilityLabel(L10n.Widget.openApp)
        case .accessoryRectangular:
            VStack(alignment: .leading) {
                Text(L10n.Common.productName).font(.headline)
                Text(L10n.Widget.openToShowToday)
            }
        #endif
        default:
            VStack(alignment: .leading, spacing: 4) {
                Image(systemName: "sunrise")
                    .font(.title2)
                    .foregroundStyle(.orange)
                Text(L10n.Widget.openAppToShowToday)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .leading)
        }
    }
}

extension WidgetSnapshot.Section {
    var tint: Color {
        switch self {
        case .overdue: .red
        case .due: .orange
        case .scheduled: .secondary
        }
    }

    var spoken: String {
        switch self {
        case .overdue: L10n.Widget.sectionOverdue
        case .due: L10n.Widget.sectionDue
        case .scheduled: L10n.Widget.sectionScheduled
        }
    }
}
