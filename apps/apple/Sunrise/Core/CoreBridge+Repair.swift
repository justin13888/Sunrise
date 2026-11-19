import Foundation

// A vault that exists and will not open (#1115), in a file of its own because
// `CoreBridge.swift` sits at the length this project lints for.
//
// `docs/04-storage/migrations.md` §Migration rigor rules 2 to 4. Each is the
// binding of the same name in `crates/sunrise-core-bindings/src/
// vault_repair.rs`, and none of them deletes anything: the repair works under
// a byte for byte copy of the file, a restore moves the vault it replaces
// aside, and a vault set aside is moved whole.
extension CoreBridge {
    /// Open the vault at `directory` that `open` refused as damaged,
    /// repairing it from its own op log under a copy beside it
    /// (`SunriseCore.openRepairing`). Creates nothing: a directory with no
    /// vault is an error.
    static func openRepairing(
        directory: URL,
        vaultRoot: Data,
        appVersion: String
    ) async throws -> CoreBridge {
        let core = try await SunriseCore.openRepairing(
            vaultDir: directory.path(percentEncoded: false),
            vaultRoot: vaultRoot,
            appVersion: appVersion
        )
        return await made(core)
    }

    /// The ways out of `problem`, best first (`openRemedies`): the table the
    /// CLI words too, so the two never offer different things.
    static func remedies(for problem: OpenProblem, context: OpenContext) -> [OpenRemedy] {
        openRemedies(problem: problem, context: context)
    }

    /// Whether `remedies` offers nothing but diagnostics (`openFailureIsFatal`).
    static func isFatal(_ remedies: [OpenRemedy]) -> Bool {
        openFailureIsFatal(remedies: remedies)
    }

    /// The copies of the closed vault in `directory` kept beside it, newest
    /// first (`vaultBackups`).
    static func backups(in directory: URL) -> [VaultBackupItem] {
        vaultBackups(vaultDir: directory.path(percentEncoded: false))
    }

    /// Put the copy at `backup` back in the place of the closed vault in
    /// `directory`, and return where the vault it replaced was moved
    /// (`restoreVaultBackup`).
    static func restoreBackup(_ backup: String, in directory: URL) throws -> String? {
        try restoreVaultBackup(vaultDir: directory.path(percentEncoded: false), backupPath: backup)
    }

    /// Move the closed vault directory `directory` aside whole, and return
    /// where it went (`setVaultAside`): what a restore from the recovery code
    /// needs before it writes into the same place.
    static func moveAside(_ directory: URL) throws -> URL {
        URL(filePath: try setVaultAside(vaultDir: directory.path(percentEncoded: false)))
    }
}
