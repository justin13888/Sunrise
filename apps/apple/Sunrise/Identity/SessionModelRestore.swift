import Foundation

/// The recovery-code restore on screen, if one is (#349).
///
/// Its own type, and its own file, for the reason ``SessionRenewal`` gives:
/// `SessionModel.swift` sits at SwiftLint's `file_length` limit. Held by the
/// session rather than by `OnboardingView` because a successful restore opens
/// the vault, which replaces the onboarding screen, and the aftercare of
/// `docs/03-crypto/recovery.md` §Recovery flow step 7 has to outlive it.
@MainActor
@Observable
final class SessionRestoration {
    var model: RestoreFromCodeModel?
}

extension SessionModel {
    /// Why a restore could not start.
    enum RestoreSetupError: LocalizedError, Equatable {
        case notNow
        case relayNotConfigured
        case signInNotConfigured
        /// The restore wrote nothing, and the unreadable vault it moved aside
        /// could not be moved back. The associated path is where it is.
        case originalNotPutBack(String)

        var errorDescription: String? {
            switch self {
            case .notNow:
                "A vault is already open on this device."
            case .relayNotConfigured:
                """
                Add your relay address in Settings first. The recovery blob \
                your code opens is stored there.
                """
            case .signInNotConfigured:
                """
                Add your sign-in provider in Settings first. The relay \
                releases the recovery blob only after you sign in again.
                """
            case let .originalNotPutBack(path):
                """
                The restore did not complete, and your existing vault could \
                not be moved back into place. Nothing was deleted: it is at \
                \(path).
                """
            }
        }
    }

    /// Whether this phase may restore from a recovery code: a first run, or a
    /// vault on disk whose key is not in this Keychain. Never an open vault.
    var canRestoreFromRecoveryCode: Bool {
        phase == .firstRun || phase == .locked(.keyMissingForExistingVault)
    }

    /// Put the restore on screen, wired to this session.
    func beginRestore() {
        guard canRestoreFromRecoveryCode, restoration.model == nil else { return }
        let settings = AppSettings(defaults: settingsDefaults)
        restoration.model = RestoreFromCodeModel(
            signIn: { try await Self.stepUpBearer(settings: settings) },
            restore: { [weak self] code, bearer, onStep in
                guard let self else { throw RecoverySetupError.vaultClosed }
                let request = RecoveryRequest(
                    relayURL: settings.relayURL.trimmed,
                    bearer: bearer,
                    code: code,
                    nickname: Platform.deviceName
                )
                try await restoreAccount(request, onStep: onStep)
            },
            rotateStreamKeys: { [weak self] in
                guard let bridge = self?.bridge else { throw RecoverySetupError.vaultClosed }
                return try await Self.rotateEveryStreamKey(bridge)
            }
        )
    }

    /// Take the restore off screen.
    func endRestore() {
        restoration.model = nil
    }

    /// Restore the account into this session's vault directory and open it.
    ///
    /// The order is what keeps the user's data:
    ///
    /// 1. A vault already here whose key is missing is moved aside, never
    ///    deleted: a recovery writes a new vault and cannot merge into one.
    /// 2. The new root is generated and handed over, and stored only once the
    ///    seam reports the vault written. A root stored for a restore that
    ///    wrote nothing would open as a fresh, empty account on the next
    ///    launch, and that looks exactly like success.
    /// 3. The relay device id is recorded as soon as the relay mints it, so a
    ///    replay that did not finish can still sync.
    ///
    /// Anything that wrote nothing puts the old directory back.
    ///
    /// `recover` is the seam call; a test passes its own.
    func restoreAccount(
        _ request: RecoveryRequest,
        onStep: @escaping @Sendable (RecoveryStep) -> Void,
        recover: (URL, Data, String, RecoveryRequest, @escaping @Sendable (RecoveryStep) -> Void)
            async throws -> CoreBridge = {
                try await CoreBridge.recover(
                    directory: $0, vaultRoot: $1, appVersion: $2, recovery: $3, onStep: $4
                )
            }
    ) async throws {
        guard canRestoreFromRecoveryCode else { throw RestoreSetupError.notNow }
        guard !request.relayURL.isEmpty else { throw RestoreSetupError.relayNotConfigured }
        let directory = location.directory
        let setAside = try Self.setAside(location)
        let root = try VaultRoot.generate()
        let store = relayDeviceStore
        let scope = RelayDeviceScope(relayURL: request.relayURL, bearer: request.bearer)
        let record: @Sendable (RecoveryStep) -> Void = { step in
            if case let .deviceRegistered(id) = step {
                try? store.store(RelayDeviceBinding(id: id, scope: scope))
            }
            onStep(step)
        }
        do {
            // The seam hands the vault back open. It is closed here and
            // reopened through `open(with:)`, the route every later launch
            // takes, so this session owns it the same way it owns any other.
            let opened = try await recover(directory, root, appVersion, request, record)
            await opened.shutdown()
        } catch {
            if case .RecoveryIncomplete? = error as? BindingError {
                try rootStore.store(root)
                await open(with: root)
            } else {
                // A failed put-back is the louder error: it says where the
                // original vault is.
                try Self.putBack(setAside, at: directory)
            }
            throw error
        }
        try rootStore.store(root)
        await open(with: root)
    }

    /// Move an unreadable vault out of the way, beside where it was, and
    /// return where it went. `nil` when there was nothing to move.
    static func setAside(_ location: VaultLocation, now: Date = .now) throws -> URL? {
        guard location.exists() else { return nil }
        let directory = location.directory
        let stamp = Int(now.timeIntervalSince1970)
        let aside = directory.deletingLastPathComponent()
            .appending(path: "\(directory.lastPathComponent).unreadable-\(stamp)")
        try FileManager.default.moveItem(at: directory, to: aside)
        return aside
    }

    /// Undo ``setAside(_:now:)`` after a restore that wrote nothing.
    ///
    /// Whatever is at `directory` now is the refused restore's own output:
    /// ``setAside(_:now:)`` moved everything that was there before, and a
    /// directory it left alone was empty. A vault that would not open can
    /// leave files behind, and they are unreadable, because the root they
    /// were written under was never stored. They are removed so the original
    /// can move back.
    /// With nothing set aside they are removed all the same, so the next
    /// restore does not find the directory taken.
    static func putBack(_ aside: URL?, at directory: URL) throws {
        let fileManager = FileManager.default
        let occupied = fileManager.fileExists(atPath: directory.path(percentEncoded: false))
        guard let aside else {
            if occupied { try fileManager.removeItem(at: directory) }
            return
        }
        do {
            if occupied { try fileManager.removeItem(at: directory) }
            try fileManager.moveItem(at: aside, to: directory)
        } catch {
            throw RestoreSetupError.originalNotPutBack(aside.path(percentEncoded: false))
        }
    }

    /// Sign in again, now, whatever session the browser holds, and return the
    /// bearer. The relay releases a recovery blob only to a token minted
    /// minutes ago.
    ///
    /// The device id claim is empty: the device the token would name does not
    /// exist until the blob has been opened.
    static func stepUpBearer(settings: AppSettings) async throws -> String {
        let issuer = settings.oidcIssuer.trimmed
        let clientID = settings.oidcClientID.trimmed
        guard !issuer.isEmpty, !clientID.isEmpty else {
            throw RestoreSetupError.signInNotConfigured
        }
        let login = SunriseLogin(issuer: issuer, clientId: clientID)
        let url = try await login.beginStepUp(deviceId: "")
        guard let parsed = URL(string: url) else { throw AccountError.badAuthorizeURL(url) }
        Platform.openExternal(parsed)
        let nowMs = UInt64(Date.now.timeIntervalSince1970 * 1000)
        return try await login.complete(timeoutMs: 5 * 60 * 1000, nowMs: nowMs).accessToken
    }

    /// Rotate every Stream's key, and return how many were rotated.
    ///
    /// The aftercare's second action. A recovery means an unknown-state
    /// environment, and rotation is what bounds what a lost device keeps
    /// reading (`docs/03-crypto/key-rotation.md`).
    static func rotateEveryStreamKey(_ bridge: CoreBridge) async throws -> Int {
        guard case let .streams(rows) = try await bridge.query(.streamList) else { return 0 }
        for row in rows {
            _ = try await bridge.submit(.rotateStreamKey(stream: row.id))
        }
        return rows.count
    }
}
