import Foundation

/// Where the vault lives on disk, and whether one is already there.
///
/// Separate from the key store because the two answers combine into the
/// launch state: a directory with no key is a *locked* vault, and a key with
/// no directory is a first run that has already been half-completed.
struct VaultLocation: Sendable {
    let directory: URL

    /// `~/Library/Application Support/Sunrise/vault`.
    ///
    /// Not `~/.sunrise` — the CLI's default — on purpose. Two clients sharing
    /// one directory would race for the vault lock, and the app has no way to
    /// tell the user why it lost.
    static func standard(fileManager: FileManager = .default) throws -> VaultLocation {
        try forVault(VaultRegistry.firstVaultID, fileManager: fileManager)
    }

    /// Where the vault registered under `id` lives.
    ///
    /// The first vault keeps `Sunrise/vault` — the path every existing install
    /// already has — and every vault added afterwards goes under
    /// `Sunrise/vaults/<id>`. Moving the original into the new layout would be
    /// a migration whose failure mode is an unopenable vault, in exchange for
    /// tidiness nobody can see.
    ///
    /// Throws rather than sanitising an unexpected id: every id this app
    /// generates is a UUID, so one that is not is a bug, and quietly rewriting
    /// it into a different path is how a vault gets written to two places.
    static func forVault(_ id: String, fileManager: FileManager = .default) throws -> VaultLocation {
        guard isSafeIdentifier(id) else { throw VaultLocationError.unusableIdentifier(id) }
        let support = try fileManager.url(
            for: .applicationSupportDirectory,
            in: .userDomainMask,
            appropriateFor: nil,
            create: true
        )
        let root = support.appending(path: "Sunrise")
        guard id != VaultRegistry.firstVaultID else {
            return VaultLocation(directory: root.appending(path: "vault"))
        }
        return VaultLocation(directory: root.appending(path: "vaults").appending(path: id))
    }

    /// Lowercase alphanumerics and hyphens only — which is exactly a lowercased
    /// `UUID` string, and excludes `.`, `/` and everything else that could
    /// climb out of the directory it names.
    static func isSafeIdentifier(_ id: String) -> Bool {
        !id.isEmpty && id.count <= 64 && id.allSatisfy {
            $0.isASCII && ($0.isLowercase && $0.isLetter || $0.isNumber || $0 == "-")
        }
    }

    /// Whether a vault has already been created here.
    ///
    /// Presence of the directory, not of any particular file: the storage
    /// layout is the core's business, and a check that named a file would go
    /// stale the next time it changed.
    func exists(fileManager: FileManager = .default) -> Bool {
        var isDirectory: ObjCBool = false
        let found = fileManager.fileExists(
            atPath: directory.path(percentEncoded: false),
            isDirectory: &isDirectory
        )
        guard found, isDirectory.boolValue else { return false }
        let contents = try? fileManager.contentsOfDirectory(atPath: directory.path(percentEncoded: false))
        return !(contents ?? []).isEmpty
    }
}

enum VaultLocationError: Error, Equatable, LocalizedError {
    case unusableIdentifier(String)

    var errorDescription: String? {
        switch self {
        case let .unusableIdentifier(id):
            "\"\(id)\" is not a usable vault identifier."
        }
    }
}
