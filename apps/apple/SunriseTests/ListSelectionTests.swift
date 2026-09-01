import Foundation
import Testing

@testable import Sunrise

/// The keyboard's place in a list.
///
/// Pure: no vault, no view. Moving, ranging and surviving a re-query is the
/// logic every row-scoped binding in `docs/08-features/keyboard.md` stands on,
/// and it is exactly the part that cannot be tested through a view body.
@MainActor
struct ListSelectionTests {
    private func selection(_ ids: [String]) -> ListSelection {
        let model = ListSelection()
        model.reconcile(with: ids)
        return model
    }

    /// A freshly drawn list has no cursor: nothing has been pointed at yet, and
    /// a list that pre-selected its first row would complete it on the first
    /// stray `X`.
    @Test
    func aNewListStartsWithNoCursor() {
        let model = selection(["a1", "b2", "c3"])
        #expect(model.cursor == nil)
        #expect(model.targets.isEmpty)
    }

    /// The first arrow adopts the near end rather than doing nothing. Pressing
    /// ↓ into a list has to land somewhere, and the top is where a reader is.
    @Test
    func theFirstArrowAdoptsTheNearEnd() {
        let down = selection(["a1", "b2", "c3"])
        down.move(.down)
        #expect(down.cursor == "a1")

        let up = selection(["a1", "b2", "c3"])
        up.move(.up)
        #expect(up.cursor == "c3")
    }

    @Test
    func movingWalksTheListAndStopsAtBothEnds() {
        let model = selection(["a1", "b2", "c3"])
        model.move(.down)
        model.move(.down)
        #expect(model.cursor == "b2")

        model.move(.down)
        model.move(.down)
        #expect(model.cursor == "c3", "the end of the list does not wrap")

        model.move(.up)
        model.move(.up)
        model.move(.up)
        #expect(model.cursor == "a1")
    }

    /// With nothing ticked, a row action still has a subject. Requiring a
    /// `Space` first would make every single completion two keys instead of
    /// one.
    @Test
    func theCursorAloneIsTheTargetUntilSomethingIsTicked() {
        let model = selection(["a1", "b2", "c3"])
        model.focus("b2")

        #expect(model.targets == ["b2"])
        #expect(!model.isMultiple)
        #expect(model.isSelected("b2"))
        #expect(!model.isSelected("a1"))
    }

    /// The first `Space` must *add* the cursor's row. Starting an empty set
    /// would deselect the very row the user was about to act on, since
    /// `targets` had been answering with the cursor all along.
    @Test
    func spaceTicksTheCursorRowRatherThanStartingEmpty() {
        let model = selection(["a1", "b2", "c3"])
        model.focus("b2")
        model.toggleAtCursor()

        #expect(model.explicit == ["b2"])
        #expect(model.targets == ["b2"])

        model.toggleAtCursor()
        #expect(model.explicit.isEmpty)
        #expect(model.targets == ["b2"], "un-ticking falls back to the cursor")
    }

    /// Ticking, walking away, and ticking again leaves only the second row:
    /// the plain arrow in between dropped the first, which is the platform's
    /// own rule and the reason ⌘-click exists.
    @Test
    func aPlainMoveBetweenTwoTicksDropsTheFirst() {
        let model = selection(["a1", "b2", "c3"])
        model.focus("a1")
        model.toggleAtCursor()
        model.move(.down)
        model.move(.down)
        model.toggleAtCursor()

        // A plain move drops the range, which is the platform's own rule.
        #expect(model.targets == ["c3"])
    }

    /// Targets come back in screen order however they were ticked, so a batch
    /// action reads top to bottom.
    @Test
    func targetsComeBackInScreenOrder() {
        let model = selection(["a1", "b2", "c3"])
        model.focus("c3")
        model.toggleAtCursor()
        model.adopt(["c3", "a1"])

        #expect(model.targets == ["a1", "c3"])
        #expect(model.isMultiple)
    }

    @Test
    func shiftArrowGrowsARangeFromTheAnchor() {
        let model = selection(["a1", "b2", "c3", "d4"])
        model.focus("a1")
        model.extend(.down)
        #expect(model.targets == ["a1", "b2"])

        model.extend(.down)
        #expect(model.targets == ["a1", "b2", "c3"])
        #expect(model.cursor == "c3")
    }

    /// Reversing shrinks. The range is recomputed from the anchor rather than
    /// accumulated, which is what every other macOS list does — and the reason
    /// `⇧↓ ⇧↓ ⇧↑` leaves two rows selected rather than three.
    @Test
    func reversingARangeUnselects() {
        let model = selection(["a1", "b2", "c3", "d4"])
        model.focus("a1")
        model.extend(.down)
        model.extend(.down)
        model.extend(.up)

        #expect(model.targets == ["a1", "b2"])
    }

    /// A range can grow upward from the anchor too.
    @Test
    func aRangeCanGrowUpwards() {
        let model = selection(["a1", "b2", "c3", "d4"])
        model.focus("d4")
        model.extend(.up)
        model.extend(.up)

        #expect(model.targets == ["b2", "c3", "d4"])
    }

    /// A plain arrow after a range collapses onto the row it lands on.
    @Test
    func aPlainArrowCollapsesTheRange() {
        let model = selection(["a1", "b2", "c3", "d4"])
        model.focus("a1")
        model.extend(.down)
        model.extend(.down)
        model.move(.down)

        #expect(model.targets == ["d4"])
        #expect(model.explicit.isEmpty)
    }

    /// At the last row an unmodified arrow still means "just this one".
    @Test
    func anArrowAtTheEndStillCollapsesTheRange() {
        let model = selection(["a1", "b2"])
        model.focus("a1")
        model.extend(.down)
        model.move(.down)

        #expect(model.cursor == "b2")
        #expect(model.explicit.isEmpty)
    }

    @Test
    func escapeDropsTheTicksAndKeepsThePlace() {
        let model = selection(["a1", "b2", "c3"])
        model.focus("a1")
        model.extend(.down)
        model.clearSelection()

        #expect(model.explicit.isEmpty)
        #expect(model.cursor == "b2")
    }

    @Test
    func gAndShiftGReachBothEnds() {
        let model = selection(["a1", "b2", "c3"])
        model.moveToEdge(.last)
        #expect(model.cursor == "c3")

        model.moveToEdge(.first)
        #expect(model.cursor == "a1")
    }

    // MARK: - The list changing underneath

    /// The case that matters: complete the row under the cursor in Today and it
    /// drops out of the query. Landing on whatever took its index means four
    /// completions take four presses; clearing the cursor would make it four
    /// presses and three arrow keys.
    @Test
    func aVanishedCursorLandsOnWhateverTookItsPlace() {
        let model = selection(["a1", "b2", "c3"])
        model.focus("b2")
        model.reconcile(with: ["a1", "c3"])

        #expect(model.cursor == "c3")
    }

    /// Completing the last row walks the cursor back rather than off the end.
    @Test
    func aVanishedLastRowLandsOnTheNewLast() {
        let model = selection(["a1", "b2", "c3"])
        model.focus("c3")
        model.reconcile(with: ["a1", "b2"])

        #expect(model.cursor == "b2")
    }

    @Test
    func aCursorThatSurvivesStaysPut() {
        let model = selection(["a1", "b2", "c3"])
        model.focus("b2")
        model.reconcile(with: ["z0", "a1", "b2", "c3"])

        #expect(model.cursor == "b2")
    }

    /// Ids that left are dropped from the tick set. Acting on them would be
    /// acting on nothing, and the count beside "Schedule 3 tasks" would lie.
    @Test
    func departedRowsLeaveTheSelection() {
        let model = selection(["a1", "b2", "c3"])
        model.focus("a1")
        model.extend(.down)
        model.extend(.down)
        #expect(model.targets.count == 3)

        model.reconcile(with: ["a1", "c3"])
        #expect(model.targets == ["a1", "c3"])
    }

    @Test
    func anEmptiedListHasNothingSelected() {
        let model = selection(["a1", "b2"])
        model.focus("a1")
        model.toggleAtCursor()
        model.reconcile(with: [])

        #expect(model.cursor == nil)
        #expect(model.targets.isEmpty)
        #expect(model.anchor == nil)
    }

    /// A click, or a shift-click, arrives through the list's own selection
    /// binding. The cursor follows it so a subsequent `⇧↓` grows from where the
    /// mouse left off, and the mouse and the keyboard cannot disagree.
    @Test
    func aMouseSelectionMovesTheCursorWithIt() {
        let model = selection(["a1", "b2", "c3"])
        model.adopt(["b2", "c3"])

        #expect(model.cursor == "b2")
        #expect(model.targets == ["b2", "c3"])

        model.extend(.down)
        #expect(model.targets == ["b2", "c3"])
    }

    /// A selection naming rows this list has never had is not a selection.
    @Test
    func adoptingIgnoresRowsThatAreNotHere() {
        let model = selection(["a1", "b2"])
        model.adopt(["b2", "gone"])

        #expect(model.targets == ["b2"])
    }

    @Test
    func focusingARowThatIsNotThereChangesNothing() {
        let model = selection(["a1", "b2"])
        model.focus("a1")
        model.focus("nope")

        #expect(model.cursor == "a1")
    }

    @Test
    func selectAllTakesEveryRow() {
        let model = selection(["a1", "b2", "c3"])
        model.selectAll()

        #expect(model.targets == ["a1", "b2", "c3"])
        #expect(model.cursor == "a1")
    }
}
