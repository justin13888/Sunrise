import Foundation

/// What a press meant in Normal mode.
enum VimResolution: Equatable, Sendable {
    /// Run it.
    case run(AppAction)
    /// The first half of a two-key motion. Swallowed, and the next press
    /// decides.
    case pending
    /// Not a vim key. The ordinary list keymap gets it — which is what keeps
    /// `X`, `D`, `S`, `M` and `F` working with vim mode on.
    case unhandled
}

/// The vim keymap, as far as a *list* can honour it.
///
/// `docs/08-features/keyboard.md` calls its table exhaustive, and it is — for a
/// surface that is half list and half document. This is the list half. What is
/// deliberately absent, and why:
///
/// - `w` `b` `e`, `0`, `$`, `i` `a` `I` `A`, `o` `O`, `x`, `p` `P` are caret
///   motions inside a text field. SwiftUI's `TextField` and `TextEditor` expose
///   no caret position at all, so they are not implementable without replacing
///   both with an `NSTextView`, and the two editors in question
///   (`TaskEditorView`, `BlockEditorView`) are outside this work.
/// - `dd` and `yy` would need `d` and `y` to become pending operators. `d` is
///   already Defer in the default keymap, and a mode that silently turned a
///   one-key defer into a two-key delete is a mode that loses somebody a task.
///
/// So this layer is **additive**: it adds the motions vim users reach for, on
/// top of the ordinary list keymap, rather than replacing it. Turning vim mode
/// on never takes an action away.
enum VimKeymap {
    /// The single-press motions. Shift-sensitive on purpose: `G` is `⇧g` and
    /// means the opposite end of the list from `gg`, so this table is the one
    /// place Shift on a letter is *not* folded away.
    static let motions: [KeyChord: AppAction] = [
        KeyChord("h"): .closeDetail,
        KeyChord("j"): .moveDown,
        KeyChord("k"): .moveUp,
        KeyChord("l"): .openDetail,
        KeyChord("g", [.shift]): .listBottom,
        KeyChord("u"): .undo,
        KeyChord("r", [.control]): .redo,
        KeyChord("/"): .searchInView,
        KeyChord(":"): .commandPalette
    ]

    /// The two-key motions. One entry, and the machinery is still worth it:
    /// `gg` is the motion vim users type without thinking, and a `g` that fell
    /// through to the list keymap would do something unrelated instead.
    static let prefixed: [String: AppAction] = ["gg": .listTop]

    /// The prefixes a press can start.
    static let prefixes: Set<Character> = ["g"]
}

/// Normal mode's one piece of state: the half-typed motion.
@MainActor
@Observable
final class VimNormalMode {
    /// Observable so a status line can show a pending `g` rather than leaving
    /// the next keystroke unexplained.
    private(set) var pending: Character?

    init() {}

    func resolve(_ chord: KeyChord) -> VimResolution {
        if let prefix = pending {
            pending = nil
            // Escape cancels a half-typed motion and nothing else — the point
            // of Normal mode's Escape.
            if chord.key == .escape { return .pending }
            if let action = VimKeymap.prefixed[String([prefix, chord.key])] {
                return .run(action)
            }
            // vim discards the prefix and re-reads the key on its own. So do
            // we: `gx` is a `g` thrown away followed by an `x`.
        }
        if let action = VimKeymap.motions[chord] { return .run(action) }
        if chord.modifiers.isEmpty, VimKeymap.prefixes.contains(chord.key) {
            pending = chord.key
            return .pending
        }
        return .unhandled
    }

    /// Forget a half-typed motion — on losing focus, or on the list changing
    /// under it.
    func reset() { pending = nil }
}

/// Per-device keyboard preferences.
///
/// The core's `keyboard.vim_mode`, read through the open vault and written to
/// this device's overlay (#1182), because `docs/07-clients/parity-matrix.md`
/// says it is a local pref and does not sync: a Mac someone drives with vim
/// keys and an iPad they do not are the same account, and the vault's shared
/// value is the wrong place to record the difference. ``bind(to:)`` loads it
/// when a vault opens; until then, and in a test that never binds, the value
/// is the one an earlier build left in `UserDefaults`, or off, and an edit
/// stays in memory.
@MainActor
@Observable
final class KeyboardPreferences {
    var vimMode: Bool {
        didSet { save() }
    }

    /// The core's key, spelled the spec's way.
    static let vimModeKey = "keyboard.vim_mode"

    /// The `UserDefaults` key an earlier build kept it under.
    static let legacyVimModeKey = "editor.vim_mode"

    /// Why the last read or write through the core failed, or `nil`.
    private(set) var errorMessage: String?

    /// Holds the value an earlier build kept, until ``bind(to:)`` moves it
    /// into the core.
    private let defaults: UserDefaults
    /// What `migrate_legacy_defaults` writes client settings into.
    private let directory: URL
    /// The open vault the preference is read from and written to.
    @ObservationIgnored private var bridge: CoreBridge?
    /// Set while ``bind(to:)`` loads, so loading writes nothing back.
    @ObservationIgnored private var loading = false
    /// The write in flight; the next one, and ``saved()``, wait for it.
    @ObservationIgnored private var writing: _Concurrency.Task<Void, Never>?

    init(defaults: UserDefaults = .standard, directory: URL = AppSettings.standardDirectory) {
        self.defaults = defaults
        self.directory = directory
        vimMode = defaults.bool(forKey: Self.legacyVimModeKey)
    }

    /// Bind to the vault that just opened: move the value an earlier build
    /// left in `UserDefaults` into it, once, then read the preference back.
    ///
    /// A migration the core refuses keeps the old key, and the next bind
    /// tries again rather than dropping it.
    func bind(to bridge: CoreBridge) async {
        self.bridge = bridge
        errorMessage = nil
        do {
            try await migrateLegacyValue(into: bridge)
        } catch {
            errorMessage = error.localizedDescription
        }
        do {
            guard case let .preferences(items) = try await bridge.query(.preferences) else { return }
            // A vault switched to while this was reading is the one to show.
            guard self.bridge === bridge else { return }
            var on = false
            if case let .bool(value)? = items.first(where: { $0.key == Self.vimModeKey })?.value {
                on = value
            }
            loading = true
            defer { loading = false }
            vimMode = on
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// Stop writing to the vault: it is closing.
    func unbind() {
        bridge = nil
    }

    /// Wait for the writes in flight to land.
    func saved() async {
        await writing?.value
    }

    /// Write the switch to this device's overlay, after the writes before it.
    private func save() {
        guard !loading, let bridge else { return }
        let command = CoreCommand.setPreference(
            key: Self.vimModeKey,
            value: .bool(value: vimMode),
            target: .device
        )
        let previous = writing
        writing = _Concurrency.Task { @MainActor [weak self] in
            await previous?.value
            self?.errorMessage = nil
            do {
                _ = try await bridge.submit(command)
            } catch {
                self?.errorMessage = error.localizedDescription
            }
        }
    }

    /// Hand the old value to the core's `migrate_legacy_defaults`, and delete
    /// the key once it has written it.
    private func migrateLegacyValue(into bridge: CoreBridge) async throws {
        guard let old = defaults.object(forKey: Self.legacyVimModeKey) as? Bool else { return }
        let value = LegacyDefault(key: Self.legacyVimModeKey, value: .bool(value: old))
        let report = try await bridge.migrateLegacyDefaults([value], directory: directory)
        if !report.unmapped.contains(Self.legacyVimModeKey) {
            defaults.removeObject(forKey: Self.legacyVimModeKey)
        }
    }
}
