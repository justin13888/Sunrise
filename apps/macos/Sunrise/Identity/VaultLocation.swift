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
        let support = try fileManager.url(
            for: .applicationSupportDirectory,
            in: .userDomainMask,
            appropriateFor: nil,
            create: true
        )
        return VaultLocation(directory: support.appending(path: "Sunrise/vault"))
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
