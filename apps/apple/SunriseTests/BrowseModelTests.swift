import Foundation
import Testing

@testable import Sunrise

/// Browse against a real vault: the sidebar's two lists, and CRUD over both.
@MainActor
struct BrowseModelTests {
    @Test
    func aFreshVaultShowsTheInboxAndNothingElse() async throws {
        let vault = try await TestVault()
        let model = BrowseModel(bridge: vault.bridge)

        await model.refresh()

        #expect(model.visibleStreams.count == 1)
        let inbox = try #require(model.visibleStreams.first)
        #expect(inbox.id == BrowseModel.inboxID, "the Inbox id comes from the seam")
        #expect(model.visibleContexts.isEmpty)
        await vault.bridge.shutdown()
    }

    @Test
    func creatingAStreamPutsItInTheSidebarWithItsColour() async throws {
        let vault = try await TestVault()
        let model = BrowseModel(bridge: vault.bridge)

        await model.createStream(name: "Travel", color: .emerald)

        let row = try #require(model.visibleStreams.first { $0.name == "Travel" })
        #expect(row.color == .emerald)
        #expect(row.openTaskCount == 0)
        #expect(model.errorMessage == nil)
        await vault.bridge.shutdown()
    }

    /// A one-field edit must not disturb the fields it did not name. This is
    /// what the seam's `#[uniffi(default)]` on every `StreamEdit` field buys,
    /// exercised through the app's own writing path.
    @Test
    func renamingAStreamKeepsItsColour() async throws {
        let vault = try await TestVault()
        let model = BrowseModel(bridge: vault.bridge)
        await model.createStream(name: "Travel", color: .indigo)
        let row = try #require(model.visibleStreams.first { $0.name == "Travel" })

        var edit = StreamEdit()
        edit.name = "Trips"
        await model.updateStream(row, edit)

        let after = try #require(model.visibleStreams.first { $0.name == "Trips" })
        #expect(after.color == .indigo)
        await vault.bridge.shutdown()
    }

    /// The editor reads the **whole** stream, not the sidebar row.
    ///
    /// `StreamListRow` carries no review cadence, so an editor built on the
    /// row would submit whatever its picker defaulted to — silently resetting
    /// the cadence every time someone renamed a stream. This pins that the
    /// read exists and that a rename leaves the cadence alone.
    @Test
    func renamingAStreamDoesNotResetItsReviewCadence() async throws {
        let vault = try await TestVault()
        let model = BrowseModel(bridge: vault.bridge)
        await model.createStream(name: "Travel", color: .indigo, cadence: .weekly)
        let row = try #require(model.visibleStreams.first { $0.name == "Travel" })

        let loaded = try #require(await model.stream(row.id))
        #expect(loaded.reviewCadence == .weekly, "the editor can see what the row cannot")

        // Exactly what the editor submits when only the name was touched.
        var edit = StreamEdit()
        edit.name = "Trips"
        edit.color = loaded.color
        edit.reviewCadence = loaded.reviewCadence
        await model.updateStream(row, edit)

        let after = try #require(await model.stream(row.id))
        #expect(after.name == "Trips")
        #expect(after.reviewCadence == .weekly)
        #expect(after.color == .indigo)
        await vault.bridge.shutdown()
    }

    /// Archiving is not deletion, and the sidebar has to be able to show one
    /// again — otherwise archiving is a one-way trip with a friendlier name.
    @Test
    func anArchivedStreamLeavesTheSidebarAndComesBackOnDemand() async throws {
        let vault = try await TestVault()
        let model = BrowseModel(bridge: vault.bridge)
        await model.createStream(name: "Travel", color: nil)
        let row = try #require(model.visibleStreams.first { $0.name == "Travel" })

        await model.setStreamArchived(row, true)
        #expect(!model.visibleStreams.contains { $0.name == "Travel" })

        model.showsArchived = true
        await model.refresh()
        #expect(model.visibleStreams.contains { $0.name == "Travel" })
        await vault.bridge.shutdown()
    }

    @Test
    func pausingAStreamIsReflectedInTheRow() async throws {
        let vault = try await TestVault()
        let model = BrowseModel(bridge: vault.bridge)
        await model.createStream(name: "Travel", color: nil)
        let row = try #require(model.visibleStreams.first { $0.name == "Travel" })

        await model.setStreamPaused(row, true)

        #expect(model.visibleStreams.first { $0.name == "Travel" }?.paused == true)
        await vault.bridge.shutdown()
    }

    /// The core reports a delete as un-undoable, and the model surfaces that
    /// rather than leaving an Undo item that would silently do nothing.
    @Test
    func deletingSaysOutLoudThatItCannotBeUndone() async throws {
        let vault = try await TestVault()
        let model = BrowseModel(bridge: vault.bridge)
        await model.createStream(name: "Travel", color: nil)
        let row = try #require(model.visibleStreams.first { $0.name == "Travel" })

        await model.deleteStream(row)

        #expect(!model.visibleStreams.contains { $0.name == "Travel" })
        let note = try #require(model.undoNote)
        #expect(note.lowercased().contains("tombstone"), "\(note)")
        model.dismissUndoNote()
        #expect(model.undoNote == nil)
        await vault.bridge.shutdown()
    }

    /// Only a delete raises a note. An edit is undoable, and a create is not
    /// but has nothing to warn about — a banner on every one of those would
    /// train people to ignore the one that matters.
    @Test
    func anEditLeavesNoUndoNoteBecauseItIsUndoable() async throws {
        let vault = try await TestVault()
        let model = BrowseModel(bridge: vault.bridge)
        await model.createContext(name: "errands", description: nil)
        let row = try #require(model.visibleContexts.first)

        var edit = ContextEdit()
        edit.name = "chores"
        await model.updateContext(row, edit)

        #expect(model.undoNote == nil)
        #expect(model.visibleContexts.first?.name == "chores")
        await vault.bridge.shutdown()
    }

    @Test
    func aContextCountsTheTasksCarryingIt() async throws {
        let vault = try await TestVault()
        let model = BrowseModel(bridge: vault.bridge)
        await model.createContext(name: "errands", description: "out of the house")
        let context = try #require(model.visibleContexts.first)
        #expect(context.description == "out of the house")

        _ = try await vault.bridge.submit(
            .createTask(draft: TaskDraftIn(
                title: "Renew passport",
                body: nil,
                streamId: nil,
                contexts: [context.id],
                priority: nil,
                energy: nil,
                estimatedDurationS: nil,
                scheduledAt: nil,
                dueAt: nil,
                schedulingConstraints: [],
                assignee: nil,
                reminderLeadS: nil
            ))
        )
        await model.refresh()

        #expect(model.visibleContexts.first?.taskCount == 1)
        await vault.bridge.shutdown()
    }

    /// The stream list is reachable from the sidebar's own selection type, and
    /// the query behind it is the core's — not a filter this client invents.
    @Test
    func aStreamListShowsOnlyThatStreamsTasks() async throws {
        let vault = try await TestVault()
        let browse = BrowseModel(bridge: vault.bridge)
        await browse.createStream(name: "Travel", color: nil)
        let travel = try #require(browse.visibleStreams.first { $0.name == "Travel" })

        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(draft("Stays in the Inbox"))
        await list.show(.stream(id: travel.id, name: travel.name))
        await list.create(draft("Lands in Travel"))

        #expect(list.tasks.map(\.title) == ["Lands in Travel"])
        await list.show(.inbox)
        #expect(list.tasks.map(\.title) == ["Stays in the Inbox"])
        await vault.bridge.shutdown()
    }

    /// A context list is the one list a capture cannot land in: capture writes
    /// to a stream, and there is no annotation for "the context I am looking
    /// at". The bar is hidden rather than writing somewhere invisible.
    @Test
    func aContextListDoesNotOfferCapture() {
        #expect(TaskListKind.todayAll.acceptsCapture)
        #expect(TaskListKind.inbox.acceptsCapture)
        #expect(TaskListKind.stream(id: "str_x", name: "Travel").acceptsCapture)
        #expect(!TaskListKind.context(id: "ctx_x", name: "errands").acceptsCapture)
    }

    /// A filtered Today is a context list wearing Today's name, and it is
    /// refused for the same reason: the captured line carries none of the
    /// contexts the filter names, so the row would be written and filtered
    /// straight back out — written correctly, and invisible.
    @Test
    func aFilteredTodayDoesNotOfferCaptureEither() {
        #expect(!TaskListKind.today(contexts: ["ctx_errands"]).acceptsCapture)
        #expect(TaskListKind.today(contexts: []).acceptsCapture)
    }

    private func draft(_ title: String) -> TaskDraftIn {
        TaskDraftIn(
            title: title,
            body: nil,
            streamId: nil,
            contexts: [],
            priority: nil,
            energy: nil,
            estimatedDurationS: nil,
            scheduledAt: nil,
            dueAt: nil,
            schedulingConstraints: [],
            assignee: nil,
            reminderLeadS: nil
        )
    }
}
