import Foundation
import Testing

@testable import Sunrise

/// Undo and redo, through the app's own writing paths.
@MainActor
struct UndoModelTests {
    @Test
    func undoNamesTheStepItWouldReverse() async throws {
        let vault = try await TestVault()
        let browse = BrowseModel(bridge: vault.bridge)
        let undo = UndoModel(bridge: vault.bridge)
        await undo.refresh()
        #expect(!undo.canUndo)
        #expect(undo.undoTitle == "Undo")

        await browse.createStream(name: "Travel", color: .emerald)
        let row = try #require(browse.visibleStreams.first { $0.name == "Travel" })
        var edit = StreamEdit()
        edit.name = "Trips"
        await browse.updateStream(row, edit)
        await undo.refresh()

        #expect(undo.canUndo)
        #expect(undo.undoTitle == "Undo edit “Travel”")
        await vault.bridge.shutdown()
    }

    /// A create *is* undoable, and undoing one deletes what it minted — see
    /// commit `056c34d`, which added the inverse the seam previously refused
    /// to build. No banner either way: a warning on every "New stream" would
    /// train people to ignore the one that matters.
    @Test
    func undoingACreateDeletesWhatItMinted() async throws {
        let vault = try await TestVault()
        let browse = BrowseModel(bridge: vault.bridge)
        let undo = UndoModel(bridge: vault.bridge)

        await browse.createStream(name: "Travel", color: .emerald)
        await undo.refresh()

        #expect(undo.canUndo)
        #expect(undo.undoTitle == "Undo new stream “Travel”")
        #expect(browse.undoNote == nil)

        await undo.undo()
        await browse.refresh()
        #expect(!browse.visibleStreams.contains { $0.name == "Travel" })
        await vault.bridge.shutdown()
    }

    /// Undo is a new write. This checks it reaches storage, not just the
    /// stack.
    @Test
    func undoingARenameRestoresTheOldName() async throws {
        let vault = try await TestVault()
        let browse = BrowseModel(bridge: vault.bridge)
        let undo = UndoModel(bridge: vault.bridge)
        await browse.createStream(name: "Travel", color: .emerald)
        let row = try #require(browse.visibleStreams.first { $0.name == "Travel" })
        var edit = StreamEdit()
        edit.name = "Trips"
        await browse.updateStream(row, edit)

        await undo.undo()
        await browse.refresh()

        #expect(browse.visibleStreams.contains { $0.name == "Travel" })
        #expect(undo.lastAction == "Undid edit “Travel”")
        await vault.bridge.shutdown()
    }

    @Test
    func redoPutsTheChangeBack() async throws {
        let vault = try await TestVault()
        let browse = BrowseModel(bridge: vault.bridge)
        let undo = UndoModel(bridge: vault.bridge)
        await browse.createStream(name: "Travel", color: .emerald)
        let row = try #require(browse.visibleStreams.first { $0.name == "Travel" })
        var edit = StreamEdit()
        edit.name = "Trips"
        await browse.updateStream(row, edit)
        await undo.undo()

        await undo.refresh()
        #expect(undo.canRedo)
        await undo.redo()
        await browse.refresh()

        #expect(browse.visibleStreams.contains { $0.name == "Trips" })
        await vault.bridge.shutdown()
    }

    /// A delete never reaches the stack, so the menu offers nothing for it.
    /// The alternative — an Undo item that silently does nothing — is the one
    /// thing this whole path exists not to do.
    @Test
    func aDeleteLeavesNothingToUndo() async throws {
        let vault = try await TestVault()
        let browse = BrowseModel(bridge: vault.bridge)
        let undo = UndoModel(bridge: vault.bridge)
        await browse.createContext(name: "errands", description: nil)
        let row = try #require(browse.visibleContexts.first)

        await browse.deleteContext(row)
        await undo.refresh()

        // The create before it is still the top of the stack, which is how
        // this reads that the delete pushed nothing of its own.
        #expect(undo.undoTitle == "Undo new context @errands")
        #expect(browse.undoNote?.lowercased().contains("tombstone") == true)
        await vault.bridge.shutdown()
    }
}

/// Saved views, against a scratch file rather than the user's real one.
@MainActor
struct SavedViewsModelTests {
    @Test
    func savingAndRecallingRoundTripsThroughTheFile() async throws {
        let (model, cleanup) = scratchStore()
        defer { cleanup() }

        await model.save(
            name: "errands",
            destination: .search,
            query: "passport",
            contexts: []
        )
        await model.load()

        let view = try #require(model.views.first)
        #expect(view.name == "errands")
        #expect(view.view == .search)
        #expect(view.query == "passport")
        // The summary is the store's rendering, not this client's.
        #expect(view.summary == "search · /passport")
    }

    /// Contexts are stored by name, and resolved against the vault on recall.
    @Test
    func recallingAppliesTheContextFilterByName() async throws {
        let (model, cleanup) = scratchStore()
        defer { cleanup() }
        var book = NameBook()
        book.contexts = ["ctx_a": "deep-work"]

        await model.save(
            name: "deep",
            destination: .list(.todayAll),
            query: "",
            contexts: ["deep-work"]
        )
        await model.load()

        let destination = model.recall(try #require(model.views.first), contexts: book)
        #expect(destination == .list(.today(contexts: ["ctx_a"])))
        #expect(model.recallNote == nil)
    }

    /// A context that no longer exists is reported, not silently applied as an
    /// invisible filter that empties the list.
    @Test
    func aVanishedContextIsReportedRatherThanSilentlyFiltering() async throws {
        let (model, cleanup) = scratchStore()
        defer { cleanup() }

        await model.save(
            name: "deep",
            destination: .list(.todayAll),
            query: "",
            contexts: ["deep-work"]
        )
        await model.load()

        let destination = model.recall(try #require(model.views.first), contexts: NameBook())
        #expect(destination == .list(.todayAll), "the view still opens")
        let note = try #require(model.recallNote)
        #expect(note.contains("no longer exist"), "\(note)")
    }

    @Test
    func aMalformedLineCostsThatViewAndNothingElse() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appending(path: "sunrise-views-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let path = directory.appending(path: "views.toml")
        try "good = \"view=today\"\nbad = \"view=nonesuch\"\n".write(
            to: path, atomically: true, encoding: .utf8
        )

        let model = SavedViewsModel(store: SavedViews.atPath(path: path.path(percentEncoded: false)))
        await model.load()

        #expect(model.views.map(\.name) == ["good"])
        #expect(model.warnings.count == 1)
    }

    @Test
    func savingTheSameNameTwiceReplacesIt() async throws {
        let (model, cleanup) = scratchStore()
        defer { cleanup() }

        await model.save(name: "one", destination: .review, query: "", contexts: [])
        await model.save(name: "one", destination: .focus, query: "", contexts: [])

        #expect(model.views.count == 1)
        #expect(model.views.first?.view == .focus)

        await model.delete(try #require(model.views.first))
        #expect(model.views.isEmpty)
    }

    /// Only a search view carries its query. Saving one from Review and
    /// recalling it must not resurrect a search string nothing will use.
    @Test
    func onlyASearchViewKeepsItsQuery() async throws {
        let (model, cleanup) = scratchStore()
        defer { cleanup() }

        await model.save(name: "week", destination: .review, query: "passport", contexts: [])
        await model.load()

        #expect(model.views.first?.query.isEmpty == true)
    }

    /// The two daily briefs. `View::Morning` and `View::Evening` exist upstream
    /// now, so "Save this view…" on either of them has to write a view that
    /// recalls as itself — the menu item used to be greyed out here because
    /// `Destination.primary` answered `nil`.
    @Test
    func theMorningBriefSavesAndRecallsAsItself() async throws {
        try await briefRoundTrips(.morning, as: .morning)
    }

    @Test
    func theEveningBriefSavesAndRecallsAsItself() async throws {
        try await briefRoundTrips(.evening, as: .evening)
    }

    private func briefRoundTrips(
        _ destination: Destination,
        as primary: PrimaryView
    ) async throws {
        let (model, cleanup) = scratchStore()
        defer { cleanup() }

        await model.save(name: "brief", destination: destination, query: "", contexts: [])
        await model.load()

        let view = try #require(model.views.first)
        #expect(view.view == primary)
        #expect(model.recall(view, contexts: NameBook()) == destination)
        #expect(model.errorMessage == nil, "a brief is not a refusal any more")
    }

    /// `Destination.primary` is total, and the sidebar is the list that proves
    /// it: anything the user can be looking at, they can save. A view added to
    /// `Destination.fixed` without a `PrimaryView` behind it fails here rather
    /// than as a menu item that silently does nothing.
    @Test
    func everySidebarDestinationCanBeSaved() async throws {
        let (model, cleanup) = scratchStore()
        defer { cleanup() }

        for destination in Destination.fixed {
            await model.save(
                name: "v-\(destination.title)",
                destination: destination,
                query: "",
                contexts: []
            )
        }
        await model.load()

        #expect(model.views.count == Destination.fixed.count)
        #expect(model.warnings.isEmpty)
        #expect(model.errorMessage == nil)
    }

    private func scratchStore() -> (SavedViewsModel, () -> Void) {
        let directory = FileManager.default.temporaryDirectory
            .appending(path: "sunrise-views-\(UUID().uuidString)")
        let path = directory.appending(path: "views.toml")
        let model = SavedViewsModel(
            store: SavedViews.atPath(path: path.path(percentEncoded: false))
        )
        return (model, { try? FileManager.default.removeItem(at: directory) })
    }
}
