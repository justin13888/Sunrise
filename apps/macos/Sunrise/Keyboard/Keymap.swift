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

    /// What the palette, the cheat sheet and the menu all call it.
    var title: String {
        switch self {
        case .quickCaptureGlobal: "Quick Capture (anywhere)"
        case .quickCapture: "New Task"
        case .today: "Go to Today"
        case .inbox: "Go to Inbox"
        case .searchInView: "Find in This List"
        case .searchGlobal: "Search Everything"
        case .commandPalette: "Command Palette"
        case .newStream: "New Stream…"
        case .markDone: "Mark Done"
        case .deferTask: "Defer to Tomorrow"
        case .schedule: "Schedule…"
        case .moveToStream: "Move to Stream…"
        case .focusMode: "Start Focus Session"
        case .moveUp: "Move Up"
        case .moveDown: "Move Down"
        case .listTop: "Go to Top"
        case .listBottom: "Go to Bottom"
        case .openDetail: "Open"
        case .closeDetail: "Close"
        case .toggleSelection: "Toggle Selection"
        case .extendSelectionUp: "Extend Selection Up"
        case .extendSelectionDown: "Extend Selection Down"
        case .undo: "Undo"
        case .redo: "Redo"
        case .printView: "Print…"
        case .exportPDF: "Export as PDF…"
        case .cheatSheet: "Keyboard Shortcuts"
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
        }
    }

    var section: KeySection {
        switch self {
        case .quickCaptureGlobal, .quickCapture: .capture
        case .today, .inbox, .searchInView, .searchGlobal, .commandPalette, .newStream: .navigation
        case .moveUp, .moveDown, .listTop, .listBottom, .openDetail, .closeDetail,
             .toggleSelection, .extendSelectionUp, .extendSelectionDown: .list
        case .markDone, .deferTask, .schedule, .moveToStream, .focusMode: .task
        case .undo, .redo: .edit
        case .printView, .exportPDF: .document
        case .cheatSheet: .help
        }
    }

    /// Whether the action needs a row under the cursor.
    ///
    /// The palette reads this to grey an entry out rather than hide it: a
    /// command that vanishes when it cannot run teaches nobody that it exists.
    var needsSelection: Bool { section == .task }
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
        case .capture: "Capture"
        case .navigation: "Going places"
        case .list: "Moving through a list"
        case .task: "Acting on what is selected"
        case .edit: "Undo"
        case .document: "Printing"
        case .help: "Help"
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
