import Foundation
import Security
import Testing

@testable import Sunrise

/// Against the real Keychain, because a fake one proves nothing about the
/// thing this replaces: the mode-0600 file the Rust side deliberately stopped
/// at. Each run uses its own service name and removes it afterwards, so it
/// leaves no residue in the developer's login keychain.
struct KeychainItemTests {
    private func scratch(
        _ accessibility: KeychainAccessibility = .afterFirstUnlockThisDeviceOnly
    ) -> KeychainItem {
        KeychainItem(
            service: "dev.sunrise.Sunrise.tests.\(UUID().uuidString)",
            account: "vault-root",
            accessibility: accessibility
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

    /// The class an item declares is the class the Keychain records — on the
    /// platform that has classes at all. macOS's file-based login keychain
    /// accepts `kSecAttrAccessible` and stores nothing, which is asserted here
    /// rather than assumed: it is the reason this fix protects iOS and leaves
    /// the Mac where it was.
    @Test
    func aWrittenItemCarriesTheClassItDeclares() throws {
        let item = scratch(.afterFirstUnlockThisDeviceOnly)
        defer { try? item.delete() }
        try item.write(Data(repeating: 4, count: 32))

        #expect(storedAccessibility(of: item) == expectedThisDeviceOnly)
    }

    /// The migration. An item added by a build that used the weaker class keeps
    /// it — `SecItemUpdate` touches only the attributes it is handed, and the
    /// vault root is never rewritten on the ordinary path — so an upgrade that
    /// only changed the constant would leave every existing installation
    /// exactly where it was.
    @Test
    func anItemLeftInTheBackupClassIsRaisedRatherThanLeftAlone() throws {
        let asAnOlderBuildWroteIt = scratch(.afterFirstUnlock)
        defer { try? asAnOlderBuildWroteIt.delete() }
        try asAnOlderBuildWroteIt.write(Data(repeating: 5, count: 32))
        #expect(storedAccessibility(of: asAnOlderBuildWroteIt) == expectedAfterFirstUnlock)

        let asThisBuildWantsIt = KeychainItem(
            service: asAnOlderBuildWroteIt.service,
            account: asAnOlderBuildWroteIt.account,
            accessibility: .afterFirstUnlockThisDeviceOnly
        )
        try asThisBuildWantsIt.upgradeAccessibilityIfNeeded()

        #expect(storedAccessibility(of: asThisBuildWantsIt) == expectedThisDeviceOnly)
        #expect(
            try asThisBuildWantsIt.read() == Data(repeating: 5, count: 32),
            "the bytes must survive it"
        )
    }

    /// First run reaches this before there is anything to raise, and must not
    /// turn "no item" into an error — the distinction decides whether the app
    /// generates a new root over an existing vault.
    @Test
    func raisingTheClassOfAnAbsentItemIsNotAnError() throws {
        let item = scratch()
        try item.upgradeAccessibilityIfNeeded()
        #expect(try item.read() == nil)
    }
}

/// The `kSecAttrAccessible` the Keychain actually recorded, or `nil` when the
/// keychain in use does not implement protection classes.
private func storedAccessibility(of item: KeychainItem) -> String? {
    let query: [String: Any] = [
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: item.service,
        kSecAttrAccount as String: item.account,
        kSecReturnAttributes as String: true,
        kSecMatchLimit as String: kSecMatchLimitOne
    ]
    var found: CFTypeRef?
    guard SecItemCopyMatching(query as CFDictionary, &found) == errSecSuccess,
          let attributes = found as? [String: Any]
    else { return nil }
    return attributes[kSecAttrAccessible as String] as? String
}

// The expectations differ by platform, and the difference is the finding:
// only the data-protection keychain has protection classes, and the Mac app
// does not use it (no App Sandbox, no keychain-access-group entitlement — see
// `project.yml`, where both are deferred to release work). A `SecItemAdd` with
// `kSecUseDataProtectionKeychain` from this app returns `errSecMissingEntitlement`
// (-34018) today. If the Mac ever gains that entitlement, these two lines
// collapse into one and macOS gains the guarantee with them.
#if os(iOS)
private let expectedThisDeviceOnly: String? = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly as String
private let expectedAfterFirstUnlock: String? = kSecAttrAccessibleAfterFirstUnlock as String
#else
private let expectedThisDeviceOnly: String? = nil
private let expectedAfterFirstUnlock: String? = nil
#endif

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

    /// The one assertion that holds on both platforms, and the regression this
    /// guards: the vault root is the key every Stream key and the database key
    /// hang off, so it is the item that must not travel in a backup.
    @Test
    func theVaultRootDeclaresTheClassThatKeepsItOffOtherDevices() {
        #expect(KeychainVaultRootStore.accessibility == .afterFirstUnlockThisDeviceOnly)
    }

    /// An installation that predates the class above is the case a one-line
    /// constant change would have missed entirely: its root stays in the class
    /// it was written under until something rewrites it, and nothing does.
    @Test
    func aRootLeftByAnOlderBuildIsRaisedOnTheNextLoad() throws {
        let vaultName = "tests-\(UUID().uuidString)"
        let asAnOlderBuildWroteIt = KeychainItem(
            service: KeychainVaultRootStore.service,
            account: vaultName,
            accessibility: .afterFirstUnlock
        )
        defer { try? asAnOlderBuildWroteIt.delete() }

        let root = try VaultRoot.generate()
        try asAnOlderBuildWroteIt.write(root)

        let store = KeychainVaultRootStore(vaultName: vaultName)
        #expect(try store.load() == root, "raising the class must not cost the user their vault")
        #expect(storedAccessibility(of: asAnOlderBuildWroteIt) == expectedThisDeviceOnly)
    }
}

/// The OIDC credential, which #131 moved into the vault root's class. Against
/// the real Keychain for the same reason the tests above are: what the class
/// is *recorded* as is the whole claim, and only the platform can answer it.
struct KeychainCredentialStoreTests {
    private func credentials(_ token: String) -> StoredCredentials {
        StoredCredentials(
            accessToken: token,
            refreshToken: "refresh-\(token)",
            expiresAtMs: 2_000,
            renewAtMs: 1_500
        )
    }

    /// The regression #131 decided. A refresh token in `AfterFirstUnlock`
    /// restores onto other hardware, and neither half of the device binding
    /// stops it there: the refresh grant sends no device id, and the relay
    /// cross-checks the token's device claim only when a device signature is
    /// presented — which `require_device_sig` makes optional and the
    /// single-tenant self-host mode forbids.
    @Test
    func theCredentialDeclaresTheClassThatKeepsItOffOtherDevices() {
        #expect(KeychainCredentialStore.accessibility == .afterFirstUnlockThisDeviceOnly)
    }

    @Test
    func aSavedCredentialComesBackAndClearsAway() throws {
        let store = KeychainCredentialStore(account: "tests-\(UUID().uuidString)")
        defer { try? store.clear() }

        #expect(try store.load() == nil)
        try store.save(credentials("access"))
        #expect(try store.load() == credentials("access"))
        try store.clear()
        #expect(try store.load() == nil)
    }

    /// An installation that stopped renewing before this build is exactly the
    /// one sitting in a backup, and `save` — which rewrites the class — is the
    /// thing it is not doing. The raise therefore has to happen on `load`.
    @Test
    func aTokenLeftByAnOlderBuildIsRaisedOnTheNextLoad() throws {
        let account = "tests-\(UUID().uuidString)"
        let asAnOlderBuildWroteIt = KeychainItem(
            service: KeychainCredentialStore.service,
            account: account,
            accessibility: .afterFirstUnlock
        )
        defer { try? asAnOlderBuildWroteIt.delete() }
        try asAnOlderBuildWroteIt.write(JSONEncoder().encode(credentials("access")))
        #expect(storedAccessibility(of: asAnOlderBuildWroteIt) == expectedAfterFirstUnlock)

        let store = KeychainCredentialStore(account: account)
        #expect(try store.load() == credentials("access"), "raising it must not sign the user out")
        #expect(storedAccessibility(of: asAnOlderBuildWroteIt) == expectedThisDeviceOnly)
    }

    /// A credential written under the new class stays under it, which is the
    /// half `save` owns.
    @Test
    func aSavedCredentialCarriesTheClassItDeclares() throws {
        let account = "tests-\(UUID().uuidString)"
        let store = KeychainCredentialStore(account: account)
        defer { try? store.clear() }
        try store.save(credentials("access"))

        let item = KeychainItem(
            service: KeychainCredentialStore.service,
            account: account,
            accessibility: KeychainCredentialStore.accessibility
        )
        #expect(storedAccessibility(of: item) == expectedThisDeviceOnly)
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
