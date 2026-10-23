import AppIntents
import Foundation

/// The two fixed lists, as read-only intents.
///
/// Two intents rather than one with a "which list" parameter, and that is a
/// considered choice: an `AppShortcut` is matched by phrase, and "what's on my
/// Sunrise list today" and "what's in my Sunrise inbox" are two sentences that
/// must reach two different reads. Folding them into one intent would leave
/// each phrase depending on a preset parameter surviving into the shortcut
/// definition, which is a subtler contract than two small structs.
enum TaskListReader {
    /// Open tasks only. A list of things to do that included the things
    /// already done would not be a list of things to do.
    static func open(from rows: [TaskItem]) -> [TaskEntity] {
        rows
            .filter { $0.state != .done && $0.state != .cancelled }
            .map(TaskEntity.init)
    }

    /// Which of the two fixed lists to read.
    enum List: Sendable {
        case today
        case inbox
    }

    /// Read one of them.
    ///
    /// `Query::Today` is asked with one reading of the *core's* clock, which is
    /// the rule every screen follows — so the automation surface and the
    /// window cannot disagree about what "today" is.
    static func rows(_ list: List, in bridge: CoreBridge) async throws -> [TaskEntity] {
        do {
            let result: CoreQueryResult
            switch list {
            case .today:
                let now = await bridge.nowMs()
                result = try await bridge.query(.today(nowMs: now, contexts: []))
            case .inbox:
                result = try await bridge.query(.inbox)
            }
            guard case let .tasks(rows) = result else { return [] }
            return open(from: rows)
        } catch {
            throw IntentError.wrapping(error)
        }
    }

    static func message(_ tasks: [TaskEntity], empty: String) -> String {
        tasks.isEmpty
            ? empty
            : L10n.Intents.TaskList.summary(
                count: tasks.count,
                titles: tasks.map(\.title).joined(separator: ", ")
            )
    }
}

/// Today: what is scheduled, due, or overdue.
struct TodayTasksIntent: AppIntent {
    /// Catalog keys as literals; see the note on ``CaptureTaskIntent/title``.
    static let title: LocalizedStringResource = LocalizedStringResource(
        "apple.intents.task_list.today_title",
        table: "Localizable"
    )

    static let description = IntentDescription(
        LocalizedStringResource("apple.intents.task_list.today_description", table: "Localizable"),
        categoryName: LocalizedStringResource("apple.intents.category_tasks", table: "Localizable"),
        searchKeywords: ["today", "agenda", "due", "list"]
    )

    /// Runs without pulling Sunrise forward. Every one of these verbs is
    /// something you do *instead of* opening the app; a capture that stole
    /// focus would interrupt exactly the thought it exists to catch.
    static let supportedModes: IntentModes = .background

    static var parameterSummary: some ParameterSummary {
        Summary("Get today's tasks from Sunrise")
    }

    func perform() async throws -> some IntentResult & ReturnsValue<[TaskEntity]> & ProvidesDialog {
        let tasks = try await IntentVault.withVault { bridge in
            try await TaskListReader.rows(.today, in: bridge)
        }
        let message = TaskListReader.message(tasks, empty: L10n.Intents.TaskList.todayEmpty)
        return .result(value: tasks, dialog: IntentDialog("\(message)"))
    }
}

/// The Inbox: everything captured and not yet triaged.
struct InboxTasksIntent: AppIntent {
    /// Catalog keys as literals; see the note on ``CaptureTaskIntent/title``.
    static let title: LocalizedStringResource = LocalizedStringResource(
        "apple.intents.task_list.inbox_title",
        table: "Localizable"
    )

    static let description = IntentDescription(
        LocalizedStringResource("apple.intents.task_list.inbox_description", table: "Localizable"),
        categoryName: LocalizedStringResource("apple.intents.category_tasks", table: "Localizable"),
        searchKeywords: ["inbox", "triage", "unsorted", "list"]
    )

    /// Runs without pulling Sunrise forward. Every one of these verbs is
    /// something you do *instead of* opening the app; a capture that stole
    /// focus would interrupt exactly the thought it exists to catch.
    static let supportedModes: IntentModes = .background

    static var parameterSummary: some ParameterSummary {
        Summary("Get inbox tasks from Sunrise")
    }

    func perform() async throws -> some IntentResult & ReturnsValue<[TaskEntity]> & ProvidesDialog {
        let tasks = try await IntentVault.withVault { bridge in
            try await TaskListReader.rows(.inbox, in: bridge)
        }
        let message = TaskListReader.message(tasks, empty: L10n.Intents.TaskList.inboxEmpty)
        return .result(value: tasks, dialog: IntentDialog("\(message)"))
    }
}
