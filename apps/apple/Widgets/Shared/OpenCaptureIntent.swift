import AppIntents
import Foundation

/// Bring Sunrise forward with the capture sheet open and empty.
///
/// What the Lock Screen capture widget's button runs. It is the foreground
/// counterpart of ``CaptureTaskIntent``, which captures a line it is handed
/// and never opens the app; this one has no line to hand over, so the only
/// useful thing it can do is put the field in front of the user.
///
/// **Why an intent and not a link.** `sunrise://capture` with no text is
/// refused by both link parsers, Swift's and `sunrise-client-core`'s, because
/// the URL scheme is a contract every client shares
/// (`docs/07-clients/interaction-patterns.md` §URL scheme). A widget button
/// can run an intent instead, and the app is the only surface that has a
/// capture sheet to open — so the route stays inside this app.
///
/// Compiled into both apps and both widget extensions (`Widgets/Shared/`).
/// Its ``supportedModes`` asks the system to open the app, which is what makes
/// it run in the app's process, where ``QuickCaptureRequest`` has someone
/// listening. Its metadata names catalog keys as literals, for the reason
/// ``CaptureTaskIntent/title`` gives.
struct OpenCaptureIntent: AppIntent {
    static let title: LocalizedStringResource = LocalizedStringResource(
        "apple.intents.open_capture.title",
        table: "Localizable"
    )

    static let description = IntentDescription(
        LocalizedStringResource("apple.intents.open_capture.description", table: "Localizable")
    )

    /// Opening the app is the whole point: there is nothing to capture until
    /// somebody types it.
    static let supportedModes: IntentModes = .foreground

    /// The widget's own action, not a Shortcuts action. Shortcuts already has
    /// **Capture Task**, which captures without opening anything.
    static let isDiscoverable = false

    init() {}

    @MainActor
    func perform() async throws -> some IntentResult {
        QuickCaptureRequest.post()
        return .result()
    }
}

/// A request to open quick capture, held until the app can act on it.
///
/// The intent can run before the app has a capture field to show: a cold
/// launch from the Lock Screen runs it while the vault is still opening, and
/// on iOS the field only exists once a vault is attached. So a request nobody
/// can serve yet is kept, and ``AppSurfaces`` takes it when it attaches a vault.
@MainActor
enum QuickCaptureRequest {
    /// Opens the capture field and says whether it could. Installed by the
    /// app; `nil` in a widget extension, which has no field to open.
    static var presenter: (@MainActor () -> Bool)?

    /// Whether a request is waiting for a capture field to exist.
    private(set) static var isPending = false

    /// Ask for the capture field: now if the app can show it, else once it can.
    static func post() {
        isPending = presenter?() != true
    }

    /// Whether a request was waiting, clearing it either way.
    static func takePending() -> Bool {
        defer { isPending = false }
        return isPending
    }
}
