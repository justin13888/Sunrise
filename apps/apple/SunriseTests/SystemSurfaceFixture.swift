import Foundation

@testable import Sunrise

/// Building the streams and tasks the system-surface suites read: the defer
/// and stream-summary intents, the Focus filter, the Live Activity mapping and
/// share filing.
enum SystemSurfaceFixture {
    /// Create a stream and return its id.
    static func stream(_ name: String, in bridge: CoreBridge) async throws -> EntityRef {
        try await bridge.submit(.createStream(draft: StreamDraftIn(
            name: name,
            description: nil,
            color: nil,
            parentId: nil,
            reviewCadence: nil,
            reminderLeadS: nil
        ))).entity
    }

    /// Create a task, optionally in a stream and scheduled, and return its id.
    static func task(
        _ title: String,
        stream: EntityRef? = nil,
        scheduledAtMs: Int64? = nil,
        in bridge: CoreBridge
    ) async throws -> EntityRef {
        try await bridge.submit(.createTask(draft: TaskDraftIn(
            title: title,
            body: nil,
            streamId: stream,
            contexts: [],
            priority: nil,
            energy: nil,
            estimatedDurationS: 1800,
            scheduledAt: scheduledAtMs.map { .instant(at: $0) },
            dueAt: nil,
            schedulingConstraints: [],
            assignee: nil,
            reminderLeadS: nil
        ))).entity
    }

    static func read(_ id: EntityRef, in bridge: CoreBridge) async throws -> TaskItem {
        try await TaskLookup.read(id, in: bridge)
    }

    static func inbox(_ bridge: CoreBridge) async throws -> [TaskItem] {
        guard case let .tasks(rows) = try await bridge.query(.inbox) else { return [] }
        return rows
    }

    /// A defaults suite of its own, so a test never reads or writes the Focus
    /// filter the app on this machine is using.
    static func scratchDefaults() -> (UserDefaults, String) {
        let name = "sunrise-tests-\(UUID().uuidString)"
        guard let defaults = UserDefaults(suiteName: name) else {
            fatalError("could not open a scratch defaults suite")
        }
        return (defaults, name)
    }

    static func discard(_ name: String) {
        UserDefaults.standard.removePersistentDomain(forName: name)
    }

    /// A scratch directory, removed by the caller.
    static func scratchDirectory() -> URL {
        FileManager.default.temporaryDirectory
            .appending(path: "sunrise-tests-\(UUID().uuidString)")
    }
}
