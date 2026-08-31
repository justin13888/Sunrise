import SwiftUI

/// The modifier keys a binding can carry.
///
/// A parallel of `EventModifiers` rather than a use of it. The keymap is data
/// that has to be *compared* — "is this press the one bound to Defer?" — and
/// `EventModifiers` arrives from AppKit carrying bits nobody bound (`.numericPad`,
/// `.capsLock`) that would make an otherwise exact press miss. Owning the set
/// means the four modifiers Sunrise binds are the only four that can decide a
/// match.
struct KeyModifiers: OptionSet, Hashable, Sendable {
    let rawValue: Int

    /// Spelled out because the second initialiser below suppresses the
    /// memberwise one `OptionSet` would otherwise inherit.
    init(rawValue: Int) {
        self.rawValue = rawValue
    }

    static let control = KeyModifiers(rawValue: 1 << 0)
    static let option = KeyModifiers(rawValue: 1 << 1)
    static let shift = KeyModifiers(rawValue: 1 << 2)
    static let command = KeyModifiers(rawValue: 1 << 3)

    /// Narrow an AppKit modifier set to the four that can decide a binding.
    init(_ event: EventModifiers) {
        var set = KeyModifiers()
        if event.contains(.control) { set.insert(.control) }
        if event.contains(.option) { set.insert(.option) }
        if event.contains(.shift) { set.insert(.shift) }
        if event.contains(.command) { set.insert(.command) }
        self = set
    }

    var eventModifiers: EventModifiers {
        var set = EventModifiers()
        if contains(.control) { set.insert(.control) }
        if contains(.option) { set.insert(.option) }
        if contains(.shift) { set.insert(.shift) }
        if contains(.command) { set.insert(.command) }
        return set
    }

    /// The glyphs, in the order Apple prints them on a menu: ⌃⌥⇧⌘.
    ///
    /// Fixed rather than derived from insertion order, because a cheat sheet
    /// that rendered ⇧⌘P in one row and ⌘⇧P in the next is a cheat sheet people
    /// stop trusting.
    var symbols: String {
        var text = ""
        if contains(.control) { text += "⌃" }
        if contains(.option) { text += "⌥" }
        if contains(.shift) { text += "⇧" }
        if contains(.command) { text += "⌘" }
        return text
    }
}

/// One key press: a character, plus the modifiers held with it.
///
/// Normalised on the way in, and the rule differs by what kind of key it is.
///
/// - **A letter** is stored lower-cased with its Shift bit intact. Case and
///   Shift say the same thing twice, and keeping the modifier is the half that
///   survives Caps Lock — `⇧⌘Z` and `⌘Z` have to stay two bindings, and `Z` and
///   `z` have to stay one key.
/// - **A symbol** keeps its glyph and loses its Shift bit. `?` is Shift-`/` on
///   this keyboard and Shift-`,` on a French one, so the glyph is the only part
///   of that press worth comparing; a binding written as `⇧/` would match here
///   and miss there.
/// - **A positional key** — the arrows, Return, Tab, Space, Escape, Delete —
///   keeps everything. One glyph however it is pressed, so `⇧↓` and `↓` are two
///   different presses and the spec binds both.
struct KeyChord: Hashable, Sendable {
    let key: Character
    let modifiers: KeyModifiers

    init(_ key: Character, _ modifiers: KeyModifiers = []) {
        if key.isLetter {
            self.key = Character(key.lowercased())
            self.modifiers = modifiers
            return
        }
        self.key = key
        self.modifiers = KeyChord.positional.contains(key)
            ? modifiers
            : modifiers.subtracting(.shift)
    }

    /// Build from what `onKeyPress` handed over.
    init(_ press: KeyPress) {
        self.init(press.key.character, KeyModifiers(press.modifiers))
    }

    /// Keys whose glyph does not change under Shift, so Shift is meaningful.
    static let positional: Set<Character> = [
        .upArrow, .downArrow, .leftArrow, .rightArrow,
        .returnKey, .tab, .space, .escape, .deleteKey
    ]

    /// The same chord with Shift dropped from a letter.
    ///
    /// Used by the list keymap, where the spec writes the row keys as capitals
    /// and somebody with Caps Lock on is still asking for the same thing. Only
    /// letters: dropping it generally would turn `⇧↓` into `↓` and lose range
    /// selection. Vim's keymap does *not* fold — `G` and `g` are two different
    /// motions there, and that distinction is the whole point of the mode.
    var caseFolded: KeyChord {
        guard key.isLetter else { return self }
        return KeyChord(key, modifiers.subtracting(.shift))
    }

    var keyEquivalent: KeyEquivalent { KeyEquivalent(key) }

    /// How a menu, a cheat sheet or the command palette prints it.
    var display: String { modifiers.symbols + keyName }

    private var keyName: String {
        switch key {
        case .upArrow: "↑"
        case .downArrow: "↓"
        case .leftArrow: "←"
        case .rightArrow: "→"
        case .returnKey: "↩"
        case .escape: "esc"
        case .tab: "⇥"
        case .space: "space"
        case .deleteKey: "⌫"
        default:
            if key.isLetter { key.uppercased() } else { String(key) }
        }
    }
}

/// The characters AppKit reports for the keys that have no glyph of their own.
///
/// Taken from `KeyEquivalent` rather than written as literals: they are private
/// -use code points, and a transcription error would produce a binding that
/// compiles, reads correctly, and never fires.
extension Character {
    static let upArrow = KeyEquivalent.upArrow.character
    static let downArrow = KeyEquivalent.downArrow.character
    static let leftArrow = KeyEquivalent.leftArrow.character
    static let rightArrow = KeyEquivalent.rightArrow.character
    static let returnKey = KeyEquivalent.return.character
    static let escape = KeyEquivalent.escape.character
    static let tab = KeyEquivalent.tab.character
    static let space = KeyEquivalent.space.character
    static let deleteKey = KeyEquivalent.delete.character
}
