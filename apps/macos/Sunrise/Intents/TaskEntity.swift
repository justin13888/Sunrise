import AppIntents
import Foundation

/// A task, as Shortcuts and Siri see it.
///
/// The projection is deliberately thin — an id, a title, and whether it is
/// finished. An `AppEntity` is what the system stores inside a shortcut's
/// definition and hands back weeks later, so every field on it is a promise
/// that it will still mean the same thing then; the id is the only field this
/// app can keep that promise about, and it is the only one the intents act on.
/// Titles here are for a human to recognise, never to address by.
struct TaskEntity: AppEntity, Equatable {
    static let typeDisplayRepresentation = TypeDisplayRepresentation(name: "Task")
    static let defaultQuery = TaskEntityQuery()

    /// The core's `EntityRef`, which is already a string.
    let id: EntityRef
    let title: String
    let isCompleted: Bool

    var displayRepresentation: DisplayRepresentation {
        DisplayRepresentation(title: "\(title)")
    }

    init(id: EntityRef, title: String, isCompleted: Bool) {
        self.id = id
        self.title = title
        self.isCompleted = isCompleted
    }

    init(_ item: TaskItem) {
        self.init(id: item.id, title: item.title, isCompleted: item.state == .done)
    }
}

/// Reading one task by id, for the intents that act on one.
enum TaskLookup {
    /// The task `id` names, or ``IntentError/taskNotFound(_:)``.
    ///
    /// The core answers a missing id with an *error* — `EngineError::NotFound`,
    /// flattened to `BindingError::Core` at the seam — rather than with an
    /// empty result, so "gone" arrives here as a throw. For a shortcut built
    /// weeks ago against a task since finished with, that is the ordinary
    /// case and not a fault, which is why it becomes one sentence the user can
    /// act on rather than the core's own words. Anything else that could make
    /// a single-row read fail would already have failed
    /// ``IntentVault/withVault(_:)`` before this ran.
    ///
    /// A tombstoned row is treated the same. It still reads back — `deleted`
    /// is a flag, not an absence — and completing something the user deleted
    /// would be the automation overruling them.
    static func read(_ id: EntityRef, in bridge: CoreBridge) async throws -> TaskItem {
        let result: CoreQueryResult
        do {
            result = try await bridge.query(.entityById(id: id))
        } catch {
            throw IntentError.taskNotFound(id)
        }
        guard case let .task(item) = result, !item.deleted else {
            throw IntentError.taskNotFound(id)
        }
        return item
    }
}

/// How the system finds tasks: by id, by typed text, and by suggestion.
///
/// `EntityStringQuery` rather than plain `EntityQuery` because the string half
/// is what makes "complete *buy milk*" work by voice — without it Siri can
/// only offer a list to point at, which is not something you can do hands-free.
struct TaskEntityQuery: EntityStringQuery {
    /// Row cap for a picker or a voice match. Past this, a list stops being
    /// something a person chooses from.
    static let limit: UInt32 = 25

    /// Resolve ids a shortcut recorded earlier.
    ///
    /// A task deleted since is dropped rather than faked. The intent that
    /// receives the short list is the one that reports it, because only it
    /// knows what the user was trying to do.
    func entities(for identifiers: [TaskEntity.ID]) async throws -> [TaskEntity] {
        try await IntentVault.withVault { bridge in
            var found: [TaskEntity] = []
            for id in identifiers {
                // Per id, so one stale entry in a shortcut's saved list does
                // not lose the rest of it.
                guard let item = try? await TaskLookup.read(id, in: bridge) else { continue }
                found.append(TaskEntity(item))
            }
            return found
        }
    }

    /// Full-text search, through the core's own `Query::Search`.
    func entities(matching string: String) async throws -> [TaskEntity] {
        try await IntentVault.withVault { bridge in
            let result = try await bridge.query(.search(text: string, limit: Self.limit))
            guard case let .tasks(rows) = result else { return [] }
            return TaskListReader.open(from: rows)
        }
    }

    /// What the picker shows before anything is typed.
    ///
    /// **This one never opens the vault.** The system asks for suggestions
    /// speculatively — while indexing, while drawing a shortcut tile — and
    /// launching the app to decrypt a vault for a picker nobody opened is
    /// exactly the background work an automation surface should not cause.
    /// With the vault closed there is honestly nothing to suggest, and an
    /// empty list says so.
    func suggestedEntities() async throws -> [TaskEntity] {
        guard let bridge = await IntentVault.existing() else { return [] }
        let now = await bridge.nowMs()
        let result = try await bridge.query(.today(nowMs: now, contexts: []))
        guard case let .tasks(rows) = result else { return [] }
        return Array(TaskListReader.open(from: rows).prefix(Int(Self.limit)))
    }
}
