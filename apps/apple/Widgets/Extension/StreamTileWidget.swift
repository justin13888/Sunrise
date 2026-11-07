import AppIntents
import SwiftUI
import WidgetKit

/// "Stream": the first open tasks in a stream the user picks.
///
/// `docs/07-clients/mobile-ios.md` §Lock Screen widgets is the specification:
/// a large widget, configured with one stream. Like ``NextUpWidget`` it draws
/// the snapshot and computes nothing, so the stream list the picker offers
/// comes from the snapshot too — the extension never opens the vault.
struct StreamTileWidget: Widget {
    static let kind = "dev.sunrise.widget.stream-tile"

    var body: some WidgetConfiguration {
        AppIntentConfiguration(
            kind: Self.kind,
            intent: StreamTileIntent.self,
            provider: StreamTileProvider()
        ) { entry in
            StreamTileView(entry: entry)
        }
        .configurationDisplayName(L10n.Widget.StreamTile.displayName)
        .description(L10n.Widget.StreamTile.description)
        .supportedFamilies([.systemLarge])
    }
}

// MARK: - Configuration

/// The widget's one setting: which stream it shows.
///
/// Metadata names catalog keys as literals, for the reason
/// `CaptureTaskIntent.title` gives.
struct StreamTileIntent: WidgetConfigurationIntent {
    static let title: LocalizedStringResource = LocalizedStringResource(
        "apple.widget.stream_tile.intent_title",
        table: "Localizable"
    )

    static let description = IntentDescription(
        LocalizedStringResource("apple.widget.stream_tile.intent_description", table: "Localizable")
    )

    @Parameter(title: LocalizedStringResource("apple.widget.stream_tile.stream_parameter", table: "Localizable"))
    var stream: StreamTileEntity?

    init() {}
}

/// A stream, as the widget's picker sees it: the snapshot's id and name.
///
/// Not the app's `StreamEntity`, whose query opens the vault. This one reads
/// the snapshot and nothing else.
struct StreamTileEntity: AppEntity {
    static let typeDisplayRepresentation = TypeDisplayRepresentation(
        name: LocalizedStringResource("apple.widget.stream_tile.stream_type", table: "Localizable")
    )
    static let defaultQuery = StreamTileQuery()

    /// The stream's `EntityRef`.
    let id: String
    let name: String

    var displayRepresentation: DisplayRepresentation {
        DisplayRepresentation(title: "\(name)")
    }
}

/// The streams the snapshot carries, which is every stream the widget can show.
struct StreamTileQuery: EntityQuery {
    /// Every id the system asks about is answered, named from the snapshot
    /// where it can be. A configured widget keeps its stream while the vault is
    /// locked and the snapshot is gone; it draws "Open Sunrise" until the
    /// stream is back, rather than forgetting which one it was set to.
    func entities(for identifiers: [StreamTileEntity.ID]) async throws -> [StreamTileEntity] {
        let snapshot = WidgetSnapshotStore.appGroup()?.read()
        return identifiers.map { id in
            StreamTileEntity(id: id, name: snapshot?.stream(id)?.name ?? "")
        }
    }

    func suggestedEntities() async throws -> [StreamTileEntity] {
        (WidgetSnapshotStore.appGroup()?.read()?.streams ?? []).map {
            StreamTileEntity(id: $0.id, name: $0.name)
        }
    }
}

// MARK: - Timeline

/// One moment of the Stream tile.
struct StreamTileEntry: TimelineEntry {
    let date: Date
    let state: State

    enum State {
        /// No snapshot: no vault open, or the app never run.
        case noVault
        /// The widget has no stream set yet.
        case unchosen
        /// The stream it is set to is not in the snapshot.
        case missing
        case tile(WidgetSnapshot.StreamTile, writtenAtMs: Int64)
    }
}

/// Reads the snapshot the app wrote, with ``NextUpProvider``'s one-entry,
/// `.never` timeline for the same reason: only the app writing a new snapshot
/// can change what this draws.
struct StreamTileProvider: AppIntentTimelineProvider {
    func placeholder(in context: Context) -> StreamTileEntry {
        StreamTileEntry(date: .now, state: .tile(.sample, writtenAtMs: WidgetSnapshot.sample.writtenAtMs))
    }

    func snapshot(for configuration: StreamTileIntent, in context: Context) async -> StreamTileEntry {
        // The gallery shows the sample rather than someone's real titles; see
        // `NextUpProvider.getSnapshot`.
        context.isPreview ? placeholder(in: context) : entry(for: configuration)
    }

    func timeline(for configuration: StreamTileIntent, in context: Context) async -> Timeline<StreamTileEntry> {
        Timeline(entries: [entry(for: configuration)], policy: .never)
    }

    private func entry(for configuration: StreamTileIntent) -> StreamTileEntry {
        StreamTileEntry(
            date: .now,
            state: Self.state(
                snapshot: WidgetSnapshotStore.appGroup()?.read(),
                chosen: configuration.stream?.id
            )
        )
    }

    /// No snapshot outranks everything: with the vault closed, the widget
    /// cannot tell a stream that is gone from one it simply cannot see.
    static func state(snapshot: WidgetSnapshot?, chosen: String?) -> StreamTileEntry.State {
        guard let snapshot else { return .noVault }
        guard let chosen else { return .unchosen }
        guard let tile = snapshot.stream(chosen) else { return .missing }
        return .tile(tile, writtenAtMs: snapshot.writtenAtMs)
    }
}

extension WidgetSnapshot.StreamTile {
    /// A made-up stream for the gallery and the placeholder. Never a user's
    /// data.
    static let sample = WidgetSnapshot.StreamTile(
        id: "sample-stream",
        name: L10n.Widget.StreamTile.sampleName,
        outstanding: 4,
        rows: WidgetSnapshot.sample.rows.map { Item(id: $0.id, title: $0.title, link: nil) }
    )
}

// MARK: - Views

struct StreamTileView: View {
    let entry: StreamTileEntry

    var body: some View {
        content
            .containerBackground(.fill.tertiary, for: .widget)
    }

    @ViewBuilder
    private var content: some View {
        switch entry.state {
        case .noVault:
            StreamTileMessage(text: L10n.Widget.StreamTile.openApp)
        case .unchosen:
            StreamTileMessage(text: L10n.Widget.StreamTile.choose)
        case .missing:
            StreamTileMessage(text: L10n.Widget.StreamTile.missing)
        case let .tile(tile, writtenAtMs):
            StreamTileList(tile: tile, writtenAtMs: writtenAtMs)
                // Elsewhere than a row, a tap opens the first task, as on
                // Next Up.
                .widgetURL(tile.rows.first?.link)
        }
    }
}

/// The stream's name and count, then its rows, each one a link.
private struct StreamTileList: View {
    let tile: WidgetSnapshot.StreamTile
    let writtenAtMs: Int64

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .firstTextBaseline) {
                Text(tile.name)
                    .font(.headline)
                    .lineLimit(1)
                    .widgetAccentable()
                Text(L10n.Widget.StreamTile.openCount(count: tile.outstanding))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Spacer()
                let written = Date(timeIntervalSince1970: Double(writtenAtMs) / 1000)
                Text(written, style: .relative)
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
                    .accessibilityLabel(L10n.Widget.updated)
                    .accessibilityValue(Text(written, style: .relative))
            }
            .accessibilityElement(children: .combine)
            if tile.rows.isEmpty {
                Spacer(minLength: 0)
                Label(L10n.Widget.StreamTile.empty, systemImage: "checkmark.circle")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            } else {
                ForEach(tile.rows) { row in
                    StreamTileRow(row: row)
                }
                let hidden = tile.outstanding - tile.rows.count
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

private struct StreamTileRow: View {
    let row: WidgetSnapshot.StreamTile.Item

    var body: some View {
        let label = HStack(spacing: 6) {
            Circle()
                .fill(.secondary)
                .frame(width: 6, height: 6)
                .accessibilityHidden(true)
            Text(row.title)
                .font(.subheadline)
                .lineLimit(1)
        }
        if let link = row.link {
            Link(destination: link) { label }
        } else {
            label
        }
    }
}

/// Why there is nothing to draw, and what to do about it.
private struct StreamTileMessage: View {
    let text: String

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Image(systemName: "sunrise")
                .font(.title2)
                .foregroundStyle(.orange)
            Text(text)
                .font(.subheadline)
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .leading)
    }
}
