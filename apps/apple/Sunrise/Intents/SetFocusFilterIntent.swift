#if os(iOS)
import AppIntents
import Foundation

/// Sunrise's Focus filter: which streams a Focus is about.
///
/// `docs/07-clients/mobile-ios.md` §Focus Filters. Added to a Focus in
/// Settings ▸ Focus ▸ Focus Filters, it narrows Today to the picked streams and
/// mutes reminders from every other stream for as long as that Focus is on.
///
/// iOS runs ``perform()`` when the Focus turns on, with the streams picked, and
/// again when it turns off, with the parameter back at its default — `nil` —
/// which lifts the filter. All it does is record the scope in
/// ``FocusFilterStore``; the list and ``ReminderScheduler`` read it from there,
/// so what a Focus means is decided in one place (``FocusFilter``) and not here.
///
/// **iOS only, for now.** macOS has the same API, but `desktop.md` specifies
/// no Focus filter and the Mac shell does not re-read Today when the scope
/// changes. Shipping the intent there would offer a filter that half works.
struct SunriseFocusFilter: SetFocusFilterIntent {
    static let title: LocalizedStringResource = "Set Sunrise Streams"

    static let description = IntentDescription(
        """
        Shows only the chosen streams in Today, and mutes reminders from the \
        other streams, while this Focus is on.
        """
    )

    @Parameter(title: "Streams")
    var streams: [StreamEntity]?

    init() {}

    init(streams: [StreamEntity]?) {
        self.streams = streams
    }

    /// What Settings prints under the filter.
    var displayRepresentation: DisplayRepresentation {
        DisplayRepresentation(
            title: "Sunrise",
            subtitle: "\(Self.summary(of: streams?.map(\.name) ?? []))"
        )
    }

    /// The subtitle, as a sentence.
    static func summary(of names: [String]) -> String {
        switch names.count {
        case 0: "All streams"
        case 1, 2, 3: names.joined(separator: ", ")
        default: "\(names.prefix(2).joined(separator: ", ")) and \(names.count - 2) more"
        }
    }

    func perform() async throws -> some IntentResult {
        let picked = streams?.map { (id: $0.id, name: $0.name) }
        await FocusFilterStore.shared.apply(picked)
        // The reminders the OS already holds are re-planned now, whether or
        // not a window is open; Today re-reads the scope when it is next
        // drawn, and at once if it is on screen (`iOS/VaultSurfaces.swift`).
        await BackgroundHost.shared.focusFilterChanged()
        return .result()
    }
}
#endif
