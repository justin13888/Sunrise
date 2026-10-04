import Foundation
import Testing

@testable import Sunrise

/// The recovery-code restore against a stub relay: no network, no browser.
///
/// The stub stands where the seam's `recover_account` stands and answers the
/// way the relay can: a wrong code, a step-up refusal (the relay's `403`), a
/// replay that never caught up, and a restore that worked. What is under test
/// is what the user is shown next in each case, and that the per-word check
/// runs on the Rust wordlist before anything is sent.
@MainActor
struct RestoreFromCodeModelTests {
    /// All-zero entropy, the one 24-word BIP-39 code anybody can write down
    /// from memory. It passes the checksum.
    private static let validCode = Array(repeating: "abandon", count: 23).joined(separator: " ") + " art"

    /// Counts what the stub was asked, and answers as told.
    private final class StubRelay: @unchecked Sendable {
        var signIns = 0
        var restores: [(code: String, bearer: String)] = []
        var signInError: (any Error)?
        var restoreError: (any Error)?
        var steps: [RecoveryStep] = []

        @MainActor
        func model() -> RestoreFromCodeModel {
            RestoreFromCodeModel(
                signIn: {
                    self.signIns += 1
                    if let error = self.signInError { throw error }
                    return "step-up-bearer"
                },
                restore: { code, bearer, onStep in
                    self.restores.append((code, bearer))
                    for step in self.steps { onStep(step) }
                    if let error = self.restoreError { throw error }
                },
                rotateStreamKeys: { 3 }
            )
        }
    }

    @Test
    func eachWordIsCheckedAgainstTheListAsItIsTyped() {
        let model = StubRelay().model()
        model.text = "abandon abandonn ability zebraz"
        #expect(model.wordCount == 4)
        #expect(model.unknownWords == [2, 4], "positions, 1-based, never the words")
        #expect(!model.canRestore)

        model.text = Self.validCode
        #expect(model.unknownWords.isEmpty)
        #expect(model.checksumProblem == nil)
        #expect(model.canRestore)
    }

    /// Twenty-four listed words that fail the checksum are refused before the
    /// sign-in, so a typo never costs the user a trip through the browser.
    @Test
    func aChecksumFailureIsCaughtBeforeAnySignIn() async {
        let relay = StubRelay()
        let model = relay.model()
        model.text = Array(repeating: "abandon", count: 24).joined(separator: " ")
        #expect(model.unknownWords.isEmpty)
        #expect(model.checksumProblem != nil)
        #expect(!model.canRestore)

        await model.restoreAccount()
        #expect(relay.signIns == 0)
        #expect(model.phase == .entering)
    }

    @Test
    func aRestoreThatCatchesUpEndsOnTheAftercareAndForgetsTheWords() async {
        let relay = StubRelay()
        relay.steps = [.blobFetched, .identityOpened, .deviceRegistered(relayDeviceId: "01X"), .caughtUp]
        let model = relay.model()
        model.text = "  " + Self.validCode.uppercased() + "\n"

        await model.restoreAccount()

        #expect(model.phase == .restored)
        #expect(relay.signIns == 1)
        #expect(relay.restores.first?.code == Self.validCode, "normalised before it is sent")
        #expect(relay.restores.first?.bearer == "step-up-bearer")
        #expect(model.text.isEmpty, "the words do not outlive the restore")

        await model.rotateKeys()
        #expect(model.rotatedStreams == 3)
    }

    /// A code that is not this account's: back to the words, and editing them
    /// is the way forward.
    @Test
    func aWrongCodeGoesBackToTheWords() async {
        let relay = StubRelay()
        relay.restoreError = BindingError.RecoveryCode(message: "recovery code: aead open failed")
        let model = relay.model()
        model.text = Self.validCode

        await model.restoreAccount()

        guard case .failed(.wrongCode) = model.phase else {
            Issue.record("expected a wrong-code failure, got \(model.phase)")
            return
        }
        #expect(model.text == Self.validCode, "kept, so the user can correct it")
        model.text = Self.validCode + " "
        #expect(model.phase == .entering)
    }

    /// The relay's `403`: the bearer was not a fresh sign-in. The code was
    /// never the problem, and the screen must not say it was.
    @Test
    func aStepUpRefusalAsksForASignInAgainNotANewCode() async {
        let relay = StubRelay()
        relay.restoreError = BindingError.StepUpRequired(
            message: "recovery: the relay will release the recovery blob only after a fresh sign-in"
        )
        let model = relay.model()
        model.text = Self.validCode

        await model.restoreAccount()

        guard case .failed(.signInRequired) = model.phase else {
            Issue.record("expected a sign-in failure, got \(model.phase)")
            return
        }
        #expect(model.canRestore, "retrying is one tap")
    }

    @Test
    func aSignInThatDidNotCompleteNeverReachesTheRelay() async {
        struct Declined: Error {}
        let relay = StubRelay()
        relay.signInError = Declined()
        let model = relay.model()
        model.text = Self.validCode

        await model.restoreAccount()

        #expect(relay.restores.isEmpty)
        guard case .failed(.signInRequired) = model.phase else {
            Issue.record("expected a sign-in failure, got \(model.phase)")
            return
        }
    }

    /// The replay timed out after the vault was written. That is not a failure
    /// to retry: the account is back, and the rest arrives with sync.
    @Test
    func aReplayTimeoutIsReportedAsRestoredButCatchingUp() async {
        let relay = StubRelay()
        relay.steps = [.blobFetched, .identityOpened, .deviceRegistered(relayDeviceId: "01X")]
        relay.restoreError = BindingError.RecoveryIncomplete(
            message: "recovery: the vault was restored but never finished reading its history"
        )
        let model = relay.model()
        model.text = Self.validCode

        await model.restoreAccount()

        guard case .incomplete = model.phase else {
            Issue.record("expected incomplete, got \(model.phase)")
            return
        }
        #expect(model.text.isEmpty, "the words are spent once the vault is written")
        #expect(!model.canRestore)
    }
}

/// The session half: what is stored, and what is put back, on each outcome.
@MainActor
struct SessionRestoreTests {
    private func scratchDirectory() -> URL {
        FileManager.default.temporaryDirectory
            .appending(path: "sunrise-tests-\(UUID().uuidString)")
    }

    private func request() -> RecoveryRequest {
        RecoveryRequest(relayURL: "https://relay.example", bearer: "b", code: "c", nickname: "n")
    }

    /// A restore that wrote nothing stores no root. A root stored here would
    /// open as a fresh, empty account on the next launch.
    @Test
    func aRefusedRestoreStoresNoRootAndPutsAnUnreadableVaultBack() async throws {
        let directory = scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        try Data("existing".utf8).write(to: directory.appending(path: "vault.db"))

        let store = StubRootStore()
        let session = SessionModel(
            location: VaultLocation(directory: directory),
            rootStore: store,
            appVersion: "test"
        )
        await session.start()
        #expect(session.phase == .locked(.keyMissingForExistingVault))

        await #expect(throws: BindingError.self) {
            try await session.restoreAccount(
                self.request(),
                onStep: { _ in },
                recover: { dir, _, _, _, _ in
                    // The seam creates the directory before it refuses.
                    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
                    throw BindingError.RecoveryCode(message: "recovery code: wrong")
                }
            )
        }

        #expect(store.stored == nil)
        let back = try Data(contentsOf: directory.appending(path: "vault.db"))
        #expect(back == Data("existing".utf8), "the unreadable vault is where it was")
        #expect(session.phase == .locked(.keyMissingForExistingVault))
    }

    /// A vault that would not open can leave files behind, and the restore is
    /// still refused. What it left is unreadable, because its root was never
    /// stored, and it must not keep the original vault from moving back.
    @Test
    func aRefusedRestoreThatLeftFilesStillPutsTheUnreadableVaultBack() async throws {
        let directory = scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        try Data("existing".utf8).write(to: directory.appending(path: "vault.db"))

        let store = StubRootStore()
        let session = SessionModel(
            location: VaultLocation(directory: directory),
            rootStore: store,
            appVersion: "test"
        )
        await session.start()
        #expect(session.phase == .locked(.keyMissingForExistingVault))

        await #expect(throws: BindingError.self) {
            try await session.restoreAccount(
                self.request(),
                onStep: { _ in },
                recover: { dir, _, _, _, _ in
                    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
                    try Data("half-open".utf8).write(to: dir.appending(path: "vault.db"))
                    try Data("lock".utf8).write(to: dir.appending(path: "vault.lock"))
                    throw BindingError.RecoveryRefused(message: "recovery: the vault would not open")
                }
            )
        }

        #expect(store.stored == nil)
        let back = try Data(contentsOf: directory.appending(path: "vault.db"))
        #expect(back == Data("existing".utf8), "the unreadable vault is where it was")
        let leftover = directory.appending(path: "vault.lock").path(percentEncoded: false)
        #expect(!FileManager.default.fileExists(atPath: leftover), "the refused restore's files are gone")
        let siblings = try FileManager.default.contentsOfDirectory(
            atPath: directory.deletingLastPathComponent().path(percentEncoded: false)
        )
        #expect(
            !siblings.contains { $0.hasPrefix("\(directory.lastPathComponent).unreadable-") },
            "nothing is stranded beside the vault"
        )
    }

    /// A restore that wrote the vault and then did not finish (a replay that
    /// timed out, a registration refused, a reopen that failed) still keeps
    /// the root, records the relay id, and opens what it wrote. Discarding the
    /// root here would leave the restored vault unreadable.
    @Test
    func anIncompleteRestoreKeepsTheRootAndOpensTheWrittenVault() async throws {
        let directory = scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = StubRootStore()
        let relayIDs = InMemoryRelayDeviceIDStore()
        let session = SessionModel(
            location: VaultLocation(directory: directory),
            rootStore: store,
            appVersion: "test",
            relayDeviceStore: relayIDs
        )
        await session.start()
        #expect(session.phase == .firstRun)

        await #expect(throws: BindingError.self) {
            try await session.restoreAccount(
                self.request(),
                onStep: { _ in },
                recover: { dir, root, version, _, onStep in
                    // The seam writes the vault under the session's root,
                    // registers, and closes it before reporting the failure.
                    let bridge = try await CoreBridge.open(
                        directory: dir, vaultRoot: root, appVersion: version
                    )
                    onStep(.deviceRegistered(relayDeviceId: "01RELAY"))
                    await bridge.shutdown()
                    throw BindingError.RecoveryIncomplete(message: "recovery: never caught up")
                }
            )
        }

        #expect(store.stored?.count == 32)
        #expect(session.phase == .unlocked)
        #expect(try relayIDs.load()?.id == "01RELAY")
        await session.lock()
    }

    /// A restore that worked stores the root it was handed, records the relay
    /// id the moment it is minted, and opens the vault.
    @Test
    func aRestoreThatWorkedStoresTheRootAndOpens() async throws {
        let directory = scratchDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = StubRootStore()
        let relayIDs = InMemoryRelayDeviceIDStore()
        let session = SessionModel(
            location: VaultLocation(directory: directory),
            rootStore: store,
            appVersion: "test",
            relayDeviceStore: relayIDs
        )
        await session.start()
        #expect(session.phase == .firstRun)

        try await session.restoreAccount(
            request(),
            onStep: { _ in },
            recover: { dir, root, version, _, onStep in
                // A real vault at the directory, under the root the session
                // handed over, standing in for the one the seam restores.
                let bridge = try await CoreBridge.open(directory: dir, vaultRoot: root, appVersion: version)
                onStep(.deviceRegistered(relayDeviceId: "01RELAY"))
                return bridge
            }
        )

        #expect(store.stored?.count == 32)
        #expect(session.phase == .unlocked)
        #expect(try relayIDs.load()?.id == "01RELAY")
        await session.lock()
    }
}
