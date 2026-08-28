import Foundation
import Security
import Testing

@testable import Sunrise

/// Against the real Keychain, because a fake one proves nothing about the
/// thing this replaces: the mode-0600 file the Rust side deliberately stopped
/// at. Each run uses its own service name and removes it afterwards, so it
/// leaves no residue in the developer's login keychain.
struct KeychainItemTests {
    private func scratch() -> KeychainItem {
        KeychainItem(
            service: "dev.sunrise.Sunrise.tests.\(UUID().uuidString)",
            account: "vault-root"
        )
    }

    @Test
    func aSecretSurvivesBeingWrittenAndReadBack() throws {
        let item = scratch()
        defer { try? item.delete() }

        #expect(try item.read() == nil, "a fresh service must not report a secret")
        try item.write(Data(repeating: 7, count: 32))
        #expect(try item.read() == Data(repeating: 7, count: 32))
    }

    @Test
    func writingTwiceReplacesRatherThanDuplicating() throws {
        let item = scratch()
        defer { try? item.delete() }

        try item.write(Data(repeating: 1, count: 32))
        try item.write(Data(repeating: 2, count: 32))
        #expect(try item.read() == Data(repeating: 2, count: 32))
    }

    /// "Nothing stored" and "cannot read" are different answers, and the
    /// difference decides whether the app generates a new key. Deleting must
    /// leave the first, not the second.
    @Test
    func deletingIsIdempotentAndLeavesNoItem() throws {
        let item = scratch()
        try item.write(Data(repeating: 3, count: 32))
        try item.delete()
        try item.delete()
        #expect(try item.read() == nil)
    }
}

struct VaultRootTests {
    @Test
    func aGeneratedRootIsTheWidthTheSeamRequires() throws {
        let root = try VaultRoot.generate()
        #expect(root.count == 32)
    }

    @Test
    func twoGeneratedRootsDiffer() throws {
        #expect(try VaultRoot.generate() != VaultRoot.generate())
    }

    /// A short root would be rejected at the seam with a less helpful message,
    /// and a long one would be silently truncated by a less careful store.
    @Test
    func aWrongWidthRootIsRefusedBeforeItReachesTheVault() throws {
        let store = KeychainVaultRootStore(vaultName: "tests-\(UUID().uuidString)")
        defer { try? store.clear() }

        #expect(throws: VaultRootError.wrongLength(8)) {
            try store.store(Data(repeating: 0, count: 8))
        }
        #expect(try store.load() == nil)
    }

    @Test
    func aStoredRootComesBackByteForByte() throws {
        let store = KeychainVaultRootStore(vaultName: "tests-\(UUID().uuidString)")
        defer { try? store.clear() }

        let root = try VaultRoot.generate()
        try store.store(root)
        #expect(try store.load() == root)
        try store.clear()
        #expect(try store.load() == nil)
    }
}

struct VaultLocationTests {
    @Test
    func anEmptyDirectoryIsNotAVault() throws {
        let directory = FileManager.default.temporaryDirectory
            .appending(path: "sunrise-tests-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        let location = VaultLocation(directory: directory)

        #expect(!location.exists())
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        #expect(!location.exists(), "an empty directory is a failed first run, not a vault")

        try Data("x".utf8).write(to: directory.appending(path: "sunrise.sqlite"))
        #expect(location.exists())
    }

    @Test
    func theStandardLocationIsNotTheCommandLineClientsVault() throws {
        let location = try VaultLocation.standard()
        let path = location.directory.path(percentEncoded: false)
        #expect(path.hasSuffix("Sunrise/vault"))
        #expect(!path.contains("/.sunrise/"), "two clients must not race for one vault lock")
    }
}
