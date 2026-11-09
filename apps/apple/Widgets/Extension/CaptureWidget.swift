#if os(iOS)
import AppIntents
import SwiftUI
import WidgetKit

/// "Quick Capture": one tap on the Lock Screen, and Sunrise opens with the
/// capture sheet ready.
///
/// `docs/07-clients/mobile-ios.md` §Widgets is the specification.
/// The button runs ``OpenCaptureIntent``, which opens the app; nothing here
/// reads the snapshot, because a capture field needs nothing from the vault to
/// appear. A locked phone asks for Face ID first, as it does for any widget
/// that opens its app.
///
/// iOS only: the accessory families are the Lock Screen's, and a Mac has the
/// ⌘⇧N hotkey and the menu bar item for the same job.
struct CaptureWidget: Widget {
    static let kind = "dev.sunrise.widget.capture"

    var body: some WidgetConfiguration {
        StaticConfiguration(kind: Self.kind, provider: CaptureProvider()) { _ in
            CaptureWidgetView()
        }
        .configurationDisplayName(L10n.Widget.Capture.displayName)
        .description(L10n.Widget.Capture.description)
        .supportedFamilies([.accessoryCircular])
    }
}

/// The widget's one moment. It draws the same thing forever.
struct CaptureEntry: TimelineEntry {
    let date: Date
}

/// A timeline of one entry that never changes, because nothing the widget
/// draws depends on anything.
struct CaptureProvider: TimelineProvider {
    func placeholder(in context: Context) -> CaptureEntry {
        CaptureEntry(date: .now)
    }

    func getSnapshot(in context: Context, completion: @escaping (CaptureEntry) -> Void) {
        completion(placeholder(in: context))
    }

    func getTimeline(in context: Context, completion: @escaping (Timeline<CaptureEntry>) -> Void) {
        completion(Timeline(entries: [placeholder(in: context)], policy: .never))
    }
}

struct CaptureWidgetView: View {
    var body: some View {
        Button(intent: OpenCaptureIntent()) {
            ZStack {
                AccessoryWidgetBackground()
                Image(systemName: "plus")
                    .font(.title2.weight(.semibold))
                    .widgetAccentable()
            }
        }
        .buttonStyle(.plain)
        .accessibilityLabel(L10n.Widget.Capture.accessibility)
        .containerBackground(.clear, for: .widget)
    }
}
#endif
