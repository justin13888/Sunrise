import Foundation
import Testing

@testable import Sunrise

/// A press, written out — see the twin in `KeymapTests.swift`.
private func press(_ key: Character, _ modifiers: KeyModifiers = []) -> KeyChord {
    KeyChord(key, modifiers)
}

/// `docs/08-features/keyboard.md` Rule 1: one renderer puts the key wherever
/// the action appears.
struct KeyHintTests {
    /// The tooltip shape the spec gives, word for word.
    @Test
    func aTooltipEndsOnItsKeyInParentheses() {
        #expect(
            Keymap.help("Undo complete \u{2018}Report\u{2019}", for: .undo)
                == "Undo complete \u{2018}Report\u{2019} (⌘Z)"
        )
        #expect(Keymap.help("Redo", for: .redo) == "Redo (⇧⌘Z)")
    }

    /// No key, no parentheses: "Export as PDF… ()" would be a hint that
    /// teaches nothing and reads as a bug.
    @Test
    func anUnboundActionKeepsItsBareTitle() {
        #expect(Keymap.help("Export", for: .exportPDF) == "Export")
        #expect(Keymap.menuTitle("Defer a week", for: .listTop) == "Defer a week")
        #expect(Keymap.pressHint(.exportPDF, to: "export").isEmpty)
    }

    @Test
    func aContextMenuRowSetsItsKeyApart() {
        #expect(Keymap.menuTitle("Complete", for: .markDone) == "Complete   X")
    }

    @Test
    func anEmptyStateSentenceNamesTheKey() {
        #expect(Keymap.pressHint(.quickCapture, to: "capture") == "Press ⌘N to capture.")
    }

    /// The note editor's table in the spec, transcribed a second time.
    @Test
    func theNoteMarksAreTheOnesTheSpecStates() {
        let expected: [(NoteMark, KeyChord)] = [
            (.bold, press("b", [.command])),
            (.italic, press("i", [.command])),
            (.underline, press("u", [.command])),
            (.strike, press("x", [.command])),
            (.code, press("e", [.command]))
        ]
        for (mark, chord) in expected {
            #expect(Keymap.chord(for: mark) == chord, "\(mark.label)")
        }
        #expect(Keymap.help("Bold", chord: Keymap.chord(for: .bold)) == "Bold (⌘B)")
    }

    @Test
    func theControlChordsAreTheConventionalOnes() {
        #expect(Keymap.quit == press("q", [.command]))
        #expect(Keymap.submitCapture == press(.returnKey))
    }
}

/// Rule 2's second half: every action with a chord is listed everywhere a
/// user would look for it. A binding that only the keymap knows about is a
/// binding only the people who read the source have.
@MainActor
struct KeymapReachTests {
    /// Every bound action this platform offers.
    private var bound: [AppAction] {
        AppAction.allCases.filter { $0.isOffered && !Keymap.chords(for: $0).isEmpty }
    }

    /// The palette itself is the one exception: it does not list itself,
    /// because it is already open — its chord is in the menu and the sheet.
    @Test
    func everyBoundActionIsInThePalette() {
        for action in bound where action != .commandPalette {
            #expect(
                CommandPaletteModel.catalogue.contains(action),
                "\(action.title) has a key and no palette row"
            )
        }
    }

    @Test
    func everyBoundActionIsOnTheCheatSheet() {
        let titles = CheatSheet.sections(for: KeyboardContext(hasList: true))
            .flatMap(\.rows)
            .map(\.title)
        for action in bound {
            #expect(titles.contains(action.title), "\(action.title) has a key and no cheat-sheet row")
        }
    }

    /// Application-scope actions are menu commands. The row keys are not —
    /// they are bare letters, and a menu key equivalent with no modifier would
    /// fire mid-word — so their menu is the row's context menu, which prints
    /// them through ``Keymap/menuTitle(_:for:)``.
    @Test
    func everyApplicationActionIsInAMenu() {
        for binding in Keymap.application where binding.action.isOffered {
            #expect(
                CommandMenus.all.contains(binding.action),
                "\(binding.action.title) is bound to \(binding.chord.display) and in no menu"
            )
        }
    }

    /// One action, one menu item: two items on one chord is one of them never
    /// firing.
    @Test
    func noActionIsInTwoMenus() {
        #expect(Set(CommandMenus.all).count == CommandMenus.all.count)
    }

    /// Printing is the Mac's. Offered greyed on an iPad it would be a command
    /// nothing on that device can ever make work.
    @Test
    func printingIsOfferedOnTheMacOnly() {
        #if os(macOS)
        #expect(AppAction.printView.isOffered)
        #else
        #expect(!AppAction.printView.isOffered)
        #expect(!CommandPaletteModel.catalogue.contains(.exportPDF))
        #endif
    }
}

/// Rule 2's first half: no view binds a key with a literal chord.
///
/// Read from source, the way `TabDropTargetTests` reads the iOS shell: the
/// claim is about what the code says, and a behavioural test would need every
/// view on screen at once to make it.
struct NoLiteralChordTests {
    /// `apps/apple`, found relative to this file.
    private static let root = URL(filePath: #filePath)
        .deletingLastPathComponent()
        .deletingLastPathComponent()

    /// What may follow `.keyboardShortcut(`. An action, one of `Keymap`'s
    /// control chords, or a role — `.defaultAction` and `.cancelAction` name
    /// what the button *is*, and the platform picks the key.
    private static let allowed = ["for:", "Keymap.", ".defaultAction", ".cancelAction"]

    @Test
    func everyKeyboardShortcutIsReadFromTheKeymap() throws {
        let sites = try Self.sites()
        // The anchor: a scan that found nothing would be green over nothing.
        #expect(sites.count >= 10, "found only \(sites.count) `.keyboardShortcut(` sites")
        #expect(sites.contains { $0.file == "SavedViewsMenu.swift" })
        for site in sites where !Self.allowed.contains(where: { site.argument.hasPrefix($0) }) {
            Issue.record(
                """
                \(site.file):\(site.line) binds `.keyboardShortcut(\(site.argument)`.

                Every chord lives in `Keymap` (docs/08-features/keyboard.md \
                Rule 2). Add an `AppAction` with its binding and use \
                `.keyboardShortcut(for:)`, or add a control chord to `Keymap`.
                """
            )
        }
    }

    private struct Site {
        let file: String
        let line: Int
        let argument: String
    }

    /// Every `.keyboardShortcut(` call in the app's own sources, comments
    /// stripped so a sentence about a shortcut is not mistaken for one.
    private static func sites() throws -> [Site] {
        var found: [Site] = []
        for folder in ["Sunrise", "macOS", "iOS"] {
            let directory = root.appending(path: folder)
            let paths = try FileManager.default
                .subpathsOfDirectory(atPath: directory.path(percentEncoded: false))
            for path in paths where path.hasSuffix(".swift") {
                let file = directory.appending(path: path)
                let lines = try String(contentsOf: file, encoding: .utf8)
                    .components(separatedBy: .newlines)
                for (index, raw) in lines.enumerated() {
                    let code = raw.components(separatedBy: "//").first ?? raw
                    guard let call = code.range(of: ".keyboardShortcut(") else { continue }
                    let argument = code[call.upperBound...].trimmingCharacters(in: .whitespaces)
                    found.append(Site(file: file.lastPathComponent, line: index + 1, argument: argument))
                }
            }
        }
        return found
    }
}

/// Empty states name the key that fills them.
struct EmptyStateHintTests {
    @Test
    func aListCaptureCanFillNamesTheCaptureKey() {
        let hint = "Press ⌘N to capture."
        let stream = TaskListKind.stream(id: "str_1", name: "Home")
        #expect(TaskListKind.todayAll.emptyDescription(hinted: true).hasSuffix(hint))
        #expect(TaskListKind.inbox.emptyDescription(hinted: true) == "Your Inbox is empty. \(hint)")
        #expect(stream.emptyDescription(hinted: true).hasSuffix(hint))
    }

    @Test
    func searchNamesItsField() {
        let blank = TaskListKind.search(text: "").emptyDescription(hinted: true)
        let missed = TaskListKind.search(text: "ferry").emptyDescription(hinted: true)
        #expect(blank.hasSuffix("Press ⌘F to jump to the field."))
        #expect(missed == "Nothing matches \u{201c}ferry\u{201d}. Press ⌘K to start a new search.")
    }

    /// A list nothing typed can land in names no capture key: a capture there
    /// would be written and filtered straight back out.
    @Test
    func aListThatRefusesCaptureTeachesNoCaptureKey() {
        #expect(TaskListKind.today(contexts: ["ctx_1"]).emptyHint.isEmpty)
        #expect(TaskListKind.context(id: "ctx_1", name: "errands").emptyHint.isEmpty)
    }

    /// A phone has no key to press, so its empty state is the bare sentence.
    @Test
    func withoutAKeyboardTheSentenceStandsAlone() {
        #expect(TaskListKind.inbox.emptyDescription(hinted: false) == TaskListKind.inbox.emptyMessage)
    }
}
