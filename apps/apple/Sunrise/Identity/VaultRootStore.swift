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

    private let item: KeychainItem

    init(vaultName: String = "default") {
        item = KeychainItem(service: Self.service, account: vaultName)
    }

    func load() throws -> Data? {
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
