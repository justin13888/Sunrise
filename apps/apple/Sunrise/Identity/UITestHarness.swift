#if DEBUG
import Foundation

/// The one hook the app offers a UI test.
///
/// A UI test drives the real window, which means the real `SessionModel` — and
/// that must not open the developer's vault or write to their login Keychain.
/// The launch argument redirects both to a scratch directory the test owns.
///
/// `#if DEBUG` and an explicit launch argument, so nothing in a release build
/// can reach it and nothing in a normal run can trip over it.
enum UITestHarness {
    /// The launch argument a UI test passes, followed by a directory path.
    static let flag = "-sunrise-ui-test-vault"

    /// The scratch vault directory this process was launched with, if any.
    static func scratchVault(
        arguments: [String] = ProcessInfo.processInfo.arguments
    ) -> URL? {
        guard let index = arguments.firstIndex(of: flag),
              arguments.index(after: index) < arguments.endIndex else { return nil }
        return URL(filePath: arguments[arguments.index(after: index)])
    }
}

/// A key store that lives and dies with the process.
///
/// Deliberately not persisted: a UI test that left a key behind would be
/// indistinguishable, on the next run, from a developer's real vault.
final class InMemoryVaultRootStore: VaultRootStore, @unchecked Sendable {
    private let lock = NSLock()
    private var root: Data?

    func load() throws -> Data? {
        lock.lock()
        defer { lock.unlock() }
        return root
    }

    func store(_ root: Data) throws {
        lock.lock()
        defer { lock.unlock() }
        self.root = root
    }

    func clear() throws {
        lock.lock()
        defer { lock.unlock() }
        root = nil
    }
}
#endif
