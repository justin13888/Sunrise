import AppIntents
import Foundation

/// A Stream, as Shortcuts, Siri and the Focus settings see it.
///
/// Thin for the reason ``TaskEntity`` is: the system stores an entity inside a
/// shortcut's definition, or a Focus filter's, and hands it back months later.
/// The id is the only field this app can promise will still mean the same
/// thing then. The name is for a human to recognise, never to address by.
struct StreamEntity: AppEntity, Equatable {
    static let typeDisplayRepresentation = TypeDisplayRepresentation(name: "Stream")
    static let defaultQuery = StreamEntityQuery()

    /// The core's `EntityRef`.
    let id: EntityRef
    let name: String

    var displayRepresentation: DisplayRepresentation {
        DisplayRepresentation(title: "\(name)")
    }

    init(id: EntityRef, name: String) {
        self.id = id
        self.name = name
    }

    init(_ row: StreamListRow) {
        self.init(id: row.id, name: row.name)
    }
}

/// Reading the stream list, for the intents and the query that need it.
enum StreamLookup {
    /// Every stream a picker should offer, Inbox first, in the core's order.
    ///
    /// Archived streams are left out. Picking one for a Focus filter or a
    /// summary would scope a surface to a stream nothing is filed into.
    static func all(in bridge: CoreBridge) async throws -> [StreamListRow] {
        guard case let .streams(rows) = try await bridge.query(.streamList) else { return [] }
        return rows.filter { !$0.archived }
    }

    /// One stream by id, or ``IntentError/streamNotFound(_:)``.
    ///
    /// Read from the list rather than by `EntityById`, because the list row is
    /// the one that carries the open count a summary reports.
    static func row(_ id: EntityRef, in bridge: CoreBridge) async throws -> StreamListRow {
        let rows: [StreamListRow]
        do {
            rows = try await all(in: bridge)
        } catch {
            throw IntentError.wrapping(error)
        }
        guard let row = rows.first(where: { $0.id == id }) else {
            throw IntentError.streamNotFound(id)
        }
        return row
    }
}

/// How the system finds streams: by id, by typed name, and by listing them.
///
/// **This query opens the vault for suggestions, which ``TaskEntityQuery`` does
/// not.** A task picker can be empty while the vault is closed, because typing a
/// title still finds the task. A stream picker has no such fallback: the Focus
/// settings page offers the list and nothing else, and an empty list would make
/// the filter impossible to configure. Reading a list of names is bounded work,
/// and somebody opened the picker to see it.
struct StreamEntityQuery: EntityStringQuery {
    /// Resolve ids a shortcut or a Focus filter recorded earlier.
    ///
    /// A stream deleted or archived since is dropped rather than faked; the
    /// intent that receives the short list is the one that reports it.
    ///
    /// **One fallback, for the Focus filter.** iOS resolves the filter's
    /// streams when the Focus changes, which can be before the first unlock
    /// after a restart, when the vault cannot be opened. Failing there would
    /// leave the previous filter in force — or none — so the streams the filter
    /// was configured with are answered from ``FocusFilterStore``'s record of
    /// them instead. Anything that record does not name still fails.
    func entities(for identifiers: [StreamEntity.ID]) async throws -> [StreamEntity] {
        do {
            return try await IntentVault.withVault { bridge in
                let wanted = Set(identifiers)
                return try await StreamLookup.all(in: bridge)
                    .filter { wanted.contains($0.id) }
                    .map(StreamEntity.init)
            }
        } catch {
            let remembered = await FocusFilterStore.shared.names
            let known = identifiers.compactMap { id in
                remembered[id].map { StreamEntity(id: id, name: $0) }
            }
            guard !known.isEmpty else { throw error }
            return known
        }
    }

    /// Streams whose name contains `string`, ignoring case and accents.
    func entities(matching string: String) async throws -> [StreamEntity] {
        try await IntentVault.withVault { bridge in
            try await StreamLookup.all(in: bridge)
                .filter { $0.name.localizedStandardContains(string) }
                .map(StreamEntity.init)
        }
    }

    func suggestedEntities() async throws -> [StreamEntity] {
        try await IntentVault.withVault { bridge in
            try await StreamLookup.all(in: bridge).map(StreamEntity.init)
        }
    }
}
