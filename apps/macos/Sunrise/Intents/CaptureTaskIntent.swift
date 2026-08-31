import AppIntents
import Foundation

/// Capture a task from Shortcuts, Spotlight or Siri.
///
/// The app's core verb, and the one that most wants an automation surface: it
/// already has a global hotkey and a menu bar item, and this is the third way
/// in — the one a Stream Deck button, a mail rule, or a spoken sentence can
/// reach.
///
/// **Parity is the requirement, not the feature.** `parity-matrix.md`
/// §Capture-surface portability says the resulting Task must be identical
/// regardless of origin, "because every surface calls the same parser". So
/// this runs the line through `previewCapture` — the same call behind ⌘⇧N,
/// which is `sunrise_domain::capture` across the seam — and submits the draft
/// it returns, unmodified. There is no second parse here and there must never
/// be one.
///
/// It lands in the Inbox unless the line names a Stream with `#`, which is
/// what `inbox-and-capture.md` requires of every capture from outside the app.
/// That falls out of using the parser rather than being enforced here: this
/// surface has no "current stream" to default to in the first place.
struct CaptureTaskIntent: AppIntent {
    static let title: LocalizedStringResource = "Capture Task"

    static let description = IntentDescription(
        """
        Adds a task to Sunrise, reading the same #stream, @context, ^when, \
        !priority and ~estimate tags that quick capture reads.
        """,
        categoryName: "Capture",
        searchKeywords: ["task", "todo", "capture", "inbox", "add"]
    )

    /// Runs without pulling Sunrise forward. Every one of these verbs is
    /// something you do *instead of* opening the app; a capture that stole
    /// focus would interrupt exactly the thought it exists to catch.
    static let supportedModes: IntentModes = .background

    static var parameterSummary: some ParameterSummary {
        Summary("Capture \(\.$line) in Sunrise")
    }

    @Parameter(
        title: "Task",
        description: "A capture line, tags and all.",
        requestValueDialog: "What would you like to capture?"
    )
    var line: String

    init() {}

    init(line: String) {
        self.line = line
    }

    func perform() async throws -> some IntentResult & ReturnsValue<TaskEntity> & ProvidesDialog {
        let text = line
        let captured = try await IntentVault.withVault { bridge in
            try await Self.capture(text, into: bridge)
        }
        return .result(value: captured.task, dialog: IntentDialog("\(captured.message)"))
    }

    /// What one capture produced, in types that survive leaving the vault.
    struct Captured: Sendable, Equatable {
        let task: TaskEntity
        let message: String
    }

    /// The whole of the work, with the vault passed in.
    ///
    /// Separated from ``perform()`` so it can be run against a scratch vault
    /// in a test — an `AppIntent` that compiles and has never been executed is
    /// exactly the defect this surface was audited for.
    static func capture(_ line: String, into bridge: CoreBridge) async throws -> Captured {
        let text = line.trimmed
        guard !text.isEmpty else { throw IntentError.nothingToCapture }
        do {
            let preview = try await bridge.previewCapture(
                text,
                timeZone: TimeZone.current.identifier
            )
            let title = preview.draft.title.trimmed
            guard !title.isEmpty else { throw IntentError.emptyTitle }
            let outcome = try await bridge.submit(.createTask(draft: preview.draft))
            return Captured(
                task: TaskEntity(id: outcome.entity, title: title, isCompleted: false),
                message: message(title: title, issues: preview.issues)
            )
        } catch {
            throw IntentError.wrapping(error)
        }
    }

    /// What to say back.
    ///
    /// Unresolved tokens are *reported*, not hidden. Their text is still in the
    /// title so nothing was lost, but a capture surface with no screen has no
    /// other way to say "there is no Stream called that" — and filing a task
    /// somewhere other than where the user believes they named is precisely
    /// the quiet failure this surface must not have.
    static func message(title: String, issues: [CaptureIssue]) -> String {
        guard let first = issues.first else { return "Captured “\(title)”." }
        let rest = issues.count - 1
        guard rest > 0 else { return "Captured “\(title)”. \(first.explanation)" }
        return "Captured “\(title)”. \(first.explanation) And \(rest) more."
    }
}
