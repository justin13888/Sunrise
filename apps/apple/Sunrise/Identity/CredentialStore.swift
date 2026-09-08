import Foundation

/// A completed login's tokens.
///
/// A mirror of the seam's `LoginCredentials` because that one is generated and
/// not `Codable`, and because this one has to redact itself: a struct carrying
/// a live bearer *and* a refresh token will end up in the first log line
/// anyone writes while debugging, unless it cannot.
struct StoredCredentials: Codable, Equatable, Sendable {
    let accessToken: String
    /// Absent when the issuer returns none for public clients, in which case
    /// renewal means signing in again.
    let refreshToken: String?
    /// When the access token stops being accepted, epoch ms.
    let expiresAtMs: UInt64
    /// When to renew: 75% of the token's life, so a failed renewal has room to
    /// be retried before anything breaks.
    let renewAtMs: UInt64

    init(_ credentials: LoginCredentials) {
        accessToken = credentials.accessToken
        refreshToken = credentials.refreshToken
        expiresAtMs = credentials.expiresAtMs
        renewAtMs = credentials.renewAtMs
    }

    init(accessToken: String, refreshToken: String?, expiresAtMs: UInt64, renewAtMs: UInt64) {
        self.accessToken = accessToken
        self.refreshToken = refreshToken
        self.expiresAtMs = expiresAtMs
        self.renewAtMs = renewAtMs
    }

    /// Whether `nowMs` is past the renewal point.
    func needsRenewal(nowMs: UInt64) -> Bool { nowMs >= renewAtMs }

    /// Whether the token is already refused.
    func hasExpired(nowMs: UInt64) -> Bool { nowMs >= expiresAtMs }
}

extension StoredCredentials: CustomStringConvertible, CustomDebugStringConvertible {
    var description: String {
        "StoredCredentials(accessToken: <redacted>, refreshToken: "
            + "\(refreshToken == nil ? "nil" : "<redacted>"), expiresAtMs: \(expiresAtMs))"
    }

    var debugDescription: String { description }
}

/// Where a login's tokens live between launches.
protocol CredentialStore: Sendable {
    func load() throws -> StoredCredentials?
    func save(_ credentials: StoredCredentials) throws
    func clear() throws
}

/// The real one: the same Keychain that holds the vault root, under its own
/// service so a user revoking one does not lose the other.
struct KeychainCredentialStore: CredentialStore {
    static let service = "dev.sunrise.Sunrise.oidc-credentials"

    /// `ThisDeviceOnly`, the same class as the vault root, and the argument is
    /// not that a session is as valuable as a vault. It is that a restored
    /// refresh token is *usable on hardware the account never authorized*, and
    /// nothing downstream stops it:
    ///
    /// - The refresh grant sends no device id. `OidcClient::refresh` in
    ///   `crates/sunrise-auth/src/login.rs` calls `exchange_refresh_token` with
    ///   no `sunrise_device_id` parameter, so the access token it mints carries
    ///   whatever device claim the original authorization had — the *old*
    ///   device's.
    /// - The relay only cross-checks that claim when a device signature is
    ///   presented. `api::signed::verify_bytes` returns `Ok(None)` before
    ///   reaching the claim comparison when neither `X-Sunrise-Device` header
    ///   is present and `require_device_sig` is off — and off is the default,
    ///   and is forced in the single-tenant self-host mode ADR-0027 makes v1's
    ///   shape.
    ///
    /// So on the deployment v1 actually ships, a refresh token lifted out of an
    /// encrypted backup opens a live session against the account. That is the
    /// defect class #42 named: a credential outliving the device it was issued
    /// to.
    ///
    /// **What it costs is close to nothing on the path a user walks.** A device
    /// restored onto new hardware already arrives with no vault root — that is
    /// `KeychainVaultRootStore.accessibility`, and
    /// `docs/03-crypto/recovery.md` §Device backups do not carry the vault root
    /// describes what happens next: the user pairs with a surviving device,
    /// which is a deliberate act on two machines. Signing in again is one more
    /// tap on a screen they are already standing in front of. The asymmetry the
    /// old comment defended bought a saved tap in exchange for a live session
    /// on unauthorized hardware.
    ///
    /// The blast radius of the restored session is bounded and worth stating:
    /// the token reaches the relay, not the plaintext. Ops are sealed to Stream
    /// keys that hang off the vault root, which did not travel. What it does
    /// reach is the account's relay surface — the device list, the blob store,
    /// op metadata — and the ability to push. That is enough.
    static let accessibility = KeychainAccessibility.afterFirstUnlockThisDeviceOnly

    private let item: KeychainItem

    init(account: String = "default") {
        item = KeychainItem(
            service: Self.service,
            account: account,
            accessibility: Self.accessibility
        )
    }

    func load() throws -> StoredCredentials? {
        // As `KeychainVaultRootStore.load` does, and for a reason that is
        // sharper here: `save` rewrites the class on every renewal, so a token
        // that is being renewed heals itself — and a token that is *not* being
        // renewed is exactly the one sitting in a backup. An installation that
        // went offline before this build would otherwise keep the old class for
        // as long as it stays offline.
        //
        // A refusal throws, and `AccountModel` already reads this with `try?`:
        // the user is signed out and signs in again. That is the correct
        // failure for a session — unlike the vault root, nothing is lost —
        // and it is strictly better than handing back a token that is still in
        // the backup-bearing class.
        try item.upgradeAccessibilityIfNeeded()
        guard let data = try item.read() else { return nil }
        // A token written by an older build that cannot be decoded is treated
        // as absent: signing in again is cheap, and refusing to launch over a
        // stale token is not.
        return try? JSONDecoder().decode(StoredCredentials.self, from: data)
    }

    func save(_ credentials: StoredCredentials) throws {
        try item.write(try JSONEncoder().encode(credentials))
    }

    func clear() throws { try item.delete() }
}
