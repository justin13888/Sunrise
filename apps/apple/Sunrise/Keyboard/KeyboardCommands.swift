import SwiftUI

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
            keyboardShortcut(chord.keyEquivalent, modifiers: chord.modifiers.eventModifiers)
        } else {
            self
        }
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
