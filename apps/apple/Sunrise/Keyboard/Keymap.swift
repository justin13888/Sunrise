import Foundation

/// Everything the keyboard can ask the app to do.
///
/// One enum for the whole surface, because the same list is needed three times
/// over: to resolve a key press, to draw the command palette, and to print the
/// cheat sheet. Three hand-kept lists would be three chances for ⌘2 to open the
/// Inbox, be described as opening Today, and be missing from the palette.
enum AppAction: String, CaseIterable, Hashable, Sendable {
    case quickCaptureGlobal
    case quickCapture
    case today
    case inbox
    case searchInView
    case searchGlobal
    case commandPalette
    case newStream
    case markDone
    case deferTask
    case schedule
    case moveToStream
    case focusMode
    case moveUp
    case moveDown
    /// `gg` and `G`. No chord in the default keymap — the spec's macOS column
    /// has none — so these reach the keyboard only through vim mode, and the
    /// palette otherwise.
    case listTop
    case listBottom
    case openDetail
    case closeDetail
    case toggleSelection
    case extendSelectionUp
    case extendSelectionDown
    case undo
    case redo
    /// ⌘P. Prints what is on screen, for the views that have a paper shape —
    /// see `PrintDocument`.
    case printView
    /// The same document, written rather than printed. No chord: ⌘⇧P is the
    /// command palette, and stealing it for an export nobody runs daily would
    /// be the wrong trade.
    case exportPDF
    case cheatSheet
    /// ⌥⌘M and ⌥⌘E: the two daily briefs, which a reminder also opens.
    case morningSummary
    case endOfDay
    /// ⇧⌘I. File → Import Calendar…, which raises a file picker.
    case importCalendar

    /// What the palette, the cheat sheet and the menu all call it.
    var title: String {
        switch self {
        case .quickCaptureGlobal: L10n.Keyboard.Command.quickCaptureGlobal
        case .quickCapture: L10n.Keyboard.Command.quickCapture
        case .today: L10n.Keyboard.Command.today
        case .inbox: L10n.Keyboard.Command.inbox
        case .searchInView: L10n.Keyboard.Command.searchInView
        case .searchGlobal: L10n.Keyboard.Command.searchGlobal
        case .commandPalette: L10n.Keyboard.Command.commandPalette
        case .newStream: L10n.Keyboard.Command.newStream
        case .markDone: L10n.Keyboard.Command.markDone
        case .deferTask: L10n.Keyboard.Command.deferTask
        case .schedule: L10n.Keyboard.Command.schedule
        case .moveToStream: L10n.Keyboard.Command.moveToStream
        case .focusMode: L10n.Keyboard.Command.focusMode
        case .moveUp: L10n.Keyboard.Command.moveUp
        case .moveDown: L10n.Keyboard.Command.moveDown
        case .listTop: L10n.Keyboard.Command.listTop
        case .listBottom: L10n.Keyboard.Command.listBottom
        case .openDetail: L10n.Keyboard.Command.openDetail
        case .closeDetail: L10n.Keyboard.Command.closeDetail
        case .toggleSelection: L10n.Keyboard.Command.toggleSelection
        case .extendSelectionUp: L10n.Keyboard.Command.extendSelectionUp
        case .extendSelectionDown: L10n.Keyboard.Command.extendSelectionDown
        case .undo: L10n.Keyboard.Command.undo
        case .redo: L10n.Keyboard.Command.redo
        case .printView: L10n.Keyboard.Command.printView
        case .exportPDF: L10n.Keyboard.Command.exportPdf
        case .cheatSheet: L10n.Keyboard.Command.cheatSheet
        case .morningSummary: L10n.Keyboard.Command.morningSummary
        case .endOfDay: L10n.Keyboard.Command.endOfDay
        case .importCalendar: L10n.Keyboard.Command.importCalendar
        }
    }

    var symbol: String {
        switch self {
        case .quickCaptureGlobal, .quickCapture: "plus.circle"
        case .today: "sun.max"
        case .inbox: "tray"
        case .searchInView, .searchGlobal: "magnifyingglass"
        case .commandPalette: "command"
        case .newStream: "number"
        case .markDone: "checkmark.circle"
        case .deferTask: "clock.arrow.circlepath"
        case .schedule: "calendar"
        case .moveToStream: "arrow.right.circle"
        case .focusMode: "timer"
        case .moveUp, .extendSelectionUp: "arrow.up"
        case .moveDown, .extendSelectionDown: "arrow.down"
        case .listTop: "arrow.up.to.line"
        case .listBottom: "arrow.down.to.line"
        case .openDetail: "square.and.pencil"
        case .closeDetail: "xmark"
        case .toggleSelection: "checklist"
        case .undo: "arrow.uturn.backward"
        case .redo: "arrow.uturn.forward"
        case .printView: "printer"
        case .exportPDF: "doc.richtext"
        case .cheatSheet: "keyboard"
        case .morningSummary: "sunrise"
        case .endOfDay: "moon.stars"
        case .importCalendar: "square.and.arrow.down"
        }
    }

    var section: KeySection {
        switch self {
        case .quickCaptureGlobal, .quickCapture: .capture
        case .today, .inbox, .searchInView, .searchGlobal, .commandPalette, .newStream,
             .morningSummary, .endOfDay: .navigation
        case .moveUp, .moveDown, .listTop, .listBottom, .openDetail, .closeDetail,
             .toggleSelection, .extendSelectionUp, .extendSelectionDown: .list
        case .markDone, .deferTask, .schedule, .moveToStream, .focusMode: .task
        case .undo, .redo: .edit
        case .printView, .exportPDF, .importCalendar: .document
        case .cheatSheet: .help
        }
    }

    /// Whether the action needs a row under the cursor.
    ///
    /// The palette reads this to grey an entry out rather than hide it: a
    /// command that vanishes when it cannot run teaches nobody that it exists.
    var needsSelection: Bool { section == .task }

    /// Whether this platform can run the action at all.
    ///
    /// Printing is the Mac's alone (`macOS/PrintJob.swift`), so on iOS and
    /// iPadOS it is left out of the palette, the cheat sheet and the key
    /// commands rather than offered greyed: unlike a row command waiting for a
    /// selection, there is nothing the user could do here to make it work.
    var isOffered: Bool {
        #if os(macOS)
        true
        #else
        self != .printView && self != .exportPDF
        #endif
    }
}

/// How the cheat sheet groups the keymap.
enum KeySection: String, CaseIterable, Hashable, Sendable {
    case capture
    case navigation
    case list
    case task
    case edit
    case document
    case help

    var title: String {
        switch self {
        case .capture: L10n.Keyboard.Heading.capture
        case .navigation: L10n.Keyboard.Heading.navigation
        case .list: L10n.Keyboard.Heading.list
        case .task: L10n.Keyboard.Heading.task
        case .edit: L10n.Keyboard.Heading.edit
        case .document: L10n.Keyboard.Heading.document
        case .help: L10n.Keyboard.Heading.help
        }
    }
}

/// Where a binding is live.
enum KeyScope: Hashable, Sendable {
    /// Bound on a menu item, so it fires wherever focus happens to be — which
    /// is what makes ⌘Z work while a title is being typed.
    case application
    /// Bound on the task list, so it fires only while a row holds key focus.
    ///
    /// This is what lets `d` mean Defer and still be a letter you can type into
    /// the capture field: a focused text field consumes the press before the
    /// list ever sees it.
    case list
}

struct KeyBinding: Hashable, Sendable {
    let chord: KeyChord
    let action: AppAction
    let scope: KeyScope
}

/// The macOS column of `docs/08-features/keyboard.md`, as data.
///
/// The spec's table is the source of truth and this is a transcription of it,
/// so the tests assert *the spec* — chord by chord — rather than asserting that
/// the app agrees with itself.
enum Keymap {
    static let bindings: [KeyBinding] = application + list

    static let application: [KeyBinding] = [
        app(KeyChord("n", [.command, .shift]), .quickCaptureGlobal),
        app(KeyChord("n", [.command]), .quickCapture),
        app(KeyChord("1", [.command]), .today),
        app(KeyChord("2", [.command]), .inbox),
        app(KeyChord("f", [.command]), .searchInView),
        app(KeyChord("k", [.command]), .searchGlobal),
        app(KeyChord("p", [.command, .shift]), .commandPalette),
        app(KeyChord("s", [.command, .shift]), .newStream),
        app(KeyChord("z", [.command]), .undo),
        app(KeyChord("z", [.command, .shift]), .redo),
        // The rest of the spec's table: the two briefs, and calendar import.
        app(KeyChord("m", [.command, .option]), .morningSummary),
        app(KeyChord("e", [.command, .option]), .endOfDay),
        app(KeyChord("i", [.command, .shift]), .importCalendar),
        // Not in the spec's table either. `docs/07-clients/parity-matrix.md`
        // marks Print / PDF export a macOS SHOULD, and ⌘P is the chord every
        // Mac user already has in their fingers for it.
        app(KeyChord("p", [.command]), .printView),
        // Not in the spec's table. `?` is, and `?` alone cannot be a menu key
        // equivalent — it would fire while somebody typed a question mark into
        // a title — so the menu carries ⌘/ and the views carry bare `?`. Both
        // open the same sheet; the menu item is the visible affordance
        // `docs/10-cross-cutting/accessibility.md` requires every shortcut to
        // have.
        app(KeyChord("/", [.command]), .cheatSheet)
    ]

    static let list: [KeyBinding] = [
        row(KeyChord("x"), .markDone),
        row(KeyChord("d"), .deferTask),
        row(KeyChord("s"), .schedule),
        row(KeyChord("m"), .moveToStream),
        row(KeyChord("f"), .focusMode),
        row(KeyChord(.upArrow), .moveUp),
        row(KeyChord("k"), .moveUp),
        row(KeyChord(.downArrow), .moveDown),
        row(KeyChord("j"), .moveDown),
        row(KeyChord(.returnKey), .openDetail),
        row(KeyChord(.rightArrow), .openDetail),
        row(KeyChord(.escape), .closeDetail),
        row(KeyChord(.leftArrow), .closeDetail),
        row(KeyChord(.space), .toggleSelection),
        row(KeyChord(.upArrow, [.shift]), .extendSelectionUp),
        row(KeyChord(.downArrow, [.shift]), .extendSelectionDown),
        row(KeyChord("?"), .cheatSheet)
    ]

    private static func app(_ chord: KeyChord, _ action: AppAction) -> KeyBinding {
        KeyBinding(chord: chord, action: action, scope: .application)
    }

    private static func row(_ chord: KeyChord, _ action: AppAction) -> KeyBinding {
        KeyBinding(chord: chord, action: action, scope: .list)
    }

    /// Resolve a press.
    ///
    /// The two scopes are looked up separately rather than one falling back to
    /// the other. A list that also answered the application chords would run ⌘Z
    /// twice — once here and once from the Undo menu item — and an undo that
    /// went back two steps is worse than one that went back none.
    static func action(for chord: KeyChord, scope: KeyScope) -> AppAction? {
        let table = scope == .application ? application : list
        // Folded for the list only: `X` and `x` are one request there, whereas
        // an application chord already says which modifiers it wants.
        let wanted = scope == .list ? chord.caseFolded : chord
        return table.first { $0.chord == wanted }?.action
    }

    /// Every chord bound to an action, in the order the spec lists them.
    static func chords(for action: AppAction) -> [KeyChord] {
        bindings.filter { $0.action == action }.map(\.chord)
    }

    /// What the palette prints beside a command, and the cheat sheet in its
    /// right-hand column. Alternates are joined so `↑ / K` reads as one row.
    static func shortcutLabel(for action: AppAction) -> String {
        chords(for: action).map(\.display).joined(separator: " / ")
    }
}

// MARK: - Hints

/// Rendering a binding into the places an action appears.
///
/// `docs/08-features/keyboard.md` Rule 1: every surface that offers a bound
/// action shows its key, and no surface formats a chord by hand. Each helper
/// reads ``Keymap/shortcutLabel(for:)``, so a tooltip, a context menu and an
/// empty state cannot name a key the app does not answer.
extension Keymap {
    /// A tooltip: the title, then the key in parentheses — "Undo complete
    /// ‘Report’ (⌘Z)". The bare title when the action has no binding.
    static func help(_ title: String, for action: AppAction) -> String {
        hinted(title, shortcutLabel(for: action))
    }

    /// The same, for a chord that is not an ``AppAction`` — a note mark.
    static func help(_ title: String, chord: KeyChord) -> String {
        hinted(title, chord.display)
    }

    /// A menu row whose platform draws no key equivalent — a context menu,
    /// whose keys are bare letters bound on the list rather than menu
    /// equivalents. The key is set after the title, apart from it.
    static func menuTitle(_ title: String, for action: AppAction) -> String {
        let keys = shortcutLabel(for: action)
        return keys.isEmpty ? title : "\(title)   \(keys)"
    }

    /// The sentence an empty state ends on: "Press ⌘N to capture." Empty when
    /// the action has no binding, so the caller never prints "Press  to".
    static func pressHint(_ action: AppAction, to purpose: String) -> String {
        pressHint(action) { L10n.Keyboard.pressHint(keys: $0, purpose: purpose) }
    }

    /// The same, worded whole by the caller: `sentence` is handed the keys
    /// and returns the catalog's sentence for them, so a translation can put
    /// the keys wherever its grammar wants them.
    static func pressHint(_ action: AppAction, _ sentence: (String) -> String) -> String {
        let keys = shortcutLabel(for: action)
        return keys.isEmpty ? "" : sentence(keys)
    }

    private static func hinted(_ title: String, _ keys: String) -> String {
        keys.isEmpty ? title : "\(title) (\(keys))"
    }
}

// MARK: - Chords that are not app actions

/// The handful of chords that belong to a control rather than to the app.
///
/// Held here so that `Keymap` stays the only place a chord is written down
/// (Rule 2). None of them is an ``AppAction``: none can run from the palette.
/// Quit is the platform's own command, which the Mac's app menu already lists
/// and iOS does not have; the capture bar's Return commits the field it sits
/// beside; and a note mark acts on the text under the caret of one editor.
extension Keymap {
    /// ⌘Q on the menu bar panel's own Quit button.
    static let quit = KeyChord("q", [.command])

    /// Return on the capture bar's Add button.
    static let submitCapture = KeyChord(.returnKey)

    /// The note editor's formatting keys — the table in
    /// `docs/08-features/keyboard.md` §Note editor.
    static func chord(for mark: NoteMark) -> KeyChord {
        switch mark {
        case .bold: KeyChord("b", [.command])
        case .italic: KeyChord("i", [.command])
        case .underline: KeyChord("u", [.command])
        case .strike: KeyChord("x", [.command])
        case .code: KeyChord("e", [.command])
        }
    }
}
