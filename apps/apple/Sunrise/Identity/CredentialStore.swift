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
    private let migration: KeychainMigration

    init(account: String = "default") {
        item = KeychainItem(
            service: Self.service,
            account: account,
            accessibility: Self.accessibility,
            domain: KeychainDomain.current
        )
        migration = KeychainMigration(
            source: KeychainItem(
                service: Self.service,
                account: account,
                accessibility: Self.accessibility,
                domain: .login
            ),
            destination: item
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
        // A refusal throws, and `AccountModel.restore()` reports it instead
        // of reading it as an empty store: `.failed`, carrying the Keychain's
        // own sentence, and a record of what *shape* the refusal was. For every
        // shape that may have left a copy of the token unread, the Try again
        // that state offers is spent on a second look at this store, and a
        // login is reached only once the store has answered, and answered
        // nothing. That ordering is the repair: a fresh token written while an
        // unreadable copy of the old one may still be sitting in the other
        // keychain is the pair of disagreeing secrets `save` describes below.
        // `.migrationUnverified` is the one shape held out of that ordering,
        // because there the pair already exists and the login is what collapses
        // it; `AccountModel.mayHaveLeftACopyUnread` is where that is decided.
        // Throwing is still strictly better than handing back a token in the
        // backup-bearing class — it just no longer costs a session to do it.
        //
        // The move between keychains comes first, for the reason
        // `KeychainVaultRootStore.load` gives: the class only starts meaning
        // anything once the item is in a keychain that implements one. That
        // order, and the two answers a migration can give, are
        // `KeychainMigration.loadMigratingIfNeeded`'s.
        guard let data = try migration.loadMigratingIfNeeded() else { return nil }
        // A token written by an older build that cannot be decoded is treated
        // as absent: signing in again is cheap, and refusing to launch over a
        // stale token is not.
        return try? JSONDecoder().decode(StoredCredentials.self, from: data)
    }

    /// Across both domains, as `clear` is, and for the sharper half of the same
    /// reason. `load` reads the other keychain before reporting nothing, so a
    /// save that wrote only this one would leave two tokens under one
    /// `(service, account)`. This store is the one where that happens with **no
    /// user action at all**: `refreshIfNeeded` renews at 75% of the token's
    /// life, so a single launch whose probe failed open writes the fresh token
    /// to the login keychain while the stale one stays in the data-protection
    /// one, and every later launch with a correct probe reads two secrets that
    /// disagree and raises `.migrationUnverified`. `AccountModel.restore()`
    /// reports that one rather than swallowing it, so the user is told which
    /// failure they are in — and the Try again it offers goes to a login, which
    /// is what ends the state. `writeAcrossDomains` writes this domain and then
    /// deletes the other, so a *successful* sign-in collapses the disagreeing
    /// pair to one secret and the next launch loads cleanly. `clear` takes both
    /// copies as well, but Sign out is rendered only under `.signedIn`, so it is
    /// not the reachable remedy — and an earlier revision of this comment
    /// inferred from that that there was no reachable remedy at all. A guard was
    /// built on the strength of that inference, and it made this the one shape
    /// with no way out of it.
    /// See `KeychainItem.writeAcrossDomains`, including why the migration's own
    /// write must not do this.
    ///
    /// **A refused other-domain delete is not a failed save, and this is where
    /// that is decided.** `writeAcrossDomains` writes first and cleans up
    /// second, so it can throw with the token already stored; it says which
    /// case that is by raising `KeychainError.writtenButOtherDomainRefused`,
    /// and only that case. This method is the boundary that owns the question
    /// "was the token stored", because `save` returns `Void` and a throw out of
    /// it is the only answer its callers get. Answering "no" when the bytes are
    /// on disk is what made a dismissed Keychain prompt cost a session: the
    /// renewal path keeps its stale credential and signs the user out at
    /// expiry, deleting the good token on the way; the sign-in path reports a
    /// failure for a login that succeeded. Both are `AccountModel`'s reading of
    /// a throw, and both are correct readings of the wrong signal.
    ///
    /// So the caveat is dropped here rather than carried. It has nowhere to go:
    /// `CredentialStore.save` is `Void`, and widening it to carry a partial
    /// success is the shape #255 is proposing for the sign-out path, across a
    /// protocol with more than one conformer. What is lost by dropping it is
    /// named in `KeychainItem.writeAcrossDomains`: the stale copy survives, and
    /// the next load's migration refuses it with `.migrationUnverified`. That
    /// state is the refused delete's doing, not this `catch`'s — it exists
    /// identically whether this line rethrows or not — and rethrowing adds the
    /// lost session on top of it.
    ///
    /// **This `catch` executes in no test**, and this declares it — one of the
    /// seven listed in `docs/07-clients/desktop.md`, where it is item 5. It is
    /// reached only through `KeychainItem.writeAcrossDomains`'s raise of
    /// `writtenButOtherDomainRefused`, item 1 of that set, so it inherits item
    /// 1's blockers exactly: the raise needs the write to succeed in its own
    /// domain, which on an ad-hoc Mac makes the other domain `.dataProtection`,
    /// whose refusal is the missing-entitlement one and is swallowed before it
    /// can reach here. What it waits on is *reach* **and then a refusal** — only
    /// a build that reaches both keychains can address the item at
    /// `.dataProtection` and so put a refusable `.login` on the far side of the
    /// delete, and that delete must then actually refuse. The refusal is
    /// `KeychainItem.meansTheOtherStoreWasUnreachable` answering `false`, which
    /// is item 3 of that set: this catch is reached only through item 1's raise,
    /// and that raise is constructed only where item 3's `false` arm has already
    /// run. So this inherits **both** blockers, and an earlier revision of this
    /// paragraph said it inherited only the first. Item 3's own blocker — the
    /// refusal having to arrive *mid-case* — is stated as a mechanism on
    /// `KeychainItem.deleteAcrossDomains`, which items 4, 6 and 7 share.
    /// Saying the refusal here is *always* the missing-entitlement one would be
    /// too strong: on such a build it is whatever `.login` answers. What *is*
    /// pinned is the discrimination this rests on —
    /// `aWriteRefusedInItsOwnDomainIsNotReportedAsAPartialSuccess` fails if a
    /// write that stored nothing is labelled as one that stored something.
    func save(_ credentials: StoredCredentials) throws {
        do {
            try item.writeAcrossDomains(try JSONEncoder().encode(credentials))
        } catch let error as KeychainError {
            guard case .writtenButOtherDomainRefused = error else { throw error }
        }
    }

    /// Across both domains. Signing out has to reach the refresh token
    /// wherever `load` could have read it from — a token left in the other
    /// keychain is a live session the user believes they ended.
    func clear() throws { try item.deleteAcrossDomains() }
}
