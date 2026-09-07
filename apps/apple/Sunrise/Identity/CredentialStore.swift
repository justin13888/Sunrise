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

    private let item: KeychainItem

    init(account: String = "default") {
        // Not `ThisDeviceOnly`, and deliberately: the relay session is not the
        // vault. What the vault root buys by refusing to travel is that a
        // backup carries no readable data; a bearer token that expires on its
        // own schedule buys much less, and taking it out of backups is a
        // sign-in-again-after-migration decision about sessions rather than
        // part of the key hierarchy. It is left where it was so that this
        // change moves exactly one class.
        item = KeychainItem(
            service: Self.service,
            account: account,
            accessibility: .afterFirstUnlock
        )
    }

    func load() throws -> StoredCredentials? {
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
