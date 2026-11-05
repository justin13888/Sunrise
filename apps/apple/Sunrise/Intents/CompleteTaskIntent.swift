import AppIntents
import Foundation

/// Mark a task done.
///
/// The second verb worth automating: capture puts things in, this takes them
/// out, and between them they are most of what a person does to a list in a
/// day without opening it.
///
/// The task is an `AppEntity` parameter rather than a title string on purpose.
/// A shortcut built today has to still complete *the same task* next month,
/// and a title is not an identity — two tasks can share one, and renaming one
/// would quietly re-point the automation. The entity carries the core's
/// `EntityRef`, which is the only thing that does not drift.
struct CompleteTaskIntent: AppIntent {
    /// Catalog keys as literals; see the note on ``CaptureTaskIntent/title``.
    static let title: LocalizedStringResource = LocalizedStringResource(
        "apple.intents.complete_task.title",
        table: "Localizable"
    )

    static let description = IntentDescription(
        LocalizedStringResource("apple.intents.complete_task.description", table: "Localizable"),
        categoryName: LocalizedStringResource("apple.intents.category_tasks", table: "Localizable"),
        searchKeywords: ["done", "finish", "complete", "check off"]
    )

    /// Runs without pulling Sunrise forward. Every one of these verbs is
    /// something you do *instead of* opening the app; a capture that stole
    /// focus would interrupt exactly the thought it exists to catch.
    static let supportedModes: IntentModes = .background

    static var parameterSummary: some ParameterSummary {
        Summary("Complete \(\.$task) in Sunrise")
    }

    @Parameter(
        title: LocalizedStringResource("apple.intents.task_parameter", table: "Localizable"),
        requestValueDialog: IntentDialog(
            LocalizedStringResource("apple.intents.complete_task.task_prompt", table: "Localizable")
        )
    )
    var task: TaskEntity

    init() {}

    init(task: TaskEntity) {
        self.task = task
    }

    func perform() async throws -> some IntentResult & ReturnsValue<TaskEntity> & ProvidesDialog {
        let target = task.id
        let done = try await IntentVault.withVault { bridge in
            try await Self.complete(target, in: bridge)
        }
        return .result(value: done.task, dialog: IntentDialog("\(done.message)"))
    }

    /// What one completion produced.
    struct Completed: Sendable, Equatable {
        let task: TaskEntity
        let message: String
    }

    /// The whole of the work, with the vault passed in — see
    /// ``CaptureTaskIntent/capture(_:into:)`` for why it is shaped this way.
    static func complete(_ id: EntityRef, in bridge: CoreBridge) async throws -> Completed {
        do {
            // Read first, so a task deleted between being picked in Shortcuts
            // and the shortcut running is reported rather than written to.
            let item = try await TaskLookup.read(id, in: bridge)
            guard item.state != .done else {
                return Completed(
                    task: TaskEntity(item),
                    message: L10n.Intents.CompleteTask.alreadyDone(title: item.title)
                )
            }
            _ = try await bridge.submit(.completeTask(id: id))
            let released = try await releasedCount(of: id, in: bridge)
            return Completed(
                task: TaskEntity(id: item.id, title: item.title, isCompleted: true),
                message: message(title: item.title, released: released)
            )
        } catch {
            throw IntentError.wrapping(error)
        }
    }

    /// How many tasks this completion unblocked.
    ///
    /// Asked of the core after the fact rather than derived here: which tasks
    /// are *still* blocked is a graph question, and the completing surface
    /// cannot answer it — the same reason `FocusModel` reads it rather than
    /// computing it.
    private static func releasedCount(of id: EntityRef, in bridge: CoreBridge) async throws -> Int {
        guard case let .unblockCascade(cascade) = try await bridge.query(
            .unblockCascade(task: id)
        ) else { return 0 }
        return cascade.released.count
    }

    static func message(title: String, released: Int) -> String {
        released == 0
            ? L10n.Intents.CompleteTask.completed(title: title)
            : L10n.Intents.CompleteTask.completedUnblocked(count: released, title: title)
    }
}
