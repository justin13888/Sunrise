import AppIntents

/// The intents the system offers without anybody assembling anything.
///
/// An `AppIntent` on its own is a *building block*: it appears in the
/// Shortcuts editor for someone who goes looking. An `AppShortcut` is the
/// finished thing — it is what puts the action in Spotlight, in the Shortcuts
/// app's gallery for this app, and behind a spoken phrase, on first launch,
/// with nothing built by hand. `parity-matrix.md` asks for an *automation
/// surface*, and a surface nobody can find is not one.
///
/// Every phrase has to contain `\(.applicationName)`; the system rejects a
/// provider whose phrases do not. That is also why the phrases read the way
/// they do — "in Sunrise", "from Sunrise" — rather than as bare commands.
///
/// The cap is ten. Six are used, which leaves room for the next verb without
/// having to argue about which one to drop.
///
/// The short titles name string-catalog keys as literals, as the intents'
/// metadata does (see the note on ``CaptureTaskIntent/title``). The phrases
/// stay English literals on purpose: Siri matches them, Xcode extracts them
/// into an `AppShortcuts.xcstrings` of its own keyed by the phrase, and that
/// mechanism — not `i18n/en.toml` — is what translates them.
struct SunriseShortcuts: AppShortcutsProvider {
    /// Yellow: the app's mark is a sun.
    static let shortcutTileColor = ShortcutTileColor.orange

    static var appShortcuts: [AppShortcut] {
        AppShortcut(
            intent: CaptureTaskIntent(),
            phrases: [
                "Capture in \(.applicationName)",
                "Add a task to \(.applicationName)",
                "New \(.applicationName) task",
                "Capture a \(.applicationName) task"
            ],
            shortTitle: LocalizedStringResource("apple.intents.shortcuts.capture", table: "Localizable"),
            systemImageName: "square.and.pencil"
        )
        AppShortcut(
            intent: CompleteTaskIntent(),
            phrases: [
                "Complete a \(.applicationName) task",
                "Mark a \(.applicationName) task done",
                "Finish a task in \(.applicationName)"
            ],
            shortTitle: LocalizedStringResource("apple.intents.shortcuts.complete", table: "Localizable"),
            systemImageName: "checkmark.circle"
        )
        AppShortcut(
            intent: TodayTasksIntent(),
            phrases: [
                "What's on my \(.applicationName) list today",
                "\(.applicationName) today",
                "Show my \(.applicationName) day"
            ],
            shortTitle: LocalizedStringResource("apple.intents.shortcuts.today", table: "Localizable"),
            systemImageName: "sun.max"
        )
        AppShortcut(
            intent: InboxTasksIntent(),
            phrases: [
                "What's in my \(.applicationName) inbox",
                "Show my \(.applicationName) inbox",
                "\(.applicationName) inbox"
            ],
            shortTitle: LocalizedStringResource("apple.intents.shortcuts.inbox", table: "Localizable"),
            systemImageName: "tray"
        )
        AppShortcut(
            intent: StartFocusIntent(),
            phrases: [
                "Start a \(.applicationName) focus session",
                "Focus with \(.applicationName)",
                "Start focusing in \(.applicationName)"
            ],
            shortTitle: LocalizedStringResource("apple.intents.shortcuts.start_focus", table: "Localizable"),
            systemImageName: "timer"
        )
        AppShortcut(
            intent: EndFocusIntent(),
            phrases: [
                "End my \(.applicationName) focus session",
                "Stop focusing in \(.applicationName)"
            ],
            shortTitle: LocalizedStringResource("apple.intents.shortcuts.end_focus", table: "Localizable"),
            systemImageName: "stop.circle"
        )
    }
}
