import AppIntents
import Foundation

/// Say how one stream stands: how much is open, and what comes first.
///
/// The read half of `mobile-ios.md` §Shortcuts and App Intents' list, beside
/// the two fixed lists. "What's left in Errands?" is a question about one
/// stream, and answering it with the whole Today list would answer a different
/// one.
struct StreamSummaryIntent: AppIntent {
    static let title: LocalizedStringResource = "Get Stream Summary"

    static let description = IntentDescription(
        "Says how many tasks are open in a Sunrise stream, and names the first few.",
        categoryName: "Tasks",
        searchKeywords: ["stream", "summary", "project", "area", "open"]
    )

    /// Runs without pulling Sunrise forward; see ``CaptureTaskIntent``.
    static let supportedModes: IntentModes = .background

    static var parameterSummary: some ParameterSummary {
        Summary("Summarise \(\.$stream) in Sunrise")
    }

    @Parameter(title: "Stream", requestValueDialog: "Which stream?")
    var stream: StreamEntity

    init() {}

    init(stream: StreamEntity) {
        self.stream = stream
    }

    func perform() async throws -> some IntentResult & ReturnsValue<[TaskEntity]> & ProvidesDialog {
        let target = stream.id
        let summary = try await IntentVault.withVault { bridge in
            try await Self.summarise(target, in: bridge)
        }
        return .result(value: summary.tasks, dialog: IntentDialog("\(summary.message)"))
    }

    /// How many open tasks a summary names. Past this a spoken answer stops
    /// being one.
    static let named = 3

    /// What one summary produced.
    struct Report: Sendable, Equatable {
        /// Every open task in the stream, in the core's order.
        let tasks: [TaskEntity]
        let message: String
    }

    /// The whole of the work, with the vault passed in — see
    /// ``CaptureTaskIntent/capture(_:into:)``.
    ///
    /// The open tasks are `Query::StreamTasks` with the finished ones removed,
    /// by ``TaskListReader/open(from:)`` — the same rule the two list intents
    /// use, so "open" means one thing across the surface.
    static func summarise(_ id: EntityRef, in bridge: CoreBridge) async throws -> Report {
        let row = try await StreamLookup.row(id, in: bridge)
        do {
            guard case let .tasks(rows) = try await bridge.query(.streamTasks(stream: id)) else {
                return Report(tasks: [], message: message(stream: row.name, open: []))
            }
            let open = TaskListReader.open(from: rows.filter { !$0.deleted })
            return Report(tasks: open, message: message(stream: row.name, open: open))
        } catch {
            throw IntentError.wrapping(error)
        }
    }

    static func message(stream: String, open: [TaskEntity]) -> String {
        let first = open.prefix(named).map(\.title)
        switch open.count {
        case 0:
            return "Nothing is open in \(stream)."
        case 1:
            return "1 task open in \(stream): \(first[0])."
        default:
            let rest = open.count - first.count
            let more = rest > 0 ? ", and \(rest) more" : ""
            return "\(open.count) tasks open in \(stream): \(first.joined(separator: ", "))\(more)."
        }
    }
}
