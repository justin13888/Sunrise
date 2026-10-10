import Foundation

/// What an active Focus filter lets through, as arithmetic on stream ids.
///
/// `docs/07-clients/mobile-ios.md` §Focus Filters: while a Focus that carries
/// Sunrise's filter is on, Today shows only the configured streams and reminders
/// from the other streams are muted. Every rule about *which* rows that means
/// lives here, pure, so the list and the scheduler cannot disagree about it.
///
/// A scope of `nil` is "no filter". It is never the empty set: a Focus whose
/// filter names no stream is read as one that filters nothing, because the
/// other reading — an empty Today and a silent phone — is a state nobody picks
/// on purpose and nothing on screen would explain.
enum FocusFilter {
    /// The scope a picked set of streams means.
    static func scope(_ picked: [EntityRef]?) -> Set<EntityRef>? {
        guard let picked, !picked.isEmpty else { return nil }
        return Set(picked)
    }

    /// Whether a row filed in `stream` is shown, or its reminder fires.
    ///
    /// An unknown stream is let through. That is a reminder whose task could
    /// not be read, or a time block, which belongs to no stream; muting either
    /// would drop an alert because of a read that failed or a field that does
    /// not exist, and a reminder that silently never fires is the worse
    /// mistake.
    static func allows(stream: EntityRef?, in scope: Set<EntityRef>?) -> Bool {
        guard let scope, let stream else { return true }
        return scope.contains(stream)
    }

    /// Today's rows, narrowed to the scope, in the order they came.
    static func scoping(_ tasks: [TaskItem], to scope: Set<EntityRef>?) -> [TaskItem] {
        guard scope != nil else { return tasks }
        return tasks.filter { allows(stream: $0.streamId, in: scope) }
    }

    /// The reminders the scope leaves audible, in the order they came.
    ///
    /// `streams` maps a reminder's entity to the stream it is filed in; an
    /// entity missing from it is let through, by ``allows(stream:in:)``.
    static func muting(
        _ reminders: [Reminder],
        to scope: Set<EntityRef>?,
        streams: [EntityRef: EntityRef]
    ) -> [Reminder] {
        guard scope != nil else { return reminders }
        return reminders.filter { allows(stream: streams[$0.entity], in: scope) }
    }
}

/// The Focus filter in force on this device.
///
/// **Device-local, and never synced.** A Focus is a fact about one phone at
/// one moment — the Mac on the desk is not in the phone's Work Focus — so the
/// filter lives in `UserDefaults` beside ``ListOrderStore`` and
/// `NotificationPreferences`, and nothing about it reaches the vault.
///
/// Persisted rather than held in memory because iOS runs the Focus filter's
/// intent when the Focus changes, which may launch Sunrise in the background
/// and end it again long before the user opens it.
@MainActor
@Observable
final class FocusFilterStore {
    /// The store the app and the Focus filter intent share.
    static let shared = FocusFilterStore()

    static let streamsKey = "focus.filter.streams"

    /// The streams in scope, or `nil` with no filter on.
    private(set) var streams: Set<EntityRef>?
    /// The names the filter was configured with, by id. Kept so a filter can
    /// still be described, and re-applied, when the vault cannot be read.
    private(set) var names: [EntityRef: String] = [:]

    private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        let stored = defaults.dictionary(forKey: Self.streamsKey) as? [String: String] ?? [:]
        names = stored
        streams = FocusFilter.scope(Array(stored.keys))
    }

    /// Put a filter in force, or lift it with `nil` or an empty list.
    func apply(_ picked: [(id: EntityRef, name: String)]?) {
        let named = Dictionary(
            (picked ?? []).map { ($0.id, $0.name) },
            uniquingKeysWith: { first, _ in first }
        )
        names = named
        streams = FocusFilter.scope(Array(named.keys))
        if named.isEmpty {
            defaults.removeObject(forKey: Self.streamsKey)
        } else {
            defaults.set(named, forKey: Self.streamsKey)
        }
    }
}
