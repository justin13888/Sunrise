import Foundation
import Security

/// Where the 32-byte vault root key lives between launches.
///
/// A protocol so the session state machine can be tested without touching the
/// developer's login Keychain — and so the three answers it can give
/// (a key, no key, or "I cannot tell you") stay explicit.
protocol VaultRootStore: Sendable {
    /// The stored root, or `nil` when this machine has never had one.
    /// Throws when a root may exist but cannot be read.
    func load() throws -> Data?
    /// Store `root`, replacing any previous value.
    func store(_ root: Data) throws
    /// Forget the root. The vault on disk becomes unreadable.
    func clear() throws
}

/// The real one.
struct KeychainVaultRootStore: VaultRootStore {
    /// Shown as "Sunrise vault key" in Keychain Access, which is what a user
    /// deleting it by hand will be looking at.
    static let service = "dev.sunrise.Sunrise.vault-root"

    /// `ThisDeviceOnly`, because the whole product rests on this key not
    /// leaving the machine that minted it. Every Stream key and the SQLCipher
    /// key hang off the vault root, so a root that travels in a backup is the
    /// vault travelling in a backup — the guarantee
    /// `docs/03-crypto/recovery.md` already states ("in one machine's Keychain
    /// and in no backup") and onboarding already promises. `AfterFirstUnlock`
    /// rather than `WhenUnlocked` for the reason the earlier constant was
    /// chosen: the vault has to open before the user is asked for anything,
    /// which includes before they have unlocked the screen after a reboot on a
    /// machine that auto-launches the app.
    ///
    /// The cost is real and is the point: a device restored from a backup
    /// arrives without this key, and its vault is unreadable until a surviving
    /// paired device sends the root over. `docs/03-crypto/recovery.md`
    /// §Device backups do not carry the vault root is where that is written
    /// down for a user or a support reader.
    static let accessibility = KeychainAccessibility.afterFirstUnlockThisDeviceOnly

    private let item: KeychainItem

    init(vaultName: String = "default") {
        item = KeychainItem(
            service: Self.service,
            account: vaultName,
            accessibility: Self.accessibility
        )
    }

    func load() throws -> Data? {
        // Before the read, because an installation that predates the class
        // above still holds its root under the older one and nothing else on
        // this path would ever rewrite it.
        try item.upgradeAccessibilityIfNeeded()
        guard let data = try item.read() else { return nil }
        guard data.count == VaultRoot.byteCount else {
            throw VaultRootError.wrongLength(data.count)
        }
        return data
    }

    func store(_ root: Data) throws {
        guard root.count == VaultRoot.byteCount else {
            throw VaultRootError.wrongLength(root.count)
        }
        try item.write(root)
    }

    func clear() throws { try item.delete() }
}

enum VaultRootError: Error, Equatable {
    /// A root that is not 32 bytes cannot key a vault, and the seam would
    /// reject it anyway; catching it here says *which* root was wrong.
    case wrongLength(Int)
    /// The system refused to produce random bytes.
    case randomnessUnavailable(OSStatus)
}

extension VaultRootError: LocalizedError {
    var errorDescription: String? {
        switch self {
        case let .wrongLength(count):
            "A vault key must be \(VaultRoot.byteCount) bytes; this one is \(count)."
        case .randomnessUnavailable:
            "The system could not generate a new vault key."
        }
    }
}

/// The vault root key itself.
enum VaultRoot {
    /// Fixed by the crypto suite; the seam rejects anything else.
    static let byteCount = 32

    /// A fresh root from the system CSPRNG.
    ///
    /// Generated here and nowhere else. There is no passphrase derivation in
    /// v1: the key is random, the Keychain holds it, and a second device gets
    /// it by pairing rather than by the user retyping anything.
    static func generate() throws -> Data {
        var bytes = [UInt8](repeating: 0, count: byteCount)
        let status = SecRandomCopyBytes(kSecRandomDefault, byteCount, &bytes)
        guard status == errSecSuccess else {
            throw VaultRootError.randomnessUnavailable(status)
        }
        return Data(bytes)
    }
}
