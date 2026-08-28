import Foundation

/// The Streams and Contexts sidebar, and CRUD over both.
///
/// Reads `StreamList` and `Contexts`, which already carry the counts a sidebar
/// wants — so listing never costs a follow-up query per row.
///
/// Every mutation goes through `submitUndoable`: a mis-renamed stream is
/// exactly the kind of thing someone wants back, and a deleted one is exactly
/// the kind of thing they cannot have back. The seam is what knows the
/// difference; this model only reports it.
@MainActor
@Observable
final class BrowseModel {
    private(set) var streams: [StreamListRow] = []
    private(set) var contexts: [ContextListRow] = []
    private(set) var errorMessage: String?
    /// Set when the last write could not be recorded for undo. Not an error —
    /// the write happened — so it is worded as a note, not a failure.
    private(set) var undoNote: String?
    /// Whether archived streams and contexts are shown. Archiving is not
    /// deletion: an archived context stays on every task that carries it, so
    /// there has to be a way to see one again.
    var showsArchived = false {
        didSet { Task { await refresh() } }
    }

    private let bridge: CoreBridge

    init(bridge: CoreBridge) {
        self.bridge = bridge
    }

    /// The Inbox row, which cannot be renamed, recoloured or deleted.
    ///
    /// The id comes from the seam rather than from a literal here: the
    /// alternative is comparing against a display name, which is translatable.
    static let inboxID: EntityRef = inboxStreamId()

    var visibleStreams: [StreamListRow] {
        showsArchived ? streams : streams.filter { !$0.archived }
    }

    var visibleContexts: [ContextListRow] {
        showsArchived ? contexts : contexts.filter { !$0.archived }
    }

    func refresh() async {
        do {
            if case let .streams(rows) = try await bridge.query(.streamList) {
                streams = rows
            }
            if case let .contexts(rows) = try await bridge.query(.contexts) {
                contexts = rows
            }
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// Follow the change stream for as long as the sidebar is on screen.
    ///
    /// Re-queries on every batch, complete or not. After a lag the ids are not
    /// the whole story, and a sidebar that patched only the rows it was told
    /// about would show the wrong open-task counts until something else
    /// happened to change.
    func follow() async {
        for await batch in await bridge.changes() {
            guard !batch.isClosed else { return }
            await refresh()
        }
    }

    // MARK: - Streams

    func createStream(name: String, color: StreamColor?) async {
        let draft = StreamDraftIn(
            name: name.trimmed,
            description: nil,
            color: color,
            parentId: nil,
            reviewCadence: nil,
            reminderLeadS: nil
        )
        await run(.createStream(draft: draft), label: "new stream “\(name.trimmed)”")
    }

    func updateStream(_ row: StreamListRow, _ edit: StreamEdit) async {
        await run(.updateStream(id: row.id, edit: edit), label: "edit “\(row.name)”")
    }

    func setStreamArchived(_ row: StreamListRow, _ archived: Bool) async {
        var edit = StreamEdit()
        edit.archived = archived
        await run(
            .updateStream(id: row.id, edit: edit),
            label: archived ? "archive “\(row.name)”" : "unarchive “\(row.name)”"
        )
    }

    func setStreamPaused(_ row: StreamListRow, _ paused: Bool) async {
        var edit = StreamEdit()
        edit.paused = paused
        await run(
            .updateStream(id: row.id, edit: edit),
            label: paused ? "pause “\(row.name)”" : "resume “\(row.name)”"
        )
    }

    func deleteStream(_ row: StreamListRow) async {
        await run(.deleteStream(id: row.id), label: "delete “\(row.name)”")
    }

    // MARK: - Contexts

    func createContext(name: String, description: String?) async {
        let draft = ContextDraftIn(name: name.trimmed, description: description?.nilIfBlank)
        await run(.createContext(draft: draft), label: "new context @\(name.trimmed)")
    }

    func updateContext(_ row: ContextListRow, _ edit: ContextEdit) async {
        await run(.updateContext(id: row.id, edit: edit), label: "edit @\(row.name)")
    }

    func setContextArchived(_ row: ContextListRow, _ archived: Bool) async {
        var edit = ContextEdit()
        edit.archived = archived
        await run(
            .updateContext(id: row.id, edit: edit),
            label: archived ? "archive @\(row.name)" : "unarchive @\(row.name)"
        )
    }

    /// Delete a context. The core removes it from every task carrying it in
    /// the same transaction, which is why this is a heavier action than
    /// archiving and is offered behind a confirmation.
    func deleteContext(_ row: ContextListRow) async {
        await run(.deleteContext(id: row.id), label: "delete @\(row.name)")
    }

    // MARK: - Writing

    func dismissUndoNote() { undoNote = nil }

    private func run(_ command: CoreCommand, label: String) async {
        do {
            let outcome = try await bridge.submitUndoable(command, label: label)
            // Only a *delete* is worth saying out loud. A create has no
            // inverse either — undoing one would mean deleting it, which the
            // core cannot reverse — but there is nothing to warn about, and a
            // banner on every "New stream" would train people to ignore the
            // one that matters. For those, a greyed-out Undo menu is the
            // honest signal: it offers nothing, and claims nothing.
            undoNote = outcome.notUndoable == .deleted
                ? undoRefusalExplanation(refusal: .deleted)
                : nil
            await refresh()
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}

extension String {
    /// `nil` for a string that is only whitespace, so an empty field clears a
    /// value rather than storing a blank one.
    var nilIfBlank: String? { trimmed.isEmpty ? nil : trimmed }
}
