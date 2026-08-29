import Foundation
import Testing

@testable import Sunrise

/// A press, as `onKeyPress` would report it.
///
/// Written out rather than taken from a real event: the point of these tests is
/// that the *spec's* table is what the app answers, and a real event would
/// bring a keyboard layout along with it.
private func press(_ key: Character, _ modifiers: KeyModifiers = []) -> KeyChord {
    KeyChord(key, modifiers)
}

/// Chord normalisation, which is where a layout-dependent bug would live.
struct KeyChordTests {
    /// `?` is Shift-`/` here and Shift-`,` in France. The character already
    /// says which key was pressed, so carrying the Shift bit as well would make
    /// a binding that matched on one layout and missed on the other.
    @Test
    func shiftIsDroppedWhereTheCharacterAlreadyCarriesIt() {
        #expect(press("?", [.shift]) == press("?"))
        #expect(press(":", [.shift]) == press(":"))
        #expect(press("!", [.shift, .command]) == press("!", [.command]))
    }

    /// An arrow has one glyph whatever is held with it, so `⇧↓` and `↓` are two
    /// different presses — and the spec binds both.
    @Test
    func shiftIsKeptOnKeysThatHaveNoShiftedGlyph() {
        #expect(press(.downArrow, [.shift]) != press(.downArrow))
        #expect(press(.upArrow, [.shift]) != press(.upArrow))
        #expect(press(.returnKey, [.shift]) != press(.returnKey))
    }

    /// A letter keeps its Shift bit and loses its case. Both halves matter:
    /// keeping Shift is what makes `⇧⌘Z` a different binding from `⌘Z`, and
    /// dropping case is what makes the binding survive Caps Lock.
    @Test
    func aLetterKeepsItsShiftAndLosesItsCase() {
        #expect(press("Z", [.command, .shift]) != press("z", [.command]))
        #expect(press("Z", [.command]) == press("z", [.command]))
        #expect(press("G", [.shift]) != press("g"))
    }

    /// Folding is on request, and only for letters. Applied generally it would
    /// turn `⇧↓` into `↓` and lose range selection entirely.
    @Test
    func foldingDropsShiftFromLettersOnly() {
        #expect(press("G", [.shift]).caseFolded == press("g"))
        #expect(press(.downArrow, [.shift]).caseFolded == press(.downArrow, [.shift]))
        #expect(press("?").caseFolded == press("?"))
    }

    /// What a menu, the palette and the cheat sheet print. Modifier order is
    /// Apple's — ⌃⌥⇧⌘ — so two rows of the same sheet cannot disagree.
    @Test
    func chordsPrintInTheOrderApplePrintsThem() {
        #expect(press("p", [.command, .shift]).display == "⇧⌘P")
        #expect(press("r", [.control]).display == "⌃R")
        #expect(press(.upArrow, [.shift]).display == "⇧↑")
        #expect(press(.space).display == "space")
        #expect(press(.escape).display == "esc")
    }
}

/// The macOS column of `docs/08-features/keyboard.md`, asserted row by row.
///
/// Deliberately literal. These are not tests that the app agrees with itself —
/// they are the spec, transcribed a second time by a different hand, and they
/// fail if either copy drifts.
struct KeymapTests {
    @Test
    func theApplicationBindingsAreTheOnesTheSpecStates() {
        let expected: [(KeyChord, AppAction)] = [
            (press("n", [.command, .shift]), .quickCaptureGlobal),
            (press("n", [.command]), .quickCapture),
            (press("1", [.command]), .today),
            (press("2", [.command]), .inbox),
            (press("f", [.command]), .searchInView),
            (press("k", [.command]), .searchGlobal),
            (press("p", [.command, .shift]), .commandPalette),
            (press("s", [.command, .shift]), .newStream),
            (press("z", [.command]), .undo),
            (press("z", [.command, .shift]), .redo)
        ]
        for (chord, action) in expected {
            #expect(
                Keymap.action(for: chord, scope: .application) == action,
                "\(chord.display) should be \(action.title)"
            )
        }
    }

    @Test
    func theRowBindingsAreTheOnesTheSpecStates() {
        let expected: [(KeyChord, AppAction)] = [
            (press("x"), .markDone),
            (press("d"), .deferTask),
            (press("s"), .schedule),
            (press("m"), .moveToStream),
            (press("f"), .focusMode),
            (press(.upArrow), .moveUp),
            (press("k"), .moveUp),
            (press(.downArrow), .moveDown),
            (press("j"), .moveDown),
            (press(.returnKey), .openDetail),
            (press(.rightArrow), .openDetail),
            (press(.escape), .closeDetail),
            (press(.leftArrow), .closeDetail),
            (press(.space), .toggleSelection),
            (press(.upArrow, [.shift]), .extendSelectionUp),
            (press(.downArrow, [.shift]), .extendSelectionDown),
            (press("?"), .cheatSheet)
        ]
        for (chord, action) in expected {
            #expect(
                Keymap.action(for: chord, scope: .list) == action,
                "\(chord.display) should be \(action.title)"
            )
        }
    }

    /// The spec writes the row keys as capitals. Somebody with caps lock on is
    /// still asking for the same thing.
    @Test
    func rowKeysAnswerToEitherCase() {
        #expect(Keymap.action(for: press("X", [.shift]), scope: .list) == .markDone)
        #expect(Keymap.action(for: press("D", [.shift]), scope: .list) == .deferTask)
        #expect(Keymap.action(for: press("J"), scope: .list) == .moveDown)
    }

    /// The two scopes do not fall through to one another. A list that also
    /// answered ⌘Z would run undo twice — once here and once from the menu item
    /// carrying the same chord — and an undo that went back two steps is worse
    /// than one that went back none.
    @Test
    func theScopesDoNotLeakIntoEachOther() {
        #expect(Keymap.action(for: press("z", [.command]), scope: .list) == nil)
        #expect(Keymap.action(for: press("x"), scope: .application) == nil)
        #expect(Keymap.action(for: press(.downArrow), scope: .application) == nil)
    }

    /// A bare letter must never reach a *menu*: a menu key equivalent fires
    /// while somebody is typing, which is how `X` would start completing tasks
    /// mid-word. Every application binding therefore carries ⌘.
    @Test
    func everyApplicationBindingCarriesCommand() {
        for binding in Keymap.application {
            #expect(
                binding.chord.modifiers.contains(.command),
                "\(binding.chord.display) would fire while typing"
            )
        }
    }

    /// One chord, one meaning, within a scope. A duplicate would resolve to
    /// whichever entry came first and silently drop the other.
    @Test
    func noChordIsBoundTwiceInOneScope() {
        for table in [Keymap.application, Keymap.list] {
            let chords = table.map(\.chord)
            #expect(Set(chords).count == chords.count)
        }
    }

    /// Every action has a name a human can read, so the palette and the cheat
    /// sheet can never show a raw case name.
    @Test
    func everyActionIsNamedAndIllustrated() {
        for action in AppAction.allCases {
            #expect(!action.title.isEmpty)
            #expect(!action.symbol.isEmpty)
        }
    }

    /// The label the palette prints. Alternates are joined rather than one of
    /// them being chosen, because `↑` and `K` are both true.
    @Test
    func alternatesAreBothPrinted() {
        #expect(Keymap.shortcutLabel(for: .moveDown) == "↓ / J")
        #expect(Keymap.shortcutLabel(for: .cheatSheet) == "⌘/ / ?")
        #expect(Keymap.shortcutLabel(for: .listTop).isEmpty)
    }
}

/// The `?` sheet.
struct CheatSheetTests {
    /// Away from a list — the Calendar, Focus, a brief — the row keys are not
    /// merely inert, they are unbound. A sheet that listed them would be lying.
    @Test
    func theRowSectionOnlyAppearsWhereThereAreRows() {
        let bare = CheatSheet.sections(for: KeyboardContext(hasList: false))
        let withRows = CheatSheet.sections(for: KeyboardContext(hasList: true))

        #expect(!bare.contains { $0.title == KeySection.task.title })
        #expect(withRows.contains { $0.title == KeySection.task.title })
    }

    /// `?` and ⌘/ are one command, and the sheet says so on one line. Grouping
    /// the two keymaps separately would print "Help" twice.
    @Test
    func oneCommandGetsOneRowEvenWithTwoBindings() {
        let sections = CheatSheet.sections(for: KeyboardContext(hasList: true))
        let help = sections.filter { $0.title == KeySection.help.title }

        #expect(help.count == 1)
        #expect(help.first?.rows.count == 1)
        #expect(help.first?.rows.first?.keys.contains("⌘/") == true)
        #expect(help.first?.rows.first?.keys.contains("?") == true)
    }

    @Test
    func theVimSectionIsGatedOnTheSetting() {
        let off = CheatSheet.sections(for: KeyboardContext(hasList: true, vimMode: false))
        let on = CheatSheet.sections(for: KeyboardContext(hasList: true, vimMode: true))

        #expect(!off.contains { $0.title == "Vim mode" })
        #expect(on.contains { $0.title == "Vim mode" })
    }

    /// Nothing on the sheet is unlabelled or keyless. A row with a blank
    /// right-hand column is a row that teaches nobody anything.
    @Test
    func everyRowNamesACommandAndAKey() {
        let sections = CheatSheet.sections(
            for: KeyboardContext(hasList: true, vimMode: true)
        )
        #expect(!sections.isEmpty)
        for section in sections {
            #expect(!section.rows.isEmpty)
            for row in section.rows {
                #expect(!row.title.isEmpty)
                #expect(!row.keys.isEmpty, "\(row.title) has no key")
            }
        }
    }
}

/// Vim mode: the part of the spec's motion list a list can honour.
@MainActor
struct VimModeTests {
    @Test
    func theSinglePressMotionsResolve() {
        let mode = VimNormalMode()
        #expect(mode.resolve(press("j")) == .run(.moveDown))
        #expect(mode.resolve(press("k")) == .run(.moveUp))
        #expect(mode.resolve(press("h")) == .run(.closeDetail))
        #expect(mode.resolve(press("l")) == .run(.openDetail))
        #expect(mode.resolve(press("u")) == .run(.undo))
        #expect(mode.resolve(press("r", [.control])) == .run(.redo))
        #expect(mode.resolve(press("/")) == .run(.searchInView))
        #expect(mode.resolve(press(":")) == .run(.commandPalette))
    }

    /// `G` is Shift-`g` and means the opposite end of the list from `gg`. The
    /// default keymap folds case; this one must not.
    @Test
    func caseSeparatesTheTwoEndsOfTheList() {
        let mode = VimNormalMode()
        #expect(mode.resolve(press("G", [.shift])) == .run(.listBottom))

        let second = VimNormalMode()
        #expect(second.resolve(press("g")) == .pending)
        #expect(second.resolve(press("g")) == .run(.listTop))
    }

    /// A half-typed `g` is swallowed, so it cannot also mean whatever `g` would
    /// otherwise mean — and the state is visible, so a status line can say so.
    @Test
    func aPendingPrefixIsHeldAndShown() {
        let mode = VimNormalMode()
        #expect(mode.resolve(press("g")) == .pending)
        #expect(mode.pending == "g")
        #expect(mode.resolve(press(.escape)) == .pending)
        #expect(mode.pending == nil)
    }

    /// vim throws the prefix away and re-reads the key. `gx` is a discarded `g`
    /// followed by an `x`, which the list keymap then reads as Mark done.
    @Test
    func anUnknownSecondKeyDiscardsThePrefix() {
        let mode = VimNormalMode()
        #expect(mode.resolve(press("g")) == .pending)
        #expect(mode.resolve(press("x")) == .unhandled)
        #expect(mode.pending == nil)
    }

    /// The mode is additive. Turning it on must not take Mark done, Defer,
    /// Schedule, Move or Focus away — they fall through to the list keymap,
    /// which is the only reason a vim user still has them.
    @Test
    func theDefaultRowKeysStillFallThrough() {
        let mode = VimNormalMode()
        for key in ["x", "d", "s", "m", "f"] {
            let chord = press(Character(key))
            #expect(mode.resolve(chord) == .unhandled)
            #expect(Keymap.action(for: chord, scope: .list) != nil)
        }
    }

    /// Per-device, never synced, under the spec's own key.
    @Test
    func vimModeIsOffByDefaultAndPersistsLocally() throws {
        let suite = "sunrise-tests-\(UUID().uuidString)"
        let defaults = try #require(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }

        #expect(KeyboardPreferences.vimModeKey == "editor.vim_mode")
        let preferences = KeyboardPreferences(defaults: defaults)
        #expect(!preferences.vimMode)

        preferences.vimMode = true
        #expect(KeyboardPreferences(defaults: defaults).vimMode)
    }
}

/// The command palette: ranking, and what it will let you run.
@MainActor
struct CommandPaletteTests {
    @Test
    func anEmptyQueryOffersEverythingInCheatSheetOrder() {
        let model = CommandPaletteModel()
        model.present(hasSelection: true)

        #expect(model.results.count == CommandPaletteModel.catalogue.count)
        #expect(model.results.map(\.action) == CommandPaletteModel.catalogue)
    }

    /// The palette is already open, so it does not offer to open itself.
    @Test
    func thePaletteIsNotOneOfItsOwnCommands() {
        #expect(!CommandPaletteModel.catalogue.contains(.commandPalette))
    }

    /// Initials, not prefixes: people type `gti` for "Go to Inbox".
    @Test
    func initialsFindACommand() {
        let model = CommandPaletteModel()
        model.present(hasSelection: true)
        model.query = "gti"

        #expect(model.chosen == .inbox)
    }

    @Test
    func aWordFindsItsCommand() {
        let model = CommandPaletteModel()
        model.present(hasSelection: true)
        model.query = "defer"

        #expect(model.chosen == .deferTask)
    }

    @Test
    func nothingMatchesGibberish() {
        let model = CommandPaletteModel()
        model.present(hasSelection: true)
        model.query = "zzzqqq"

        #expect(model.results.isEmpty)
        #expect(model.chosen == nil)
    }

    /// A command needing a row is shown greyed rather than hidden. The palette
    /// is also the app's answer to "what can this do", and a list that changed
    /// length with the selection would answer differently every time.
    @Test
    func rowCommandsAreShownButNotRunnableWithNothingSelected() {
        let model = CommandPaletteModel()
        model.present(hasSelection: false)
        model.query = "mark done"

        let entry = model.results.first
        #expect(entry?.action == .markDone)
        #expect(entry?.isEnabled == false)
        #expect(model.chosen == nil)
    }

    /// Every row prints its binding, which is what the spec asks the palette to
    /// do — it is how the keymap becomes discoverable without a manual.
    @Test
    func everyBoundCommandPrintsItsKey() {
        let model = CommandPaletteModel()
        model.present(hasSelection: true)
        let bound = model.results.filter { !Keymap.chords(for: $0.action).isEmpty }

        #expect(!bound.isEmpty)
        for entry in bound {
            #expect(!entry.shortcut.isEmpty)
        }
    }

    /// Clamped, not wrapped: an arrow that jumped from the last row to the
    /// first would move the highlight off screen on a key that means "down".
    @Test
    func theHighlightClampsAtBothEnds() {
        let model = CommandPaletteModel()
        model.present(hasSelection: true)

        model.moveHighlight(.up)
        #expect(model.highlighted == 0)

        for _ in 0..<(model.results.count + 5) { model.moveHighlight(.down) }
        #expect(model.highlighted == model.results.count - 1)
    }

    /// Typing re-ranks, so the highlight has to go back to the top or Return
    /// runs whatever happened to be at the old index.
    @Test
    func typingResetsTheHighlight() {
        let model = CommandPaletteModel()
        model.present(hasSelection: true)
        model.moveHighlight(.down)
        model.moveHighlight(.down)
        #expect(model.highlighted == 2)

        model.query = "in"
        #expect(model.highlighted == 0)
    }

    /// Reopening starts empty. A palette still holding last week's query would
    /// run the wrong command on the first Return.
    @Test
    func dismissingClearsTheQuery() {
        let model = CommandPaletteModel()
        model.present(hasSelection: true)
        model.query = "defer"
        model.dismiss()

        #expect(!model.isPresented)
        #expect(model.query.isEmpty)
    }

    /// A tie goes to the shorter name, which is the one the query described
    /// more completely.
    @Test
    func aCloserNameOutranksALongerOne() throws {
        let short = try #require(PaletteMatch.score("open", against: "Open"))
        let long = try #require(PaletteMatch.score("open", against: "Open Something Else"))
        #expect(short > long)
        #expect(PaletteMatch.score("open", against: "Inbox") == nil)
    }
}
