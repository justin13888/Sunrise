import Foundation
import Testing

@testable import Sunrise

/// Turning a sidebar drag into `sort_order` writes.
///
/// The arithmetic is the core's — these pin what this client does *with* it:
/// which rows it rewrites, and which it leaves alone.
struct StreamOrderTests {
    private func row(_ name: String, _ key: String) -> StreamListRow {
        StreamListRow(
            id: "str_\(name)",
            name: name,
            color: .slate,
            openTaskCount: 0,
            archived: false,
            paused: false,
            sortOrder: key
        )
    }

    private var rows: [StreamListRow] {
        [row("a", "B"), row("b", "M"), row("c", "X")]
    }

    /// The property the whole scheme exists for: dragging one row is one
    /// write. A list of a hundred streams costs the same as a list of three.
    @Test
    func onlyTheDraggedRowIsRewritten() throws {
        let moves = StreamOrder.moves(moving: IndexSet(integer: 2), to: 0, in: rows)
        #expect(moves.map(\.id) == ["str_c"])
        // Above the first row, so below its key.
        let key = try #require(moves.first?.key)
        #expect(key < "B")
    }

    @Test
    func aRowDraggedToTheEndLandsAfterTheLastKey() {
        let moves = StreamOrder.moves(moving: IndexSet(integer: 0), to: 3, in: rows)
        #expect(moves.map(\.id) == ["str_a"])
        #expect((moves.first?.key ?? "") > "X")
    }

    @Test
    func aRowDraggedIntoTheMiddleLandsBetweenItsNewNeighbours() {
        let moves = StreamOrder.moves(moving: IndexSet(integer: 0), to: 2, in: rows)
        let key = moves.first?.key ?? ""
        #expect(key > "B" && key < "X", "expected a key between b and c, got \(key)")
    }

    /// A drop that puts a row back where it already was writes nothing, so it
    /// costs no command and leaves no undo entry.
    @Test
    func aDropThatChangesNothingWritesNothing() {
        #expect(StreamOrder.moves(moving: IndexSet(integer: 1), to: 1, in: rows).isEmpty)
        #expect(StreamOrder.moves(moving: IndexSet(integer: 1), to: 2, in: rows).isEmpty)
        #expect(StreamOrder.moves(moving: IndexSet(), to: 0, in: rows).isEmpty)
    }

    /// A ⇧-selected run keeps its internal order and takes successive keys, so
    /// the rows land next to each other rather than on top of each other.
    @Test
    func aMultiRowDragTakesSuccessiveKeys() {
        let moves = StreamOrder.moves(moving: IndexSet([0, 1]), to: 3, in: rows)
        #expect(moves.map(\.id) == ["str_a", "str_b"])
        let keys = moves.map(\.key)
        #expect(keys.count == 2)
        #expect(keys[0] > "X", "the run must land after the row it was dropped past")
        #expect(keys[1] > keys[0], "two rows in one drag must not share a key")
    }

    /// Bounds that admit nothing between them mean the list this was
    /// calculated against has moved on. Writing *something* would put a row
    /// where nobody dropped it, so nothing is written at all — not even the
    /// rows whose keys were computed before the failure.
    @Test
    func anUncomputableKeyAbandonsTheWholeDragRatherThanHalfOfIt() {
        let moves = StreamOrder.moves(
            moving: IndexSet([0, 1]),
            to: 3,
            in: rows,
            key: { _, _ in nil }
        )
        #expect(moves.isEmpty)
    }

    /// The core is what knows how to make a key; this is the seam call the
    /// sidebar makes, checked for the two answers it can give.
    @Test
    func theSeamMakesKeysAndRefusesImpossibleOnes() throws {
        let first = try #require(streamSortKeyBetween(after: nil, before: nil))
        #expect(!first.isEmpty)

        let between = try #require(streamSortKeyBetween(after: "B", before: "C"))
        #expect(between > "B" && between < "C", "adjacent keys must lengthen, got \(between)")

        // Equal, reversed, and not a key at all.
        #expect(streamSortKeyBetween(after: "M", before: "M") == nil)
        #expect(streamSortKeyBetween(after: "X", before: "B") == nil)
        #expect(streamSortKeyBetween(after: "a0", before: nil) == nil)
    }
}

/// Reordering the sidebar against a real vault.
@MainActor
struct StreamReorderTests {
    private func names(_ model: BrowseModel) -> [String] {
        model.orderableStreams.map(\.name)
    }

    /// A new stream is appended, and a dragged one moves — through the core,
    /// so the order is in the vault rather than in this process.
    @Test
    func draggingAStreamReordersTheVaultAndNotJustTheView() async throws {
        let vault = try await TestVault()
        let model = BrowseModel(bridge: vault.bridge)
        for name in ["Alpha", "Bravo", "Charlie"] {
            await model.createStream(name: name, color: nil)
        }
        #expect(names(model) == ["Alpha", "Bravo", "Charlie"], "new streams append")

        #expect(await model.moveStreams(from: IndexSet(integer: 2), to: 0))
        #expect(names(model) == ["Charlie", "Alpha", "Bravo"])

        // A second model over the same vault reads the same order back. If the
        // arrangement were kept in this object, or in `UserDefaults`, this is
        // where it would show.
        let other = BrowseModel(bridge: vault.bridge)
        await other.refresh()
        #expect(names(other) == ["Charlie", "Alpha", "Bravo"])
        await vault.bridge.shutdown()
    }

    /// The Inbox is not a Stream: it has no entity, no key, and no position to
    /// write. Keeping it out of the orderable list is what stops a drag from
    /// producing a command the core would reject.
    @Test
    func theInboxIsNotPartOfTheOrder() async throws {
        let vault = try await TestVault()
        let model = BrowseModel(bridge: vault.bridge)
        await model.createStream(name: "Travel", color: nil)

        #expect(model.inboxStream?.id == BrowseModel.inboxID)
        #expect(!model.orderableStreams.contains { $0.id == BrowseModel.inboxID })
        #expect(model.visibleStreams.first?.id == BrowseModel.inboxID, "pinned to the top")
        #expect(model.inboxStream?.sortOrder.isEmpty == true, "no position, not first position")
        await vault.bridge.shutdown()
    }

    /// A reorder is an ordinary stream edit, so it undoes like a rename — and
    /// it undoes without touching the streams that never moved.
    @Test
    func aReorderIsUndoable() async throws {
        let vault = try await TestVault()
        let model = BrowseModel(bridge: vault.bridge)
        for name in ["Alpha", "Bravo"] {
            await model.createStream(name: name, color: nil)
        }
        await model.moveStreams(from: IndexSet(integer: 1), to: 0)
        #expect(names(model) == ["Bravo", "Alpha"])

        _ = try await vault.bridge.undo()
        await model.refresh()
        #expect(names(model) == ["Alpha", "Bravo"])
        await vault.bridge.shutdown()
    }
}
