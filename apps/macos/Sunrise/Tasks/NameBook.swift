import Foundation

/// Display names for the ids a task carries.
///
/// A `TaskItem` references its stream and contexts by id, because that is what
/// converges; the names live on the entities themselves. One lookup table,
/// refreshed with the list, beats a per-row query — and it is the difference
/// between an N-row list costing one read and costing 2N.
struct NameBook: Equatable, Sendable {
    var streams: [EntityRef: String] = [:]
    var contexts: [EntityRef: String] = [:]

    func stream(_ id: EntityRef) -> String? { streams[id] }

    /// Context names in the order the task carries them, skipping any the book
    /// has not heard of — a context deleted between the two reads is not worth
    /// showing as a raw id.
    func contextNames(_ ids: [EntityRef]) -> [String] {
        ids.compactMap { contexts[$0] }
    }

    /// Read both lists from the core.
    static func load(from bridge: CoreBridge) async -> NameBook {
        var book = NameBook()
        if case let .streams(rows)? = try? await bridge.query(.streamList) {
            book.streams = Dictionary(uniqueKeysWithValues: rows.map { ($0.id, $0.name) })
        }
        if case let .contexts(rows)? = try? await bridge.query(.contexts) {
            book.contexts = Dictionary(uniqueKeysWithValues: rows.map { ($0.id, $0.name) })
        }
        return book
    }
}
