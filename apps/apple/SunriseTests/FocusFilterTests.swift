import Foundation
import Testing

@testable import Sunrise

private typealias Surface = SystemSurfaceFixture

/// The Focus filter: which streams a Focus is about, and what that does to
/// Today and to the reminder schedule (`mobile-ios.md` §Focus Filters).
@MainActor
struct FocusFilterTests {
    // MARK: - The scope

    /// No pick, or an empty one, is no filter. Read the other way it would be
    /// an empty Today and a silent phone, which nobody picks on purpose.
    @Test
    func anEmptyPickFiltersNothing() {
        #expect(FocusFilter.scope(nil) == nil)
        #expect(FocusFilter.scope([]) == nil)
        #expect(FocusFilter.scope(["str_a", "str_a"]) == ["str_a"])
    }

    /// A row with no stream, or a reminder whose stream could not be read,
    /// is let through: muting it would drop an alert over a failed read.
    @Test
    func anUnknownStreamIsLetThrough() {
        let scope: Set<EntityRef> = ["str_work"]
        #expect(FocusFilter.allows(stream: "str_work", in: scope))
        #expect(!FocusFilter.allows(stream: "str_home", in: scope))
        #expect(FocusFilter.allows(stream: nil, in: scope))
        #expect(FocusFilter.allows(stream: "str_home", in: nil))
    }

    @Test
    func remindersOutsideTheScopeAreMuted() {
        let reminders = [
            Reminder(entity: "tsk_work", kind: .task, fireAt: 1, title: "Work", deferredFrom: nil),
            Reminder(entity: "tsk_home", kind: .task, fireAt: 2, title: "Home", deferredFrom: nil),
            Reminder(entity: "blk_1", kind: .block, fireAt: 3, title: "Block", deferredFrom: nil)
        ]
        let streams = ["tsk_work": "str_work", "tsk_home": "str_home"]
        let kept = FocusFilter.muting(reminders, to: ["str_work"], streams: streams)
        #expect(kept.map(\.title) == ["Work", "Block"], "a block belongs to no stream and still fires")
        #expect(FocusFilter.muting(reminders, to: nil, streams: [:]) == reminders)
    }

    // MARK: - The store

    /// A stream id the core's settings file accepts.
    private static let work: EntityRef = "str_01ARZ3NDEKTSV4RRFFQ69G5FAV"

    /// Device-local and persisted, in the core's client settings file: iOS
    /// runs the filter's intent when the Focus changes, possibly long before
    /// anybody opens Sunrise.
    @Test
    func theStoreRemembersAndLiftsTheFilter() {
        let (defaults, name) = Surface.scratchDefaults()
        let directory = Surface.scratchDirectory()
        defer {
            Surface.discard(name)
            try? FileManager.default.removeItem(at: directory)
        }

        let store = FocusFilterStore(directory: directory, defaults: defaults)
        #expect(store.streams == nil)
        store.apply([(id: Self.work, name: "Work")])
        #expect(store.streams == [Self.work])
        let relaunched = FocusFilterStore(directory: directory, defaults: defaults)
        #expect(relaunched.streams == [Self.work], "survives a relaunch")
        #expect(relaunched.names == [Self.work: "Work"])
        let written = try? clientSettings(dir: directory.path(percentEncoded: false))
        let expected = [FocusFilterStream(stream: Self.work, name: "Work")]
        #expect(written?.focusFilter == expected, "through the core")

        store.apply(nil)
        #expect(store.streams == nil)
        #expect(
            FocusFilterStore(directory: directory, defaults: defaults).streams == nil,
            "a Focus ending lifts it for good"
        )
    }

    /// A filter an earlier build kept in `UserDefaults` moves into the client
    /// settings the first time the store is built, and its key is deleted, so
    /// the second build reads it from the file alone.
    @Test
    func aFilterKeptInUserDefaultsMovesOnceAndItsKeyIsDeleted() {
        let (defaults, name) = Surface.scratchDefaults()
        let directory = Surface.scratchDirectory()
        defer {
            Surface.discard(name)
            try? FileManager.default.removeItem(at: directory)
        }
        defaults.set([Self.work: "Work"], forKey: FocusFilterStore.legacyKey)

        let first = FocusFilterStore(directory: directory, defaults: defaults)
        #expect(first.streams == [Self.work])
        #expect(first.names == [Self.work: "Work"])
        #expect(defaults.object(forKey: FocusFilterStore.legacyKey) == nil, "deleted once written")

        let second = FocusFilterStore(directory: directory, defaults: defaults)
        #expect(second.streams == [Self.work], "read back from the file")
    }

    #if os(iOS)
    /// The Settings subtitle is the client layer's sentence.
    @Test
    func theFilterIsDescribedAsTheCoreDescribesIt() {
        #expect(SunriseFocusFilter.summary(of: []) == "All streams")
        #expect(SunriseFocusFilter.summary(of: ["A", "B", "C", "D"]) == "A, B and 2 more")
    }
    #endif

    // MARK: - Today

    /// Today shows only the scoped streams; a stream list the user opened by
    /// name is not narrowed.
    @Test
    func todayShowsOnlyTheScopedStreams() async throws {
        let vault = try await TestVault()
        let (defaults, name) = Surface.scratchDefaults()
        let directory = Surface.scratchDirectory()
        defer {
            Surface.discard(name)
            try? FileManager.default.removeItem(at: directory)
        }
        let store = FocusFilterStore(directory: directory, defaults: defaults)

        let work = try await Surface.stream("Work", in: vault.bridge)
        let home = try await Surface.stream("Home", in: vault.bridge)
        let now = Int64(await vault.bridge.nowMs())
        _ = try await Surface.task("Write the report", stream: work, scheduledAtMs: now, in: vault.bridge)
        _ = try await Surface.task("Water the plants", stream: home, scheduledAtMs: now, in: vault.bridge)

        let today = TaskListModel(bridge: vault.bridge, kind: .todayAll, focusFilter: store)
        await today.refresh()
        #expect(Set(today.tasks.map(\.title)) == ["Write the report", "Water the plants"])

        store.apply([(id: work, name: "Work")])
        await today.refresh()
        #expect(today.tasks.map(\.title) == ["Write the report"])
        #expect(today.groups.flatMap(\.tasks).map(\.title) == ["Write the report"])

        let homeList = TaskListModel(
            bridge: vault.bridge,
            kind: .stream(id: home, name: "Home"),
            focusFilter: store
        )
        await homeList.refresh()
        #expect(homeList.tasks.map(\.title) == ["Water the plants"])
        #expect(homeList.focusScope == nil)
        await vault.bridge.shutdown()
    }

    // MARK: - Reminders

    /// The scheduler withdraws a muted stream's reminders while the Focus is
    /// on, and puts them back when it ends.
    @Test
    func theSchedulerMutesOtherStreamsAndRestoresThem() async throws {
        let vault = try await TestVault()
        let (defaults, name) = Surface.scratchDefaults()
        let directory = Surface.scratchDirectory()
        defer {
            Surface.discard(name)
            try? FileManager.default.removeItem(at: directory)
        }
        let store = FocusFilterStore(directory: directory, defaults: defaults)
        let center = FakeNotificationCenter()
        let scheduler = ReminderScheduler(
            bridge: vault.bridge,
            preferences: NotificationPreferences(defaults: defaults),
            center: center,
            focusFilter: store
        ) { _ in }

        let work = try await Surface.stream("Work", in: vault.bridge)
        let home = try await Surface.stream("Home", in: vault.bridge)
        let soon = Int64(await vault.bridge.nowMs()) + 600_000
        _ = try await Surface.task("Standup", stream: work, scheduledAtMs: soon, in: vault.bridge)
        _ = try await Surface.task("Dentist", stream: home, scheduledAtMs: soon, in: vault.bridge)

        await scheduler.start()
        #expect(Set(scheduler.scheduled.map(\.title)) == ["Standup", "Dentist"])

        store.apply([(id: work, name: "Work")])
        await scheduler.reconcile()
        #expect(scheduler.scheduled.map(\.title) == ["Standup"])
        let pending = await center.pendingReminders
        #expect(pending.count == 1, "the muted reminder is withdrawn from the OS, not merely not added")

        store.apply(nil)
        await scheduler.reconcile()
        #expect(Set(scheduler.scheduled.map(\.title)) == ["Standup", "Dentist"])
        await vault.bridge.shutdown()
    }

    /// A reminder carries its entity, not its stream. A task's stream is its
    /// own and a routine's is its template's, so a Focus mutes a routine of a
    /// muted stream rather than letting every routine through.
    @Test
    func aReminderIsFiledUnderItsTasksOrItsRoutinesStream() async throws {
        let vault = try await TestVault()
        let work = try await Surface.stream("Work", in: vault.bridge)
        let task = try await Surface.task("Standup", stream: work, in: vault.bridge)
        let routine = try await vault.bridge.submit(.createRoutine(draft: RoutineDraftIn(
            template: Template(
                title: "Weekly report",
                streamId: work,
                contexts: [],
                energy: nil,
                priority: nil,
                estimatedDurationS: nil,
                body: nil
            ),
            rrule: try #require(RecurrenceField(text: "every week").rule),
            timezone: "UTC",
            startsAt: Timestamp(Date().timeIntervalSince1970 * 1000),
            endsAt: nil,
            schedulingConstraints: [],
            catchupPolicy: .skip
        ))).entity

        for entity in [task, routine] {
            let row = try await vault.bridge.query(.entityById(id: entity))
            #expect(ReminderScheduler.stream(of: row) == work, "\(row)")
        }
        #expect(ReminderScheduler.stream(of: .tasks(tasks: [])) == nil)
        await vault.bridge.shutdown()
    }
}
