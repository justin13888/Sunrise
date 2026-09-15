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

    /// The launch argument a UI test passes to *keep* the recovery ceremony.
    ///
    /// The ceremony is a sheet over the whole window, presented the moment a
    /// vault is created — and every UI test in both suites starts by creating
    /// one, because the scratch directory is empty and the key store dies with
    /// the process. Left on, it covers the app before the first assertion and
    /// every suite fails at once, which is exactly what happened the first time
    /// this shipped.
    ///
    /// Opt **in** rather than opt out, so a test that says nothing gets the
    /// window it is trying to drive, and the one suite that wants the ceremony
    /// asks for it by name and is the only place it can flake.
    static let recoveryFlag = "-sunrise-ui-test-recovery"

    /// Whether this process should present the recovery ceremony.
    ///
    /// `true` for any launch that is not a UI test, which is every real one.
    static func presentsRecoveryCeremony(
        arguments: [String] = ProcessInfo.processInfo.arguments
    ) -> Bool {
        scratchVault(arguments: arguments) == nil || arguments.contains(recoveryFlag)
    }

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
