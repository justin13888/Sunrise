import Foundation
import Testing

@testable import Sunrise

/// The surfaces that outlive the window — the menu bar item, the capture panel
/// and the reminder schedule — and what happens to them when the vault
/// underneath is replaced.
///
/// Real cores, for the same reason `VaultSwitchingTests` uses them:
/// `crates/sunrise-core/src/vault_lock.rs` admits one open vault per process
/// and `switchTo` shuts the old one down, so "this surface is still holding the
/// previous vault" is a runtime fact and not something a type would catch.
@MainActor
struct AppSurfacesTests {
    private func scratchDirectory() -> URL {
        FileManager.default.temporaryDirectory
            .appending(path: "sunrise-tests-\(UUID().uuidString)")
    }

    private func scratchDefaults() throws -> UserDefaults {
        try #require(UserDefaults(suiteName: "sunrise-tests-\(UUID().uuidString)"))
    }

    private struct Fixture {
        let session: SessionModel
        let second: VaultDescriptor
        let directories: [URL]
    }

    /// Two vaults, each with its own directory and its own key store.
    private func fixture() throws -> Fixture {
        let registry = VaultRegistry(defaults: try scratchDefaults())
        let second = registry.add(name: "Work")
        let firstDirectory = scratchDirectory()
        let secondDirectory = scratchDirectory()
        let firstStore = StubRootStore()
        let secondStore = StubRootStore()

        let session = SessionModel(
            vaults: registry,
            appVersion: "test",
            resolve: { descriptor in
                descriptor.id == VaultRegistry.firstVaultID
                    ? VaultBinding(
                        location: VaultLocation(directory: firstDirectory),
                        rootStore: firstStore
                    )
                    : VaultBinding(
                        location: VaultLocation(directory: secondDirectory),
                        rootStore: secondStore
                    )
            }
        )
        return Fixture(
            session: session,
            second: second,
            directories: [firstDirectory, secondDirectory]
        )
    }

    private func clean(_ fixture: Fixture, _ surfaces: AppSurfaces) async {
        surfaces.detach()
        await fixture.session.lock()
        for directory in fixture.directories {
            try? FileManager.default.removeItem(at: directory)
        }
    }

    private func draft(_ title: String) -> TaskDraftIn {
        TaskDraftIn(
            title: title,
            body: nil,
            streamId: nil,
            contexts: [],
            priority: nil,
            energy: nil,
            estimatedDurationS: nil,
            scheduledAt: nil,
            dueAt: nil,
            schedulingConstraints: [],
            assignee: nil,
            reminderLeadS: nil
        )
    }

    private func inboxTitles(_ bridge: CoreBridge) async -> [String] {
        let list = TaskListModel(bridge: bridge, kind: .inbox)
        await list.refresh()
        return list.tasks.map(\.title)
    }

    /// **The one this file exists for.**
    ///
    /// `AppSurfaces.attach` used to return early once it had a `MenuBarModel`,
    /// which was correct while a Mac had one vault. After multi-account
    /// switching landed it meant the capture panel went on holding a bridge to
    /// the `Core` that `switchTo` had already shut down — and the panel's
    /// commit was a `try?`, so a ⌘⇧N capture went nowhere and said nothing.
    /// A user's typed line disappeared.
    @Test
    func aCaptureAfterAVaultSwitchReachesTheNewVault() async throws {
        let fixture = try fixture()
        let session = fixture.session
        let surfaces = AppSurfaces()

        await session.start()
        await session.createVault()
        surfaces.attach(bridge: try #require(session.bridge))
        try await surfaces.commitCapture(draft("Belongs to the first vault"))

        await session.switchTo(fixture.second)
        await session.createVault()
        let second = try #require(session.bridge)
        surfaces.attach(bridge: second)

        try await surfaces.commitCapture(draft("Typed after the switch"))

        #expect(
            await inboxTitles(second) == ["Typed after the switch"],
            "the capture reached the vault that is open, and only it"
        )
        await clean(fixture, surfaces)
    }

    /// Everything bound to a vault is replaced, not kept. A stale `MenuBarModel`
    /// shows the previous vault's counts; a stale `ReminderScheduler` keeps
    /// scheduling the previous vault's alerts.
    @Test
    func attachingANewVaultRebuildsEverySurfaceBoundToTheOldOne() async throws {
        let fixture = try fixture()
        let session = fixture.session
        let surfaces = AppSurfaces()

        await session.start()
        await session.createVault()
        surfaces.attach(bridge: try #require(session.bridge))
        let firstMenuBar = try #require(surfaces.menuBar)
        let firstReminders = try #require(surfaces.reminders)
        let firstVault = try #require(surfaces.vault)

        await session.switchTo(fixture.second)
        await session.createVault()
        surfaces.attach(bridge: try #require(session.bridge))

        #expect(surfaces.menuBar !== firstMenuBar, "the badge would show the old vault's counts")
        #expect(surfaces.reminders !== firstReminders, "it would schedule the old vault's alerts")
        #expect(surfaces.vault !== firstVault)
        #expect(surfaces.vault === session.bridge)
        await clean(fixture, surfaces)
    }

    /// Re-attaching the *same* bridge is still a rebuild, and still leaves the
    /// surfaces usable. A re-render must not cost a capture.
    @Test
    func reattachingTheSameVaultLeavesTheSurfacesWorking() async throws {
        let fixture = try fixture()
        let session = fixture.session
        let surfaces = AppSurfaces()

        await session.start()
        await session.createVault()
        let bridge = try #require(session.bridge)
        surfaces.attach(bridge: bridge)
        surfaces.attach(bridge: bridge)

        try await surfaces.commitCapture(draft("Still lands"))
        #expect(surfaces.menuBar != nil)
        #expect(await inboxTitles(bridge) == ["Still lands"])
        await clean(fixture, surfaces)
    }

    /// The window between one vault closing and the next opening is reachable
    /// now, and ⌘⇧N works from any app. A capture into that window has to say
    /// so — the whole point of the panel is that nothing typed into it is lost.
    @Test
    func aCaptureWithNoOpenVaultIsRefusedRatherThanSwallowed() async throws {
        let surfaces = AppSurfaces()
        #expect(surfaces.vault == nil)

        await #expect(throws: CaptureError.noOpenVault) {
            try await surfaces.commitCapture(draft("Nowhere to go"))
        }
        surfaces.detach()
    }

    /// Detaching lets go of the vault as well as the hotkey, so nothing holds a
    /// bridge to a core that is shutting down.
    @Test
    func detachingLetsGoOfTheVault() async throws {
        let fixture = try fixture()
        let session = fixture.session
        let surfaces = AppSurfaces()

        await session.start()
        await session.createVault()
        surfaces.attach(bridge: try #require(session.bridge))
        #expect(surfaces.vault != nil)

        surfaces.detach()
        #expect(surfaces.vault == nil)
        #expect(surfaces.menuBar == nil)
        #expect(surfaces.reminders == nil)
        #expect(surfaces.ical == nil)
        #expect(surfaces.hotkeyStatus == .idle)
        await clean(fixture, surfaces)
    }

    // MARK: - Routine materialization

    /// **`CoreBridge.startRoutineTimer` had no caller at all.**
    ///
    /// Nothing in the running app started periodic materialization, so
    /// recurrence advanced only when `Core::open` ran it once at unlock or
    /// somebody pressed "Generate now" — a Mac left open across midnight
    /// simply stopped generating. This asserts the call is made, that it
    /// crosses the seam against a real core, and that the vault it names is
    /// the one that is open.
    @Test
    func theRoutineTimerRunsAgainstTheOpenVault() async throws {
        let fixture = try fixture()
        let session = fixture.session
        let surfaces = AppSurfaces()

        await session.start()
        await session.createVault()
        let bridge = try #require(session.bridge)
        #expect(surfaces.routineTimer == .stopped, "nothing has started one yet")

        surfaces.attach(bridge: bridge)
        await surfaces.startRoutineTimer()

        #expect(surfaces.routineTimerIsRunning(against: bridge))
        #expect(surfaces.routineTimer.summary == "Running")
        await clean(fixture, surfaces)
    }

    /// The timer belongs to the `Core`, and a vault switch shuts that `Core`
    /// down. A client that started one timer at launch would leave every vault
    /// opened after the first with no recurrence — and would have no way to
    /// tell, because "a timer is running" was true the whole time.
    @Test
    func aVaultSwitchRestartsTheTimerAgainstTheNewVault() async throws {
        let fixture = try fixture()
        let session = fixture.session
        let surfaces = AppSurfaces()

        await session.start()
        await session.createVault()
        let first = try #require(session.bridge)
        surfaces.attach(bridge: first)
        await surfaces.startRoutineTimer()
        #expect(surfaces.routineTimerIsRunning(against: first))

        await session.switchTo(fixture.second)
        await session.createVault()
        let second = try #require(session.bridge)

        // `attach` alone must not carry the claim over: at this point the old
        // core is shut down and the new one has no timer.
        surfaces.attach(bridge: second)
        #expect(surfaces.routineTimer == .stopped, "the old vault's timer went down with it")
        #expect(!surfaces.routineTimerIsRunning(against: first))

        await surfaces.startRoutineTimer()
        #expect(surfaces.routineTimerIsRunning(against: second))
        #expect(!surfaces.routineTimerIsRunning(against: first))
        await clean(fixture, surfaces)
    }

    /// With no vault there is nothing to start, and saying "running" would be
    /// the same lie in a smaller costume.
    @Test
    func theTimerStaysStoppedWithNoVaultOpen() async {
        let surfaces = AppSurfaces()
        await surfaces.startRoutineTimer()
        #expect(surfaces.routineTimer == .stopped)
        #expect(surfaces.routineTimer.summary == "Not running")
        surfaces.detach()
    }
}
