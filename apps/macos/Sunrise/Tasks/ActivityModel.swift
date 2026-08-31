import Foundation

/// One entity's activity feed.
///
/// `Query::ActivityTimeline` has been on the seam since the bridge was built
/// and had no caller: the op log recorded every completion, deferral and move,
/// and the app could not show any of it. This is the reader.
///
/// Nothing here words an event. `ActivityRow.phrase` is
/// `sunrise_domain::activity_phrase`, which is also what the CSV export and
/// the CLI print — so "deferred (3rd time)" reads the same in all three rather
/// than three ways.
@MainActor
@Observable
final class ActivityModel {
    private(set) var rows: [ActivityRow] = []
    private(set) var nowMs: UInt64 = 0
    private(set) var timeZone: String = TimeZone.current.identifier
    private(set) var errorMessage: String?

    /// How far back the feed goes. A cap, not a page: an entity with hundreds
    /// of events has a story in its first screenful, and the rest is the op
    /// log, which the export already offers whole.
    static let limit: UInt32 = 100

    private let bridge: CoreBridge
    private let entity: EntityRef

    init(bridge: CoreBridge, entity: EntityRef) {
        self.bridge = bridge
        self.entity = entity
    }

    func refresh() async {
        timeZone = TimeZone.current.identifier
        nowMs = await bridge.nowMs()
        do {
            guard case let .activity(events) = try await bridge.query(
                .activityTimeline(entity: entity, limit: Self.limit)
            ) else {
                rows = []
                return
            }
            rows = events
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func follow() async {
        for await batch in await bridge.changes() {
            guard !batch.isClosed else { return }
            await refresh()
        }
    }

    /// `today` / `yesterday` / `-3d` for a row, in the domain's words.
    func day(of row: ActivityRow) -> RelativeDay {
        relativeDay(
            at: .instant(at: Int64(row.atMs)),
            nowMs: nowMs,
            tz: timeZone
        )
    }

    /// The clock time a row happened at.
    func clock(of row: ActivityRow) -> String {
        let formatter = DateFormatter()
        formatter.timeZone = TimeZone(identifier: timeZone)
        formatter.dateFormat = "HH:mm"
        return formatter.string(from: Date(timeIntervalSince1970: Double(row.atMs) / 1000))
    }

    /// This device's id, for telling "you did this here" from "another device
    /// did this".
    func deviceIdentifier() async -> String { await bridge.deviceId() }

    /// Whether a row was written by this device.
    ///
    /// Worth showing: an activity feed on a synced vault is a record of what
    /// *every* device did, and "this was you, on the laptop" is the difference
    /// between reading it and being puzzled by it.
    func isThisDevice(_ row: ActivityRow, deviceID: String) -> Bool {
        !deviceID.isEmpty && row.device == deviceID
    }
}
