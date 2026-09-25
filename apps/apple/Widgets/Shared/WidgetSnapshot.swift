import Foundation
#if os(macOS)
import Security
#endif

/// What the widgets draw: the head of Today, as the app last read it.
///
/// **A projection, not a cache of the vault.** The widget extension never
/// opens the vault. It could not: the core holds the vault lock for as long as
/// the app runs, the vault root lives in the app's Keychain items, and a
/// widget's memory ceiling is far below what `Core::open` needs. So the app
/// reads Today through the ordinary seam, projects the few fields a glance
/// needs into this value, and writes it into the App Group container the
/// extension can read. Everything the widget knows is here, and nothing else
/// leaves the SQLCipher vault — see `docs/07-clients/mobile-ios.md`
/// §Widgets for what that costs and why it is bounded.
///
/// Compiled into both apps *and* both widget extensions, so it may import
/// system frameworks and nothing else: no seam types, no SwiftUI, no
/// `DeepLink`. The
/// links a row opens are therefore carried as finished URLs the app built with
/// `DeepLink.url`, rather than re-derived here from a second copy of the
/// scheme.
struct WidgetSnapshot: Codable, Equatable, Sendable {
    /// Bumped whenever a field changes meaning. A widget that reads a snapshot
    /// written by a different version treats it as absent, which draws "open
    /// Sunrise" rather than a misread.
    static let currentVersion = 1

    /// How many rows the snapshot carries. The large family draws the most,
    /// and the rest are cut from here — which is also the ceiling on how many
    /// titles ever leave the vault.
    static let rowLimit = 8

    /// The head of Today, in the order the core returned it.
    struct Row: Codable, Equatable, Sendable, Identifiable {
        /// The task's `EntityRef`.
        let id: String
        let title: String
        let section: Section
        /// `sunrise://task/<id>?action=open`, built by the app.
        let link: URL?
    }

    /// Which part of Today a row sits in. The core's `TodaySection`, decided
    /// by `today_section` in the app and copied here as data — the overdue
    /// boundary is start-of-day-local, and a widget re-deriving it from a
    /// timestamp would disagree with the list behind it every evening.
    enum Section: String, Codable, Sendable {
        case overdue
        case due
        case scheduled
    }

    var version = WidgetSnapshot.currentVersion
    /// When the app wrote this, as Unix milliseconds. The widget prints it as
    /// "updated … ago": the spec asks for the stamp, because a widget cannot
    /// say how stale it is any other way.
    var writtenAtMs: Int64
    /// Every open task in Today, not just the rows carried.
    var outstanding: Int
    var overdue: Int
    var inbox: Int
    /// At most ``rowLimit``.
    var rows: [Row]

    /// Whether two snapshots draw the same widget.
    ///
    /// Everything but the stamp. The app writes — and asks WidgetKit to
    /// reload, which iOS budgets — only when this is false, so a burst of
    /// changes that leaves Today as it was costs nothing.
    func drawsTheSameAs(_ other: WidgetSnapshot) -> Bool {
        var stamped = other
        stamped.writtenAtMs = writtenAtMs
        return self == stamped
    }
}

/// Where the snapshot lives, and the only code that touches that file.
///
/// A directory rather than a hard-coded App Group lookup, so a test can hand it
/// a scratch directory and the app can hand it the group container.
struct WidgetSnapshotStore: Sendable {
    static let fileName = "widget-snapshot.json"

    /// The `Info.plist` key both the app and the extension carry, holding the
    /// App Group identifier `project.yml` gave them.
    ///
    /// Read from the bundle rather than written here, because it differs by
    /// platform and by signing team: iOS requires a `group.` identifier, and a
    /// Developer ID Mac app needs one prefixed with its team to be granted it
    /// with no provisioning profile. `project.yml` is where both are spelled,
    /// next to the entitlement that grants them.
    static let groupInfoKey = "SunriseAppGroup"

    let directory: URL

    var file: URL { directory.appending(path: Self.fileName) }

    /// The store in this process's App Group container, or `nil` when the
    /// bundle declares no group or the process was not granted it.
    ///
    /// **Granted, not merely named.** iOS enforces that itself: its
    /// `containerURL` answers `nil` to a process without the entitlement.
    /// macOS answers with a path either way, so there the grant is read off
    /// this process's own signature. That keeps a build with no entitlements
    /// (`mise run macos-app` signs nothing) from writing into a group
    /// container it was never given — which is also the container a signed,
    /// installed Sunrise's widgets read.
    static func appGroup(bundle: Bundle = .main) -> WidgetSnapshotStore? {
        guard let group = bundle.object(forInfoDictionaryKey: groupInfoKey) as? String,
              !group.isEmpty,
              isGranted(group),
              let directory = FileManager.default.containerURL(
                  forSecurityApplicationGroupIdentifier: group
              ) else { return nil }
        return WidgetSnapshotStore(directory: directory)
    }

    static func isGranted(_ group: String) -> Bool {
        #if os(macOS)
        guard let task = SecTaskCreateFromSelf(nil),
              let value = SecTaskCopyValueForEntitlement(
                  task,
                  "com.apple.security.application-groups" as CFString,
                  nil
              ),
              let groups = value as? [String] else { return false }
        return groups.contains(group)
        #else
        // `containerURL` is the check on iOS; see above.
        return true
        #endif
    }

    /// The snapshot on disk, or `nil` for none, an unreadable one, or one
    /// from a different ``WidgetSnapshot/currentVersion``.
    func read() -> WidgetSnapshot? {
        guard let data = try? Data(contentsOf: file),
              let snapshot = try? JSONDecoder().decode(WidgetSnapshot.self, from: data),
              snapshot.version == WidgetSnapshot.currentVersion else { return nil }
        return snapshot
    }

    /// Replace the snapshot, atomically.
    ///
    /// On iOS the file stays readable after the first unlock since boot, and
    /// not before: a Lock Screen widget has to draw while the phone is
    /// locked, and `.complete` would make it unreadable at exactly that
    /// moment. What it guards is the gap between boot and the first unlock,
    /// which is the class the rest of an iOS app's files are in by default.
    func write(_ snapshot: WidgetSnapshot) throws {
        try FileManager.default.createDirectory(
            at: directory,
            withIntermediateDirectories: true
        )
        let data = try JSONEncoder().encode(snapshot)
        #if os(iOS)
        try data.write(to: file, options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
        #else
        try data.write(to: file, options: .atomic)
        #endif
    }

    /// Remove the snapshot. Called whenever no vault is open, so that locking
    /// Sunrise, signing out or switching vaults takes the titles off the Home
    /// Screen too. Absent already is success.
    func erase() throws {
        do {
            try FileManager.default.removeItem(at: file)
        } catch CocoaError.fileNoSuchFile {
            return
        }
    }
}
