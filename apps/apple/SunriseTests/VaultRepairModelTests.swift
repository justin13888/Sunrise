import Foundation
import Testing

@testable import Sunrise

/// What a recorded remedy run saw: which actions ran, and what was copied.
@MainActor
private final class RemedyLog {
    var ran: [String] = []
    var copied: String?
    var context = OpenContext(hasBackup: false, canRecover: false, repairTried: false)
}

/// The repair screen's model, over remedies a test stands in for (#1115):
/// it offers what the core's table returns, runs the one picked, and keeps
/// every other one offered when it fails.
@MainActor
struct VaultRepairModelTests {
    private func model(
        _ problem: OpenProblem,
        log: RemedyLog,
        failing: [String: any Error] = [:]
    ) -> VaultRepairModel {
        let run: @MainActor (String) throws -> Void = { name in
            log.ran.append(name)
            if let error = failing[name] { throw error }
        }
        return VaultRepairModel(
            problem: problem,
            message: "the vault would not open",
            appVersion: "9.9.9",
            actions: .init(
                repair: { try run("repair") },
                restoreNewestBackup: { try run("restore") },
                tryAgain: { try run("tryAgain") },
                recoverFromCode: { log.ran.append("recover") },
                context: { log.context },
                copy: { log.copied = $0 }
            )
        )
    }

    @Test
    func aDamagedVaultOffersWhatTheCoresTableReturns() {
        let log = RemedyLog()
        log.context = OpenContext(hasBackup: true, canRecover: true, repairTried: false)
        let repair = model(.damaged, log: log)
        #expect(repair.remedies == [.repair, .restoreBackup, .recoverFromCode, .sendDiagnostics])
        #expect(repair.remedies == CoreBridge.remedies(for: .damaged, context: log.context))
        #expect(!repair.isFatal)
    }

    @Test
    func eachRemedyRunsItsOwnAction() async {
        let log = RemedyLog()
        log.context = OpenContext(hasBackup: true, canRecover: true, repairTried: false)
        let repair = model(.damaged, log: log)
        await repair.perform(.repair)
        await repair.perform(.restoreBackup)
        await repair.perform(.recoverFromCode)
        #expect(log.ran == ["repair", "restore", "recover"])
        #expect(repair.failure == nil)

        let busy = model(.busy, log: log)
        #expect(busy.remedies == [.tryAgain])
        await busy.perform(.tryAgain)
        #expect(log.ran.last == "tryAgain")
    }

    @Test
    func aRemedyThatIsNotOfferedDoesNotRun() async {
        let log = RemedyLog()
        let repair = model(.damaged, log: log)
        #expect(!repair.remedies.contains(.restoreBackup), "no copy beside the vault")
        await repair.perform(.restoreBackup)
        #expect(log.ran.isEmpty)
    }

    /// A repair that left the vault damaged is not offered again, as the
    /// table says, and every other remedy is still there.
    @Test
    func aFailedRepairIsShownAndTheOthersStay() async {
        let log = RemedyLog()
        log.context = OpenContext(hasBackup: true, canRecover: true, repairTried: false)
        let repair = model(
            .damaged,
            log: log,
            failing: ["repair": BindingError.VaultDamaged(message: "still damaged")]
        )
        await repair.perform(.repair)
        #expect(repair.failure?.remedy == .repair)
        #expect(repair.failure?.reason == "still damaged")
        #expect(repair.remedies == [.restoreBackup, .recoverFromCode, .sendDiagnostics])
        #expect(repair.running == nil)
    }

    /// A failure that names no problem of its own leaves the problem, and
    /// so the offer, as it was.
    @Test
    func aFailedRestoreIsShownAndNothingIsHidden() async {
        let log = RemedyLog()
        log.context = OpenContext(hasBackup: true, canRecover: false, repairTried: false)
        let repair = model(.damaged, log: log, failing: ["restore": VaultRepairError.noCopy])
        let before = repair.remedies
        await repair.perform(.restoreBackup)
        #expect(repair.failure == .init(remedy: .restoreBackup, reason: L10n.VaultOpen.noCopy))
        #expect(repair.problem == .damaged)
        #expect(repair.remedies == before)
    }

    /// A repair that could not write its copy names a new problem, and the
    /// screen offers that problem's remedies.
    @Test
    func aRemedyThatNamesANewProblemOffersItsRemedies() async {
        let log = RemedyLog()
        let repair = model(
            .damaged,
            log: log,
            failing: ["repair": BindingError.VaultBackupFailed(message: "disk full")]
        )
        await repair.perform(.repair)
        #expect(repair.problem == .backupFailed)
        #expect(repair.message == "disk full")
        #expect(repair.remedies == [.tryAgain, .sendDiagnostics])
    }

    @Test
    func theDiagnosticsAreCopiedWithTheProblemAndTheBuild() async throws {
        let log = RemedyLog()
        let repair = model(.tooOld, log: log)
        #expect(repair.remedies == [.sendDiagnostics])
        #expect(repair.isFatal)
        await repair.perform(.sendDiagnostics)
        let copied = try #require(log.copied)
        #expect(repair.diagnosticsCopied)
        #expect(copied.contains("Sunrise 9.9.9, core \(coreBuildInfo().coreVersion)"))
        #expect(copied.contains("Open failure: tooOld"))
        #expect(copied.contains("Error: the vault would not open"))
    }

    @Test
    func anUpdateIsAnInstructionAndRunsNothing() async {
        let log = RemedyLog()
        let repair = model(.tooNew, log: log)
        #expect(repair.remedies == [.updateApp])
        #expect(!repair.isFatal)
        await repair.perform(.updateApp)
        #expect(log.ran.isEmpty)
    }

    @Test
    func onlyTheVaultVariantsNameAProblem() {
        #expect(VaultRepairModel.problem(of: BindingError.VaultDamaged(message: "")) == .damaged)
        #expect(VaultRepairModel.problem(of: BindingError.VaultBackupFailed(message: "")) == .backupFailed)
        let migration = BindingError.VaultMigrationFailed(message: "")
        #expect(VaultRepairModel.problem(of: migration) == .migrationFailed)
        #expect(VaultRepairModel.problem(of: BindingError.VaultTooNew(message: "")) == .tooNew)
        #expect(VaultRepairModel.problem(of: BindingError.VaultTooOld(message: "")) == .tooOld)
        #expect(VaultRepairModel.problem(of: BindingError.VaultBusy(message: "")) == .busy)
        #expect(VaultRepairModel.problem(of: BindingError.Core(message: "")) == nil)
        #expect(VaultRepairModel.problem(of: VaultRepairError.noCopy) == nil)
        #expect(VaultRepairModel.reason(of: BindingError.VaultBusy(message: "held")) == "held")
        #expect(VaultRepairModel.reason(of: VaultRepairError.noCopy) == L10n.VaultOpen.noCopy)
    }
}

/// An open that refuses the first `refusals` calls with `error`, then opens
/// for real.
private final class RefusingOpen: @unchecked Sendable {
    private let lock = NSLock()
    private var refusals: Int
    private let error: any Error

    init(refusals: Int, error: any Error) {
        self.refusals = refusals
        self.error = error
    }

    func refuse(_ count: Int) { lock.withLock { refusals = count } }

    func open(_ directory: URL, _ root: Data, _ version: String, _ bundle: Data?) async throws -> CoreBridge {
        let refused = lock.withLock {
            defer { refusals = max(0, refusals - 1) }
            return refusals > 0
        }
        if refused { throw error }
        return try await CoreBridge.open(
            directory: directory, vaultRoot: root, appVersion: version, pairedBundle: bundle
        )
    }
}

/// The session's wiring, through the real core: a refused open lands on
/// the repair screen, the repair and the restore go through the bindings,
/// and a remedy that works opens the vault.
@MainActor
struct SessionRepairTests {
    private static let root = Data(repeating: 7, count: 32)

    private func vault() async throws -> URL {
        let directory = FileManager.default.temporaryDirectory
            .appending(path: "sunrise-tests-\(UUID().uuidString)")
        let made = try await CoreBridge.open(directory: directory, vaultRoot: Self.root, appVersion: "test")
        await made.shutdown()
        return directory
    }

    @Test
    func aRefusedOpenIsRepairedAndItsCopyRestored() async throws {
        let directory = try await vault()
        defer { try? FileManager.default.removeItem(at: directory) }
        let damaged = BindingError.VaultDamaged(message: "integrity check failed")
        let opener = RefusingOpen(refusals: 1, error: damaged)
        let session = SessionModel(
            location: VaultLocation(directory: directory),
            rootStore: StubRootStore(root: Self.root),
            appVersion: "test",
            openBridge: { try await opener.open($0, $1, $2, $3) }
        )

        await session.start()
        guard case .failed = session.phase else {
            Issue.record("a refused open is a failure, not \(session.phase)")
            return
        }
        let repair = try #require(session.openFailure.model)
        #expect(repair.message == "integrity check failed")
        #expect(repair.remedies == [.repair, .sendDiagnostics], "no copy yet and no relay")

        await repair.perform(.repair)
        #expect(session.phase == .unlocked)
        #expect(session.openFailure.model == nil)
        await session.lock()
        #expect(!CoreBridge.backups(in: directory).isEmpty, "the repair kept a copy")

        opener.refuse(1)
        await session.start()
        let again = try #require(session.openFailure.model)
        #expect(again.remedies == [.repair, .restoreBackup, .sendDiagnostics])
        await again.perform(.restoreBackup)
        #expect(session.phase == .unlocked)
        await session.lock()
    }

    @Test
    func anOpenFailureNoProblemNamesHasNoRemedies() async throws {
        let directory = try await vault()
        defer { try? FileManager.default.removeItem(at: directory) }
        let opener = RefusingOpen(refusals: 1, error: BindingError.Core(message: "core: no"))
        let session = SessionModel(
            location: VaultLocation(directory: directory),
            rootStore: StubRootStore(root: Self.root),
            appVersion: "test",
            openBridge: { try await opener.open($0, $1, $2, $3) }
        )
        await session.start()
        #expect(session.phase == .failed(BindingError.Core(message: "core: no").localizedDescription))
        #expect(session.openFailure.model == nil)
    }

    /// The recovery code is offered for a vault no build upgrades, on an
    /// account the relay knows, and puts the restore on screen.
    @Test
    func theRecoveryCodePutsTheRestoreOnScreen() async throws {
        let directory = try await vault()
        defer { try? FileManager.default.removeItem(at: directory) }
        let relayIDs = InMemoryRelayDeviceIDStore()
        try relayIDs.store(RelayDeviceBinding(
            id: "01RELAY",
            scope: RelayDeviceScope(relayURL: "https://relay.example", bearer: "b")
        ))
        let opener = RefusingOpen(refusals: 1, error: BindingError.VaultTooOld(message: "too old"))
        let session = SessionModel(
            location: VaultLocation(directory: directory),
            rootStore: StubRootStore(root: Self.root),
            appVersion: "test",
            relayDeviceStore: relayIDs,
            openBridge: { try await opener.open($0, $1, $2, $3) }
        )
        await session.start()
        let repair = try #require(session.openFailure.model)
        #expect(repair.remedies == [.recoverFromCode, .sendDiagnostics])
        #expect(session.canRestoreFromRecoveryCode)

        await repair.perform(.recoverFromCode)
        #expect(session.restoration.model != nil)
        session.endRestore()
    }
}
