import Foundation
import Testing

@testable import Sunrise

/// Row commands, against a real vault.
///
/// The same function the key handler and the command palette both call, so
/// these assert what `X`, `D`, `S` and `M` actually do rather than what a view
/// body says they do.
@MainActor
struct ListCommandTests {
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

    /// A list of three, plus the selection and sheet state a window would own.
    private struct Fixture {
        let list: TaskListModel
        let selection: ListSelection
        let sheets: RowSheets
    }

    private func fixture(_ bridge: CoreBridge) async -> Fixture {
        let list = TaskListModel(bridge: bridge, kind: .inbox)
        for title in ["First", "Second", "Third"] {
            await list.create(draft(title))
        }
        let selection = ListSelection()
        selection.reconcile(with: list.ordered.map(\.id))
        return Fixture(list: list, selection: selection, sheets: RowSheets())
    }

    /// Nothing under the cursor means the press was not ours, so it can fall
    /// through to whatever SwiftUI would have done with it.
    @Test
    func aRowActionWithNothingSelectedIsNotHandled() async throws {
        let vault = try await TestVault()
        let scene = await fixture(vault.bridge)

        let handled = ListCommand.perform(
            .markDone,
            list: scene.list,
            selection: scene.selection,
            sheets: scene.sheets,
            escapes: ListEscapes()
        )

        #expect(!handled)
        await vault.bridge.shutdown()
    }

    /// `S` opens the schedule sheet on the ticked rows, and the sheet knows how
    /// many it is about.
    @Test
    func scheduleOpensASheetOverTheWholeSelection() async throws {
        let vault = try await TestVault()
        let scene = await fixture(vault.bridge)
        scene.selection.move(.down)
        scene.selection.extend(.down)

        let handled = ListCommand.perform(
            .schedule,
            list: scene.list,
            selection: scene.selection,
            sheets: scene.sheets,
            escapes: ListEscapes()
        )

        #expect(handled)
        #expect(scene.sheets.scheduling?.tasks.count == 2)
        await vault.bridge.shutdown()
    }

    /// `M` likewise, and the picker it opens is offered the vault's streams
    /// sorted by name rather than by id.
    @Test
    func moveOpensAPickerOverNamedStreams() async throws {
        let vault = try await TestVault()
        let scene = await fixture(vault.bridge)
        scene.selection.move(.down)

        #expect(
            ListCommand.perform(
                .moveToStream,
                list: scene.list,
                selection: scene.selection,
                sheets: scene.sheets,
                escapes: ListEscapes()
            )
        )
        #expect(scene.sheets.moving?.tasks.count == 1)
        #expect(!scene.list.streamChoices.isEmpty, "the Inbox is a stream and can be moved into")
        await vault.bridge.shutdown()
    }

    /// `Return` opens the editor on the cursor's row.
    @Test
    func returnOpensTheEditorOnTheCursorRow() async throws {
        let vault = try await TestVault()
        let scene = await fixture(vault.bridge)
        scene.selection.move(.down)
        let expected = try #require(scene.selection.cursor)

        #expect(
            ListCommand.perform(
                .openDetail,
                list: scene.list,
                selection: scene.selection,
                sheets: scene.sheets,
                escapes: ListEscapes()
            )
        )
        #expect(scene.sheets.editing?.id == expected)
        await vault.bridge.shutdown()
    }

    /// Movement is handled here rather than left to SwiftUI, so the model and
    /// the highlight cannot drift apart.
    @Test
    func movementRunsThroughTheSelectionModel() async throws {
        let vault = try await TestVault()
        let scene = await fixture(vault.bridge)

        for action in [AppAction.moveDown, .moveDown, .extendSelectionDown] {
            #expect(
                ListCommand.perform(
                    action,
                    list: scene.list,
                    selection: scene.selection,
                    sheets: scene.sheets,
                    escapes: ListEscapes()
                )
            )
        }

        #expect(scene.selection.targets.count == 2)
        await vault.bridge.shutdown()
    }

    /// An application chord that reached the list by mistake is refused, so it
    /// can still reach the menu item that owns it.
    @Test
    func applicationActionsAreLeftToTheWindow() async throws {
        let vault = try await TestVault()
        let scene = await fixture(vault.bridge)

        for action in [AppAction.undo, .redo, .today, .quickCapture] {
            #expect(
                !ListCommand.perform(
                    action,
                    list: scene.list,
                    selection: scene.selection,
                    sheets: scene.sheets,
                    escapes: ListEscapes()
                )
            )
        }
        await vault.bridge.shutdown()
    }

    /// `F` starts a session and asks the window for the Focus screen. Both
    /// halves matter: a timer running on a screen nobody was taken to is a
    /// timer nobody stops.
    @Test
    func focusStartsASessionAndAsksForTheScreen() async throws {
        let vault = try await TestVault()
        let scene = await fixture(vault.bridge)
        scene.selection.move(.down)

        let shown = Box()
        #expect(
            ListCommand.perform(
                .focusMode,
                list: scene.list,
                selection: scene.selection,
                sheets: scene.sheets,
                escapes: ListEscapes(showFocus: { shown.value = true })
            )
        )
        try await _Concurrency.Task.sleep(for: .milliseconds(200))

        #expect(shown.value)
        guard case let .focusSessions(sessions)? =
            try? await vault.bridge.query(.runningFocusSessions)
        else {
            Issue.record("expected a running session")
            return
        }
        #expect(sessions.count == 1)
        await vault.bridge.shutdown()
    }

    // MARK: - Batches

    /// `X` over a range completes every row in it, and re-reads once rather
    /// than once per row.
    @Test
    func completingASelectionCompletesEveryRowInIt() async throws {
        let vault = try await TestVault()
        let scene = await fixture(vault.bridge)
        scene.selection.selectAll()

        await scene.list.complete(scene.list.rows(for: scene.selection.targets))

        #expect(scene.list.tasks.filter { $0.state == .done }.count == 3)
        await vault.bridge.shutdown()
    }

    @Test
    func deferringASelectionMovesEveryRowInIt() async throws {
        let vault = try await TestVault()
        let scene = await fixture(vault.bridge)
        scene.selection.selectAll()

        await scene.list.defer_(scene.list.rows(for: scene.selection.targets), byDays: 1)

        #expect(scene.list.tasks.allSatisfy { $0.scheduledAt != nil })
        #expect(scene.list.tasks.allSatisfy { $0.deferredCount >= 1 })
        await vault.bridge.shutdown()
    }

    /// Scheduling is not deferring. Being pushed out four times is a fact about
    /// a task worth surfacing; picking a date for the first time is not.
    @Test
    func schedulingSetsATimeWithoutCountingAsADeferral() async throws {
        let vault = try await TestVault()
        let scene = await fixture(vault.bridge)
        let task = try #require(scene.list.tasks.first)

        await scene.list.schedule(task, at: Date(timeIntervalSince1970: 1_800_000_000))

        let after = try #require(scene.list.tasks.first { $0.id == task.id })
        #expect(after.scheduledAt != nil)
        #expect(after.deferredCount == 0)
        await vault.bridge.shutdown()
    }

    @Test
    func movingASelectionRehomesEveryRowInIt() async throws {
        let vault = try await TestVault()
        let scene = await fixture(vault.bridge)
        let browse = BrowseModel(bridge: vault.bridge)
        await browse.createStream(name: "Travel", color: .sky)
        await scene.list.refresh()
        let travel = try #require(
            scene.list.streamChoices.first { $0.name == "Travel" }
        )
        scene.selection.reconcile(with: scene.list.ordered.map(\.id))
        scene.selection.selectAll()

        await scene.list.move(scene.list.rows(for: scene.selection.targets), toStream: travel.id)

        #expect(scene.list.tasks.isEmpty, "they are no longer in the Inbox")
        await vault.bridge.shutdown()
    }

    /// Today is sectioned, so its screen order is the sections' order and not
    /// the query's. The cursor walks the screen.
    @Test
    func theOrderTheCursorWalksIsTheOrderOnScreen() async throws {
        let vault = try await TestVault()
        let today = TaskListModel(bridge: vault.bridge, kind: .todayAll)
        await today.create(draft("Nothing scheduled"))
        await today.refresh()

        #expect(today.ordered.map(\.id) == today.groups.flatMap(\.tasks).map(\.id))
        await vault.bridge.shutdown()
    }
}

/// A mutable flag a `@Sendable` closure can set.
///
/// `ListEscapes` hands its callbacks to the view layer, so they cannot capture
/// a local `var`; one reference box is cheaper than making the whole struct
/// observable for the sake of a test.
@MainActor
private final class Box {
    var value = false
}
