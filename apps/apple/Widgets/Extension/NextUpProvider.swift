import WidgetKit

/// One moment of the Next Up widget: the snapshot as it stood, or `nil` for
/// none — no vault open, the app never run, or a file this build cannot read.
struct NextUpEntry: TimelineEntry {
    let date: Date
    let snapshot: WidgetSnapshot?
}

/// Reads the snapshot the app wrote. It computes nothing.
///
/// **A timeline of one entry, with a `.never` policy.** The widget has nothing
/// it could recompute on a schedule — which tasks are next is the core's
/// decision, and the core is not in this process — so the only thing that can
/// make a new entry worth drawing is the app writing a new snapshot, and the
/// app asks WidgetKit to reload when it does. The age stamp is a relative
/// `Text`, which the system keeps current without a new entry.
struct NextUpProvider: TimelineProvider {
    func placeholder(in context: Context) -> NextUpEntry {
        NextUpEntry(date: .now, snapshot: .sample)
    }

    func getSnapshot(in context: Context, completion: @escaping (NextUpEntry) -> Void) {
        // The gallery shows the sample rather than someone's real titles:
        // the preview is a picture of what the widget does, drawn in a
        // system sheet the user did not open to read their tasks in.
        completion(context.isPreview ? placeholder(in: context) : current())
    }

    func getTimeline(in context: Context, completion: @escaping (Timeline<NextUpEntry>) -> Void) {
        completion(Timeline(entries: [current()], policy: .never))
    }

    private func current() -> NextUpEntry {
        NextUpEntry(date: .now, snapshot: WidgetSnapshotStore.appGroup()?.read())
    }
}

extension WidgetSnapshot {
    /// Made-up rows for the gallery and the redacted placeholder. Never a
    /// user's data.
    static let sample = WidgetSnapshot(
        writtenAtMs: Int64(Date.now.timeIntervalSince1970 * 1000),
        outstanding: 4,
        overdue: 1,
        rows: [
            Row(id: "sample-1", title: L10n.Widget.sampleReport, section: .overdue, link: nil),
            Row(id: "sample-2", title: L10n.Widget.sampleDraft, section: .due, link: nil),
            Row(id: "sample-3", title: L10n.Widget.sampleFlights, section: .scheduled, link: nil),
            Row(id: "sample-4", title: L10n.Widget.samplePlants, section: .scheduled, link: nil)
        ]
    )
}
