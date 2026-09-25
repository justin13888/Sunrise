import Foundation
import Testing

@testable import Sunrise

/// The widgets' snapshot: what the app writes, when, and when it takes it back.
///
/// The widget extension itself is not under test here — it has no logic to
/// test, which is the point of it. Everything it draws is decided by the code
/// below, against a real vault, so the questions worth asking are all on this
/// side of the App Group.
@MainActor
struct WidgetPublisherTests {
    private func scratchStore() -> WidgetSnapshotStore {
        WidgetSnapshotStore(
            directory: FileManager.default.temporaryDirectory
                .appending(path: "sunrise-widget-tests-\(UUID().uuidString)")
        )
    }

    private func draft(_ title: String, dueAt: TimeValue? = nil) -> TaskDraftIn {
        TaskDraftIn(
            title: title,
            body: nil,
            streamId: nil,
            contexts: [],
            priority: nil,
            energy: nil,
            estimatedDurationS: nil,
            scheduledAt: nil,
            dueAt: dueAt,
            schedulingConstraints: [],
            assignee: nil,
            reminderLeadS: nil
        )
    }

    /// Midnight local, in Unix milliseconds, for the day `nowMs` falls in.
    private func startOfToday(_ nowMs: UInt64) -> Int64 {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = .current
        let day = calendar.startOfDay(for: Date(timeIntervalSince1970: Double(nowMs) / 1000))
        return Int64(day.timeIntervalSince1970 * 1000)
    }

    // MARK: - The store

    @Test
    func aSnapshotSurvivesTheRoundTrip() throws {
        let store = scratchStore()
        defer { try? FileManager.default.removeItem(at: store.directory) }
        let snapshot = WidgetSnapshot(
            writtenAtMs: 1_700_000_000_000,
            outstanding: 2,
            overdue: 1,
            inbox: 3,
            rows: [.init(id: "tsk_1", title: "One", section: .overdue, link: URL(string: "sunrise://x"))]
        )

        try store.write(snapshot)

        #expect(store.read() == snapshot)
    }

    /// A snapshot from another format version reads as absent — which draws
    /// "Open Sunrise" — rather than as a misread.
    @Test
    func aSnapshotFromAnotherVersionReadsAsAbsent() throws {
        let store = scratchStore()
        defer { try? FileManager.default.removeItem(at: store.directory) }
        var snapshot = WidgetSnapshot(writtenAtMs: 0, outstanding: 0, overdue: 0, inbox: 0, rows: [])
        snapshot.version = WidgetSnapshot.currentVersion + 1

        try store.write(snapshot)

        #expect(store.read() == nil)
    }

    @Test
    func erasingIsIdempotent() throws {
        let store = scratchStore()
        defer { try? FileManager.default.removeItem(at: store.directory) }
        try store.write(WidgetSnapshot(writtenAtMs: 0, outstanding: 0, overdue: 0, inbox: 0, rows: []))

        try store.erase()
        try store.erase()

        #expect(store.read() == nil)
        #expect(!FileManager.default.fileExists(atPath: store.file.path))
    }

    @Test
    func theStampAloneDoesNotMakeADifferentWidget() {
        let base = WidgetSnapshot(writtenAtMs: 1, outstanding: 1, overdue: 0, inbox: 0, rows: [])
        var later = base
        later.writtenAtMs = 2
        var different = base
        different.inbox = 1

        #expect(base.drawsTheSameAs(later))
        #expect(!base.drawsTheSameAs(different))
    }

    // MARK: - The projection

    /// The rows are Today's open tasks, in the core's sections, each linking
    /// to itself through a link `DeepLink` parses back.
    @Test
    func theSnapshotIsTodaysOpenTasksInTheCoresSections() async throws {
        let vault = try await TestVault()
        let now = await vault.bridge.nowMs()
        let midnight = startOfToday(now)
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(draft("Due earlier today", dueAt: .instant(at: midnight + 60_000)))
        await list.create(draft("Was due yesterday", dueAt: .instant(at: midnight - 60_000)))
        await list.create(draft("Already done", dueAt: .instant(at: midnight + 120_000)))
        await list.create(draft("Undated, so Inbox"))
        await list.complete(try #require(list.tasks.first { $0.title == "Already done" }))

        let store = scratchStore()
        defer { try? FileManager.default.removeItem(at: store.directory) }
        let publisher = WidgetPublisher(bridge: vault.bridge, store: store, reload: {})
        await publisher.publish()

        let snapshot = try #require(store.read())
        #expect(publisher.errorMessage == nil)
        #expect(snapshot.outstanding == 2, "a completed task is not left to do")
        #expect(snapshot.overdue == 1)
        let sections = Dictionary(uniqueKeysWithValues: snapshot.rows.map { ($0.title, $0.section) })
        #expect(sections == ["Due earlier today": .due, "Was due yesterday": .overdue])
        for row in snapshot.rows {
            let link = try #require(row.link)
            #expect(DeepLink(url: link) == .task(row.id, .open))
        }
        await vault.bridge.shutdown()
    }

    /// Only ``WidgetSnapshot/rowLimit`` titles ever leave the vault, while
    /// the count still says how many there are.
    @Test
    func theRowsAreCappedButTheCountIsNot() async throws {
        let vault = try await TestVault()
        let now = await vault.bridge.nowMs()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        let total = WidgetSnapshot.rowLimit + 2
        for index in 0 ..< total {
            await list.create(draft("Task \(index)", dueAt: .instant(at: startOfToday(now) + 60_000)))
        }

        let store = scratchStore()
        defer { try? FileManager.default.removeItem(at: store.directory) }
        await WidgetPublisher(bridge: vault.bridge, store: store, reload: {}).publish()

        let snapshot = try #require(store.read())
        #expect(snapshot.rows.count == WidgetSnapshot.rowLimit)
        #expect(snapshot.outstanding == total)
        await vault.bridge.shutdown()
    }

    // MARK: - When it writes

    /// WidgetKit budgets reloads on iOS. A publish that finds Today as it
    /// was spends none of that budget.
    @Test
    func anUnchangedTodayIsNotRedrawn() async throws {
        let vault = try await TestVault()
        let store = scratchStore()
        defer { try? FileManager.default.removeItem(at: store.directory) }
        let reloads = ReloadCounter()
        let publisher = WidgetPublisher(bridge: vault.bridge, store: store) { reloads.count += 1 }

        await publisher.publish()
        await publisher.publish()
        #expect(reloads.count == 1)

        let now = await vault.bridge.nowMs()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(draft("New", dueAt: .instant(at: Int64(now))))
        await publisher.publish()
        #expect(reloads.count == 2)
        await vault.bridge.shutdown()
    }

    /// Once stopped, nothing is written — the lock that stopped it has
    /// already erased the file.
    @Test
    func aStoppedPublisherWritesNothing() async throws {
        let vault = try await TestVault()
        let store = scratchStore()
        defer { try? FileManager.default.removeItem(at: store.directory) }
        let publisher = WidgetPublisher(bridge: vault.bridge, store: store, reload: {})

        publisher.stop()
        await publisher.publish()

        #expect(store.read() == nil)
        await vault.bridge.shutdown()
    }

    // MARK: - The surfaces

    /// Attaching starts publishing, and detaching takes the titles back off
    /// disk.
    @Test
    func detachingWithdrawsTheSnapshot() async throws {
        let vault = try await TestVault()
        let store = scratchStore()
        defer { try? FileManager.default.removeItem(at: store.directory) }
        let surfaces = AppSurfaces(widgets: WidgetFeed(store: store, reload: {}))

        surfaces.attach(bridge: vault.bridge)
        let publisher = try #require(surfaces.widgets?.publisher)
        await publisher.publish()
        #expect(store.read() != nil)

        surfaces.detach()

        #expect(surfaces.widgets?.publisher == nil)
        #expect(store.read() == nil)
        await vault.bridge.shutdown()
    }

    /// A lock leaves the vault surfaces bound, but the widgets are withdrawn —
    /// and a publish that was already running when it landed writes nothing.
    @Test
    func withdrawingErasesWithoutDetaching() async throws {
        let vault = try await TestVault()
        let store = scratchStore()
        defer { try? FileManager.default.removeItem(at: store.directory) }
        let surfaces = AppSurfaces(widgets: WidgetFeed(store: store, reload: {}))
        surfaces.attach(bridge: vault.bridge)
        let publisher = try #require(surfaces.widgets?.publisher)
        await publisher.publish()

        surfaces.widgets?.withdraw()
        await publisher.publish()

        #expect(store.read() == nil)
        #expect(surfaces.vault != nil, "only the widgets go; the rest is the session's to release")
        surfaces.detach()
        await vault.bridge.shutdown()
    }

    /// Attaching a second vault takes the first vault's snapshot away before
    /// the second one's is written.
    @Test
    func replacingTheVaultWithdrawsTheOldSnapshot() async throws {
        let vault = try await TestVault()
        let store = scratchStore()
        defer { try? FileManager.default.removeItem(at: store.directory) }
        let feed = WidgetFeed(store: store, reload: {})
        feed.start(bridge: vault.bridge)
        let first = try #require(feed.publisher)
        await first.publish()
        #expect(store.read() != nil)

        feed.start(bridge: vault.bridge)

        #expect(store.read() == nil)
        #expect(first.isStopped)
        #expect(feed.publisher !== first)
        feed.withdraw()
        await vault.bridge.shutdown()
    }

    /// The default publishes nothing, so no test can write into the container
    /// the installed app's widgets read.
    @Test
    func surfacesWithNoFeedPublishNothing() {
        #expect(AppSurfaces().widgets == nil)
    }

    /// Both apps name the group their widgets read, in the form their
    /// platform grants.
    @Test
    func theAppNamesItsAppGroup() throws {
        let group = try #require(
            Bundle.main.object(forInfoDictionaryKey: WidgetSnapshotStore.groupInfoKey) as? String
        )
        #if os(iOS)
        #expect(group == "group.dev.sunrise")
        // iOS hands a container only to a process the group was granted to,
        // so this is the entitlement being in force, not merely declared.
        // (macOS returns a path whether or not it was.)
        #expect(WidgetSnapshotStore.appGroup() != nil)
        #else
        #expect(group.hasSuffix("dev.sunrise"))
        #endif
    }

    /// macOS hands out a container path for any group, granted or not, so
    /// the store reads the grant off the process's signature instead. A
    /// group nobody granted is refused.
    #if os(macOS)
    @Test
    func aGroupThisProcessWasNotGrantedIsRefused() {
        #expect(!WidgetSnapshotStore.isGranted("dev.sunrise.not-granted"))
    }
    #endif
}

@MainActor
private final class ReloadCounter {
    var count = 0
}
