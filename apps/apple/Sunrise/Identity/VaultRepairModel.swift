import Foundation

/// The ways out of a vault that would not open, if one is on screen (#1115).
///
/// Held by the session rather than by the screen for the reason
/// ``SessionRestoration`` gives: a remedy that works opens the vault, which
/// replaces the screen it ran from.
@MainActor
@Observable
final class SessionOpenFailure {
    var model: VaultRepairModel?
}

/// A vault that exists and would not open, and the remedies the core offers
/// for it.
///
/// The table is the core's, never this type's: what is offered is
/// `openRemedies` for the problem the failed open named
/// (`crates/sunrise-client-core/src/vault_session.rs`), the same table the CLI
/// words under its error. This type runs the one the user picks, keeps the
/// screen standing when it fails, and asks the table again with what the
/// failure changed: a repair that left the vault damaged is not offered
/// twice, and a repair that kept a copy makes that copy one to offer.
@MainActor
@Observable
final class VaultRepairModel {
    /// What each remedy runs. ``SessionModel/makeRepairModel(for:root:)``
    /// wires them to the core; a test passes its own.
    struct Actions {
        /// Repair the vault from its own op log and open it
        /// (`CoreBridge.openRepairing`).
        var repair: @MainActor () async throws -> Void
        /// Put back the newest copy beside the vault and open it
        /// (`CoreBridge.backups`, `CoreBridge.restoreBackup`).
        var restoreNewestBackup: @MainActor () async throws -> Void
        /// Open it again, unchanged.
        var tryAgain: @MainActor () async throws -> Void
        /// Put the recovery-code restore on screen. The restore moves the
        /// vault aside (`CoreBridge.moveAside`) only once it is about to
        /// write, and moves it back when it wrote nothing.
        var recoverFromCode: @MainActor () -> Void
        /// What the table is asked with besides the problem: whether a copy
        /// is beside the vault and whether the account has a code. Read
        /// afresh after every remedy, since a repair writes a copy.
        var context: @MainActor () -> OpenContext
        /// Where the diagnostics go.
        var copy: @MainActor (String) -> Void = { PlatformPasteboard.set($0) }
    }

    /// A remedy that ran and did not work, and why.
    struct RemedyFailure: Equatable {
        let remedy: OpenRemedy
        let reason: String
    }

    /// Why the vault would not open: the open's, then the last remedy's
    /// when that names one.
    private(set) var problem: OpenProblem
    /// The error's own words, for the screen and the diagnostics.
    private(set) var message: String
    /// What is offered now, best first. Never empty.
    private(set) var remedies: [OpenRemedy] = []
    /// The remedy in flight; every other one waits for it.
    private(set) var running: OpenRemedy?
    /// The last remedy that did not work. Shown above the remedies, all of
    /// which stay offered.
    private(set) var failure: RemedyFailure?
    /// The diagnostics were copied.
    private(set) var diagnosticsCopied = false
    /// A repair came back with the vault still damaged.
    private var repairTried = false
    private let appVersion: String
    private let actions: Actions

    init(problem: OpenProblem, message: String, appVersion: String, actions: Actions) {
        self.problem = problem
        self.message = message
        self.appVersion = appVersion
        self.actions = actions
        refresh()
    }

    /// The problem a failed open names, for the `BindingError` variants a
    /// client branches on; `nil` for anything else, which offers nothing a
    /// screen of remedies could hold.
    nonisolated static func problem(of error: any Error) -> OpenProblem? {
        switch error as? BindingError {
        case .VaultDamaged?: .damaged
        case .VaultBackupFailed?: .backupFailed
        case .VaultMigrationFailed?: .migrationFailed
        case .VaultTooNew?: .tooNew
        case .VaultTooOld?: .tooOld
        case .VaultBusy?: .busy
        default: nil
        }
    }

    /// The core's own words for `error`: the message a `BindingError`
    /// carries, rather than the generated description that wraps it in the
    /// variant's name.
    nonisolated static func reason(of error: any Error) -> String {
        switch error as? BindingError {
        case let .VaultDamaged(message)?, let .VaultBackupFailed(message)?,
             let .VaultMigrationFailed(message)?, let .VaultTooNew(message)?,
             let .VaultTooOld(message)?, let .VaultBusy(message)?, let .Core(message)?:
            message
        default:
            error.localizedDescription
        }
    }

    /// Nothing here mends it: the only remedy is the diagnostics.
    var isFatal: Bool { CoreBridge.isFatal(remedies) }

    /// Run `remedy`. A remedy that works opens the vault and the session
    /// takes this screen down; one that does not is reported here, and the
    /// rest stay offered.
    func perform(_ remedy: OpenRemedy) async {
        guard running == nil, remedies.contains(remedy) else { return }
        guard let open = opening(remedy) else {
            // `updateApp` is an instruction, not an action: the screen says
            // what to do.
            if remedy == .recoverFromCode { actions.recoverFromCode() }
            if remedy == .sendDiagnostics {
                actions.copy(diagnostics)
                diagnosticsCopied = true
            }
            return
        }
        failure = nil
        running = remedy
        defer { running = nil }
        do {
            try await open()
        } catch {
            let named = Self.problem(of: error)
            if remedy == .repair, named == .damaged { repairTried = true }
            if let named {
                problem = named
                message = Self.reason(of: error)
            }
            failure = RemedyFailure(remedy: remedy, reason: Self.reason(of: error))
            refresh()
        }
    }

    /// What a report needs: the build, the problem, the error and what was
    /// offered. Plain and untranslated, for whoever reads the report; it
    /// holds no task, key or token.
    var diagnostics: String {
        let core = coreBuildInfo()
        var lines = [
            "Sunrise \(appVersion), core \(core.coreVersion) (storage v\(core.storageV), "
                + "doc schema v\(core.docSchemaV), wire v\(core.wireProtoV))",
            "OS: \(ProcessInfo.processInfo.operatingSystemVersionString)",
            "Open failure: \(problem)",
            "Error: \(message)",
            "Offered: \(remedies.map { "\($0)" }.joined(separator: ", "))"
        ]
        if let failure {
            lines.append("Last remedy: \(failure.remedy) did not work: \(failure.reason)")
        }
        return lines.joined(separator: "\n")
    }

    /// What `remedy` runs when it is one that opens the vault; `nil` for one
    /// that does not.
    private func opening(_ remedy: OpenRemedy) -> (@MainActor () async throws -> Void)? {
        switch remedy {
        case .repair: actions.repair
        case .restoreBackup: actions.restoreNewestBackup
        case .tryAgain: actions.tryAgain
        case .recoverFromCode, .sendDiagnostics, .updateApp: nil
        }
    }

    private func refresh() {
        var context = actions.context()
        context.repairTried = repairTried
        remedies = CoreBridge.remedies(for: problem, context: context)
    }
}

/// Why a remedy could not start.
enum VaultRepairError: LocalizedError, Equatable {
    /// The copy the screen offered is no longer beside the vault.
    case noCopy

    var errorDescription: String? {
        switch self {
        case .noCopy: L10n.VaultOpen.noCopy
        }
    }
}

extension SessionModel {
    /// The remedies for an open of ``location`` under `root` that threw
    /// `error`, wired to this session; `nil` when the error names no problem
    /// a remedy is for.
    ///
    /// Each remedy opens through ``reopen(_:)``, so one that works lands on
    /// `.unlocked` like any other open, and one that fails throws back to the
    /// screen instead of replacing it. A pairing's `bundle` goes with every
    /// ordinary reopen: it is spent only on an open that worked. The repair
    /// takes none, as the binding says: a damaged vault has joined its
    /// account already.
    func makeRepairModel(for error: any Error, root: Data, bundle: Data?) -> VaultRepairModel? {
        guard let problem = VaultRepairModel.problem(of: error) else { return nil }
        let directory = location.directory
        let appVersion = self.appVersion
        let relayDevices = relayDeviceStore
        let reopenAsBefore: @MainActor (SessionModel) async throws -> Void = { session in
            try await session.reopen { try await session.openBridge(directory, root, appVersion, bundle) }
        }
        let actions = VaultRepairModel.Actions(
            repair: { [weak self] in
                try await self?.reopen {
                    try await CoreBridge.openRepairing(
                        directory: directory, vaultRoot: root, appVersion: appVersion
                    )
                }
            },
            restoreNewestBackup: { [weak self] in
                guard let self else { return }
                guard let newest = CoreBridge.backups(in: directory).first else {
                    throw VaultRepairError.noCopy
                }
                _ = try CoreBridge.restoreBackup(newest.path, in: directory)
                try await reopenAsBefore(self)
            },
            tryAgain: { [weak self] in
                guard let self else { return }
                try await reopenAsBefore(self)
            },
            recoverFromCode: { [weak self] in self?.beginRestore() },
            context: {
                OpenContext(
                    hasBackup: !CoreBridge.backups(in: directory).isEmpty,
                    // As the CLI asks it: a device the relay knows is on an
                    // account a recovery code restores.
                    canRecover: (try? relayDevices.load()) != nil,
                    repairTried: false
                )
            }
        )
        return VaultRepairModel(
            problem: problem,
            message: VaultRepairModel.reason(of: error),
            appVersion: appVersion,
            actions: actions
        )
    }
}
