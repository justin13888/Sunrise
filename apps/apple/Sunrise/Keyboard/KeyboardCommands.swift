import SwiftUI
#if os(iOS)
import UIKit
#endif

/// The things in the main window that can hold the keyboard.
///
/// One enum rather than a `@FocusState` per field: ⌘F has to be able to *move*
/// focus, and moving it means naming where it is going.
enum PaneFocus: Hashable, Sendable {
    case capture
    case rows
    case search
}

extension View {
    /// Bind a menu item to whatever `docs/08-features/keyboard.md` gives this
    /// action.
    ///
    /// Read from the keymap rather than written at the call site, so the menu,
    /// the palette and the cheat sheet cannot end up printing three different
    /// answers for the same command. Only chords carrying ⌘ are eligible: a
    /// menu key equivalent with no modifier fires while somebody is typing into
    /// a text field, which is how `X` would start completing tasks mid-word.
    @ViewBuilder
    func keyboardShortcut(for action: AppAction) -> some View {
        if let chord = Keymap.chords(for: action).first(where: { $0.modifiers.contains(.command) }) {
            keyboardShortcut(chord)
        } else {
            self
        }
    }

    /// Bind one of `Keymap`'s chords that is not an action — Quit, a field's
    /// Return, a note mark. The only other way a view in this app binds a key.
    func keyboardShortcut(_ chord: KeyChord) -> some View {
        keyboardShortcut(chord.keyEquivalent, modifiers: chord.modifiers.eventModifiers)
    }

    /// Resolve a press against one of the keymaps and act on it.
    ///
    /// The view's whole share of the keyboard is this modifier: matching is
    /// `Keymap`'s, and what an action *does* is the caller's. Neither is in a
    /// view body, which is what makes both testable.
    func onKeyChord(
        scope: KeyScope,
        vim: VimNormalMode? = nil,
        perform: @escaping (AppAction) -> Bool
    ) -> some View {
        onKeyPress(phases: .down) { press in
            let chord = KeyChord(press)
            if let vim {
                switch vim.resolve(chord) {
                case let .run(action):
                    return perform(action) ? .handled : .ignored
                case .pending:
                    // Half a motion. Swallowed so the `g` of `gg` does not also
                    // fall through to whatever `g` would otherwise mean.
                    return .handled
                case .unhandled:
                    break
                }
            }
            guard let action = Keymap.action(for: chord, scope: scope) else { return .ignored }
            return perform(action) ? .handled : .ignored
        }
    }
}

/// Whether this device is held to the desktop keyboard set.
///
/// `docs/08-features/keyboard.md` Rule 3: the Mac, and an iPad — whose
/// hardware keyboard is the expected way to drive it at a desk — get the
/// palette, the cheat sheet and every hint. A phone is exempt, and stays
/// inert: a palette or an empty state that teaches a ⌘ chord is noise on a
/// device that almost never has a key to press it with.
enum KeyboardClass {
    @MainActor
    static var isDesktopClass: Bool {
        #if os(macOS)
        true
        #else
        UIDevice.current.userInterfaceIdiom == .pad
        #endif
    }
}

/// Which menu carries each bound action, as data.
///
/// The Mac's menu bar (`macOS/AppCommands.swift`) and the iPad's key commands
/// (``KeyCommandMenus``) both draw from these lists, so "every action with a
/// chord is in at least one menu" — `docs/08-features/keyboard.md` Rule 2 — is
/// something a test can read rather than something a reviewer has to notice.
enum CommandMenus {
    /// File, top: capture into this window, and a new stream.
    static let newItem: [AppAction] = [.quickCapture, .newStream]
    /// File → Import Calendar….
    static let importExport: [AppAction] = [.importCalendar]
    /// File → Print… / Export as PDF….
    static let printing: [AppAction] = [.printView, .exportPDF]
    /// The app menu: capture from anywhere, and the two daily briefs.
    static let app: [AppAction] = [.quickCaptureGlobal, .morningSummary, .endOfDay]
    /// View: the two fixed lists, both searches, and the palette.
    static let go: [AppAction] = [.today, .inbox, .searchInView, .searchGlobal, .commandPalette]
    /// Help: the cheat sheet, on ⌘/ because `?` cannot be a menu key.
    static let help: [AppAction] = [.cheatSheet]
    /// Undo and Redo. On the Mac they are ``UndoMenu``'s two toolbar buttons,
    /// titled after the step they would take; on the iPad, Edit's own items.
    static let edit: [AppAction] = [.undo, .redo]

    static let all: [AppAction] = newItem + importExport + printing + app + go + help + edit
}

/// The iPad's application-scope keys, as `UIKeyCommand`-backed menu commands.
///
/// `docs/08-features/keyboard.md` Rule 3 holds a tablet with a hardware
/// keyboard to the desktop set. SwiftUI turns each of these into a key command
/// — listed in the ⌘-hold overlay and the iPadOS menu bar, which is the
/// visible affordance the accessibility spec asks of every shortcut — and each
/// hands its action to the window through ``AppSurfaces/request(_:)``, the same
/// door the Mac's menu bar uses.
///
/// Undo and Redo replace the system's pair rather than sit beside them: two
/// menu items on ⌘Z is one of them silently never firing.
struct KeyCommandMenus: Commands {
    let surfaces: AppSurfaces

    var body: some Commands {
        CommandGroup(replacing: .newItem) {
            items(CommandMenus.newItem + CommandMenus.importExport + CommandMenus.printing)
        }
        CommandGroup(replacing: .undoRedo) {
            items(CommandMenus.edit)
        }
        CommandMenu(L10n.Keyboard.menuGo) {
            items(CommandMenus.app + CommandMenus.go + CommandMenus.help)
        }
    }

    private func items(_ actions: [AppAction]) -> some View {
        ForEach(actions.filter(\.isOffered), id: \.self) { action in
            Button(action.title) { surfaces.request(action) }
                .keyboardShortcut(for: action)
        }
    }
}
