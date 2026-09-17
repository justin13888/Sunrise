import Foundation
import Security

/// When the Keychain will release an item, and — the half this app is
/// deciding — whether the item may leave the device inside a backup.
///
/// Spelled as an enum rather than passing `kSecAttrAccessible` values around
/// because the two constants below differ in one respect that no reviewer
/// should have to recall from memory, and because the `CFString` constants
/// cannot be stored in a `Sendable` struct.
enum KeychainAccessibility: Sendable, Equatable {
    /// Readable once the device has been unlocked at least once since boot,
    /// and **carried in an encrypted device backup**: an item in this class
    /// restores onto different hardware.
    case afterFirstUnlock
    /// The same availability, minus the backup. iOS keeps the item wrapped
    /// under the device's UID key, so an encrypted backup cannot re-key it for
    /// another device: it survives a restore onto the *same* device and never
    /// appears on a different one.
    case afterFirstUnlockThisDeviceOnly

    /// The `kSecAttrAccessible` value.
    var attribute: CFString {
        switch self {
        case .afterFirstUnlock: kSecAttrAccessibleAfterFirstUnlock
        case .afterFirstUnlockThisDeviceOnly: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        }
    }
}

/// A secret in the macOS Keychain, addressed by service and account.
///
/// This is the reason the Rust side stopped at a mode-0600 file: Swift reaches
/// the Keychain natively, and a Rust binding to it would have been a second,
/// worse implementation of something the platform already does — including
/// the part that matters, which is that the bytes are encrypted at rest under
/// the user's login and are not readable by another user on the machine.
struct KeychainItem: Sendable {
    /// Groups the app's items. Shows as the item's "Where" in Keychain Access.
    let service: String
    /// Distinguishes items within the service.
    let account: String
    /// The protection class the bytes are stored under. Required rather than
    /// defaulted: whether a secret may travel in a backup is a decision each
    /// caller has to take, and a default is how one gets taken by accident.
    let accessibility: KeychainAccessibility
    /// Which keychain the item is addressed to. Required for the same reason
    /// `accessibility` is, and with sharper teeth: the two domains are two
    /// different stores on macOS, so a defaulted one is how an item silently
    /// stops being where the last build put it. ``KeychainMigration`` is what
    /// moves an existing item between them.
    let domain: KeychainDomain

    /// The `kSecClass`, service, account and domain every query below starts
    /// from. Collected in one place so the domain cannot be applied to three
    /// of the four operations.
    private var baseQuery: [String: Any] {
        var query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account
        ]
        domain.apply(to: &query)
        return query
    }

    /// The stored bytes, or `nil` when there is no such item.
    ///
    /// Throws for every other failure. The distinction is load-bearing: "no
    /// item" means first run, and "the Keychain refused" means a vault whose
    /// key exists and cannot be reached right now. Treating the second as the
    /// first would generate a new root and orphan the existing vault.
    func read() throws -> Data? {
        var query = baseQuery
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)
        switch status {
        case errSecSuccess:
            guard let data = item as? Data else { throw KeychainError.malformedItem }
            return data
        case errSecItemNotFound:
            return nil
        default:
            throw KeychainError.unexpected(status)
        }
    }

    /// The same secret, addressed to the keychain this one is *not* in.
    ///
    /// Only meaningful where ``KeychainDomain/domainsAreDistinctStores`` is
    /// `true`; both callers check that first.
    private var inOtherDomain: KeychainItem {
        KeychainItem(
            service: service,
            account: account,
            accessibility: accessibility,
            domain: domain.other
        )
    }

    /// Whether a mutation the *other* domain refused means that store was
    /// **unreachable from this build**, and so says nothing at all about
    /// whether a copy of ours survives in it.
    ///
    /// Exactly two statuses qualify, and both are established in this tree
    /// rather than recalled. `errSecMissingEntitlement` is what a
    /// `.dataProtection`-addressed mutation answers on every build this
    /// repository can sign — measured, and pinned for `SecItemAdd`,
    /// `SecItemUpdate` and `SecItemDelete` alike by
    /// `theUnreachableDomainRefusesMutationsAndAnswersReadsAsEmpty`.
    /// `errSecItemNotFound` is the store answering that there was nothing of
    /// ours there; ``delete()`` already absorbs it before it can become an
    /// error, so it is named here for the contract rather than because a throw
    /// can carry it today.
    ///
    /// Everything else is raised. A locked keychain, a denied prompt or an I/O
    /// failure is the other store refusing *on its own terms* — it was reached
    /// well enough to say no, and a copy of ours may be sitting in it.
    ///
    /// **The `false` answer is pinned directly** by
    /// `KeychainUnreachableStatusTests`, which asks this predicate about a
    /// locked keychain, a denied prompt, a cancelled prompt and an I/O failure
    /// and requires `false` for each. That is possible because the method is a
    /// pure `OSStatus -> Bool` making no `Security.framework` call; it was
    /// unpinned only because it was `private`, which `@testable import` does not
    /// reach past. Widening it to `internal` observes a decision the production
    /// path takes and cannot make `Security.framework` answer anything it would
    /// not — the ground on which `probeRuns` and `probeInsertQuery` were widened.
    ///
    /// **What executes in no test is the predicate being *asked* that question
    /// mid-case**, declared here on the method that owns it; it is item 3 of the
    /// seven listed in `docs/07-clients/desktop.md`. Every other-domain delete
    /// this repository builds either *succeeds*, as
    /// `aRefusalOnThisDomainDoesNotSpareTheCopyInTheOther`'s does, so never asks
    /// this, or is refused with `errSecMissingEntitlement` — so inside a
    /// *running* cross-domain delete the predicate is only ever asked about a
    /// status it answers `true` to, which is what
    /// `aCrossDomainWriteKeepsWhatItJustWrote` pins, and
    /// ``deleteInOtherDomain()``'s re-raise is never reached. Getting there needs
    /// the other keychain locked or a prompt denied *while a case runs*: a hard
    /// status the ad-hoc build already produces, that no entitlement supplies,
    /// and that this suite cannot arrange — ``deleteAcrossDomains()`` states why.
    static func meansTheOtherStoreWasUnreachable(_ status: OSStatus) -> Bool {
        status == errSecMissingEntitlement || status == errSecItemNotFound
    }

    /// ``delete()`` in the other domain, with only
    /// ``meansTheOtherStoreWasUnreachable(_:)``'s two statuses swallowed.
    ///
    /// The blanket `try?` this replaces rested on a premise this same type
    /// refutes in ``readAcrossDomains()``: that a refusal in the domain this
    /// build did not resolve to means there was nothing of ours to remove. The
    /// cross-domain path exists precisely because the other store *can be
    /// locked or refuse a prompt while this one answers*, and both cannot be
    /// true. On an entitled Mac with a locked login keychain the swallowed
    /// delete leaves the one copy ``readAcrossDomains()`` is then guaranteed to
    /// find: a sign-out that reports success while a live refresh token waits
    /// where the next launch looks, and a token renewal that leaves the two
    /// copies behind ``KeychainError/migrationUnverified``.
    private func deleteInOtherDomain() throws {
        do {
            try inOtherDomain.delete()
        } catch let error as KeychainError {
            guard case let .unexpected(status) = error,
                  Self.meansTheOtherStoreWasUnreachable(status)
            else { throw error }
        }
    }

    /// ``read()``, and — only when this domain holds nothing — a look in the
    /// other one before answering `nil`.
    ///
    /// This exists because ``KeychainDomain/probe()`` fails open, and failing
    /// open is right *before* a migration and wrong *after* one. Before, a
    /// wrong `.login` answer reads the keychain the item is still in. After,
    /// the item has moved, and a single transient `SecItemAdd` failure at
    /// launch makes the probe answer `.login`, the read find an empty login
    /// keychain, and the app report a missing key — the "looks exactly like a
    /// lost vault" screen, to a user whose vault is intact.
    ///
    /// **The second read can only turn a `nil` into bytes.** It never turns a
    /// success into a failure and never turns a `nil` into a throw: a refusal
    /// from the other domain is swallowed. Here — and *only* here — the swallow
    /// stays blanket, which is the one place this type still differs from its
    /// two cross-domain mutations. The reason is not the one an earlier
    /// revision gave: it is **not** that a refusal in the unresolved domain
    /// means there was nothing of ours to find, a premise the paragraph below
    /// refutes and ``deleteInOtherDomain()`` has retired. It is that a refused
    /// *read* has changed nothing, so swallowing it answers the `nil` this
    /// method would have answered before the fallback existed, while raising it
    /// would turn a genuine first run — nothing anywhere, refused on the way
    /// past — into "a key may exist and cannot be reached".
    ///
    /// So the trade deliberately runs the other way from the mutations'. A
    /// swallowed *delete* leaves a live secret in a store a later launch can
    /// still read, which is a failure that outlives the call; a swallowed read
    /// leaves this method answering the `nil` it would have answered before the
    /// fallback existed, which is at worst a retry next launch.
    ///
    /// The *first* read keeps ``read()``'s contract in full: a refusal there is
    /// still "a key may exist and cannot be reached" and still throws.
    ///
    /// The entitlement is **not** what the `try?` is defending against, and an
    /// earlier revision of this comment said it was. Measured on the ad-hoc Mac
    /// this repository builds, and pinned by
    /// `theUnreachableDomainRefusesMutationsAndAnswersReadsAsEmpty`: a
    /// `.dataProtection` **query** answers `errSecItemNotFound` (-25300), not
    /// `errSecMissingEntitlement`. The -34018 refusal lands on the *mutating*
    /// calls — `SecItemAdd`, `SecItemUpdate` and `SecItemDelete`. So the swallow
    /// here is only ever *exercised* on the entitled Mac, where the other store
    /// can be locked or can refuse a prompt while this one answers; on the
    /// unsigned one no read is refused and the second read simply finds
    /// nothing.
    ///
    /// **This swallow executes in no test**, and this declares it — item 7. On
    /// every build this repository makes the other domain's *read* is answered
    /// rather than refused: `.dataProtection` returns `errSecItemNotFound`
    /// (pinned by `theUnreachableDomainRefusesMutationsAndAnswersReadsAsEmpty`),
    /// `.login` answers cleanly, and on iOS the guard below short-circuits
    /// before the `try?` is reached. So it absorbs nothing. **Measured, not
    /// inferred:** on 2026-09-17 the `try?` was replaced with `try` and
    /// `mise run macos-app` run on the result — 586 passed, 0 failed, 0 skipped.
    /// The mutation survives. That is the macOS suite; on iOS the guard above
    /// short-circuits before the line, so there is nothing there to measure.
    /// It joins items 3, 4 and 6 on the
    /// refusal side: it needs the other store to refuse a read on its own terms
    /// while a case is running. Reach supplies no part of it.
    func readAcrossDomains() throws -> Data? {
        if let data = try read() { return data }
        guard KeychainDomain.domainsAreDistinctStores else { return nil }
        return try? inOtherDomain.read()
    }

    /// Store `data`, replacing any existing value.
    func write(_ data: Data) throws {
        let query = baseQuery
        // The protection class rides along on the update, not only on the
        // insert: `SecItemUpdate` changes exactly the attributes it is handed,
        // so an item added by an older build would otherwise keep that build's
        // class for the life of the installation.
        let update: [String: Any] = [
            kSecValueData as String: data,
            kSecAttrAccessible as String: accessibility.attribute
        ]

        let updated = SecItemUpdate(query as CFDictionary, update as CFDictionary)
        if updated == errSecSuccess { return }
        guard updated == errSecItemNotFound else { throw KeychainError.unexpected(updated) }

        var insert = query
        insert[kSecValueData as String] = data
        insert[kSecAttrAccessible as String] = accessibility.attribute
        let added = SecItemAdd(insert as CFDictionary, nil)
        guard added == errSecSuccess else { throw KeychainError.unexpected(added) }
    }

    /// ``write(_:)``, and then the same item in the other domain removed.
    ///
    /// The third side of the symmetry ``readAcrossDomains()`` opened and
    /// ``deleteAcrossDomains()`` closed halfway. Once a `load` can *find* a
    /// secret in the other domain, a `save` that only writes this one leaves
    /// two copies under one `(service, account)` and no rule about which is
    /// current — and the next launch whose ``KeychainDomain/probe()`` answers
    /// correctly walks into ``KeychainMigration``'s one refusal:
    /// ``KeychainError/migrationUnverified``, two different secrets claiming one
    /// name, with no way out that the user can reach.
    ///
    /// The steady state that motivates it needs no user action. An entitled Mac
    /// holds its token in `.dataProtection`; one launch's probe fails open to
    /// `.login` — the premise the whole fallback is built on — the cross-domain
    /// read finds the token and the session restores; then a background renewal
    /// writes the *fresh* token to `.login` while the stale one sits in
    /// `.dataProtection`. Every later launch with a correct probe compares the
    /// two, disagrees, and signs the user out in silence.
    ///
    /// Write first, then delete, for ``KeychainMigration``'s invariant: a
    /// readable copy exists at every instant. This domain's write keeps
    /// ``write(_:)``'s contract and still throws, and nothing is deleted unless
    /// it succeeded.
    ///
    /// The other domain's failure is **not** blanket-swallowed. Only
    /// ``meansTheOtherStoreWasUnreachable(_:)``'s two statuses are — the
    /// missing-entitlement refusal an unentitled build gets, and not-found —
    /// and anything else is raised, because a store that refused on its own
    /// terms may still hold the stale copy this method exists to remove.
    ///
    /// **It is raised as ``KeychainError/writtenButOtherDomainRefused(_:)``,
    /// and the distinct case is the repair.** Raising the other domain's status
    /// was right; raising it as a bare ``KeychainError/unexpected(_:)`` was not,
    /// because by then ``write(_:)`` has already succeeded, and a throw out of a
    /// write reads to every caller as "nothing was stored". On an entitled Mac
    /// with the other keychain locked or a prompt dismissed, that misreading
    /// cost the session twice: a silent renewal whose fresh token landed kept
    /// the *stale* credential in memory and signed the user out at expiry, the
    /// sign-out's `try? clear()` deleting the good token just written; and a
    /// sign-in cleared the credential and failed the state *while the token was
    /// stored*, so the next launch found no source, answered with the
    /// destination's, and signed in a user told sign-in had failed. Both land on
    /// this method's own motivating path: the two-copy state it exists to
    /// collapse is exactly when the delete has something to refuse.
    ///
    /// **What the case does not buy, said rather than left to be found.** The
    /// stale copy survives, so the next load's
    /// ``KeychainMigration/migrate(before:)`` finds destination and source
    /// unequal and raises ``KeychainError/migrationUnverified``, which
    /// ``KeychainMigration/loadMigratingIfNeeded()`` keeps outside its `do` and
    /// lets through — the silent signed-out state, one launch later. The refused
    /// delete creates that whether this throws or not, and the two outcomes
    /// above are strictly worse, losing the session *now* and the good copy with
    /// it. Closing it means teaching the migration that a just-written
    /// destination is authoritative: its verify step, not this method. Writing
    /// the fresh bytes into the other domain so the copies agree was rejected —
    /// every status reaching this line, a locked keychain, a denied prompt, an
    /// I/O failure, refuses a *write* there too.
    ///
    /// **This half is untested by construction, and saying so is the point.**
    /// Two of the seven are here: the raise below, and the cross-domain delete's
    /// own effect — the other domain's copy being removed — observable in no
    /// configuration this repository builds. They do **not** share a blocker, and
    /// an earlier revision said they did. The delete effect waits on *reach*
    /// alone: a reaching build plants a copy, addresses the item away from it,
    /// and watches the delete take it. The raise waits on reach **and** on the
    /// other store refusing on its own terms — it is constructed only in the
    /// `catch` below, which `deleteInOtherDomain()` enters only when
    /// ``meansTheOtherStoreWasUnreachable(_:)`` answers `false`, so the raise
    /// executing *implies* that arm executing and inherits its blocker whole.
    /// The raise needs ``write(_:)`` to **succeed** first, so on an ad-hoc Mac the item's own domain
    /// has to be `.login` and the other is then necessarily `.dataProtection`, whose
    /// mutations answer `errSecMissingEntitlement` unconditionally — swallowed by
    /// ``meansTheOtherStoreWasUnreachable(_:)`` before the re-label. Inverting which
    /// domain the item is addressed in, as ``deleteAcrossDomains()``'s case does, fails
    /// here: `try write(data)` sits *outside* the `do`, so it throws first and the
    /// delete is never reached. The delete's effect needs a copy planted in the domain
    /// this build cannot write to; on iOS the guard below short-circuits. What
    /// `aCrossDomainWriteKeepsWhatItJustWrote` does pin is the two reachable properties:
    /// the write survives on macOS when the other domain refuses, and the delete is
    /// guarded on iOS where the domains are one store. Neither wants a fault-injection
    /// seam inside the type that holds the vault root, rejected four times now: it would
    /// be a second implementation of `Security.framework` to get wrong.
    ///
    /// **Deliberately not folded into ``write(_:)``.**
    /// ``KeychainMigration/migrate(before:)`` writes its destination and only
    /// then deletes its source, and for the credential store the source *is*
    /// this item in the other domain. A `write` that deleted the other domain
    /// would remove the source before the verify step had confirmed the
    /// destination, turning the five-step invariant into the data loss it was
    /// written to prevent. The migration keeps the plain ``write(_:)``; the
    /// stores, whose save is the last word on what the secret is, call this.
    func writeAcrossDomains(_ data: Data) throws {
        try write(data)
        guard KeychainDomain.domainsAreDistinctStores else { return }
        do {
            try deleteInOtherDomain()
        } catch let error as KeychainError {
            // Only a status is re-labelled: an error this method has not
            // accounted for must not be asserted to mean the write survived.
            guard case let .unexpected(status) = error else { throw error }
            throw KeychainError.writtenButOtherDomainRefused(status)
        }
    }

    /// Raise an existing item to `accessibility`, if it is not there already.
    ///
    /// The vault root is written once and read on every launch afterwards, so
    /// nothing on the ordinary path ever rewrites it: without this, an item
    /// added under a weaker class by an earlier build would keep that class
    /// forever, and the upgrade that tightened the class would be a no-op for
    /// exactly the installations that already exist. Call it before the read.
    ///
    /// A keychain that does not implement protection classes returns no
    /// `kSecAttrAccessible` and is left alone. That is the macOS file-based
    /// login keychain, which an app without the data-protection entitlement
    /// still uses: it accepts the attribute on add, stores nothing, and
    /// reports nothing back. There is no class there to raise or lower.
    func upgradeAccessibilityIfNeeded() throws {
        let match = baseQuery
        var query = match
        query[kSecReturnAttributes as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne

        var found: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &found)
        switch status {
        case errSecSuccess: break
        case errSecItemNotFound: return
        default: throw KeychainError.unexpected(status)
        }

        guard let attributes = found as? [String: Any],
              let current = attributes[kSecAttrAccessible as String] as? String,
              current != accessibility.attribute as String
        else { return }

        // Failing the load is the deliberate answer to a refusal here. The
        // alternative is to hand back a key that is still in the backup-bearing
        // class while telling the user nothing, and a security downgrade that
        // survives its own fix is the thing this method exists to prevent. No
        // data is at risk: the item is untouched, nothing generates a new root
        // on a throw, and the next launch tries again.
        let updated = SecItemUpdate(
            match as CFDictionary,
            [kSecAttrAccessible as String: accessibility.attribute] as CFDictionary
        )
        guard updated == errSecSuccess else {
            throw KeychainError.accessibilityNotRaised(updated)
        }
    }

    /// Remove the item. Succeeds when there is nothing to remove.
    func delete() throws {
        let status = SecItemDelete(baseQuery as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw KeychainError.unexpected(status)
        }
    }

    /// ``delete()``, and the same item in the other domain with it.
    ///
    /// The counterpart ``readAcrossDomains()`` requires, and the reason it is
    /// not optional: once a `load` can *find* a secret in the other domain, a
    /// `clear` that only removes this one would leave a signed-out session's
    /// refresh token, or a "forgotten" vault root, readable where the next
    /// launch will look for it. Whatever a read can reach, a clear removes.
    ///
    /// The other domain's failure is **not** blanket-swallowed. Only
    /// ``meansTheOtherStoreWasUnreachable(_:)``'s two statuses are, and
    /// anything else is raised: a store that was reached well enough to refuse
    /// on its own terms may still be holding the copy a `clear` is supposed to
    /// take away, and reporting that as success is the failure this method
    /// exists to prevent. This domain's delete keeps ``delete()``'s contract
    /// and still throws, and when both refuse it is this domain's status that
    /// is raised — the one ``clear()``'s callers were already written against.
    ///
    /// **Both are attempted, and only then is this domain's status raised.**
    /// An earlier revision put `try delete()` on its own line ahead of the
    /// cross-domain half, so a refusal here skipped the other domain entirely
    /// and left behind the one copy ``readAcrossDomains()`` can still find —
    /// with `AccountModel.signOut()` swallowing the throw, a user told they
    /// are signed out while a live refresh token waits where the next launch
    /// looks. That is not a hypothetical ordering: on the ad-hoc Mac a
    /// `.dataProtection`-addressed delete answers `errSecMissingEntitlement`
    /// (-34018), so the *whole* cross-domain clear was a no-op for exactly the
    /// item that needed it.
    ///
    /// **Decision 13's promise, narrowed rather than broken.** That promise was
    /// that `clear()` would gain no new way to throw, and this comment used to
    /// keep it by raising this domain's status and only this domain's. It still
    /// holds on every configuration this repository builds: there, the other
    /// domain's refusal is the missing-entitlement one, which is swallowed. It
    /// gains exactly one new throw, on an entitled Mac whose other keychain is
    /// locked or refuses a prompt — a real failure that was previously reported
    /// to the user as a successful sign-out. `AccountModel.signOut()` swallows
    /// the throw with a `try?` of its own, so raising it here does not by
    /// itself reach the user; that is tracked separately.
    ///
    /// **The tie-break below executes in no test**, and this declares it — one of the
    /// seven. The `??` needs *both* deletes to refuse, and the one case reaching the
    /// other-domain arm, `aRefusalOnThisDomainDoesNotSpareTheCopyInTheOther`, has that
    /// delete *succeed*. **Measured, not inferred:** on 2026-09-17 the `??` was
    /// replaced with a plain `failureToRaise = error` and `mise run macos-app`
    /// run on the result — 586 passed, 0 failed, 0 skipped. The mutation
    /// survives on the macOS suite. It asserts that *a* ``KeychainError`` is raised, not
    /// which. Both refusing at once needs the locked keychain
    /// ``meansTheOtherStoreWasUnreachable(_:)`` records this suite cannot produce
    /// mid-case, which no entitlement supplies either.
    ///
    /// **Why this suite cannot produce it**, on a mechanism rather than the
    /// premise an earlier revision asserted in five places — that the suite
    /// "holds the keychain unlocked by construction". It holds nothing: it makes
    /// no keychain-state call of any kind, and inherits whatever the host login
    /// session already unlocked. What blocks a lock is that its smallest scope is
    /// the *machine's* default keychain. Swift Testing parallelizes by default and
    /// `.serialized` orders only the suite that carries it, so no trait here keeps
    /// a lock away from the cases running beside it — several of which write real
    /// `.login` items — and getting back out without a UI prompt needs a password
    /// no case here has.
    /// `docs/07-clients/desktop.md` states it in full, with the SDK's own
    /// deprecation and platform annotations.
    func deleteAcrossDomains() throws {
        // Named for what it is rather than for which domain produced it: since
        // the other domain's refusal became raisable, either delete can be the
        // one that fills it.
        var failureToRaise: (any Error)?
        do {
            try delete()
        } catch {
            failureToRaise = error
        }
        if KeychainDomain.domainsAreDistinctStores {
            do {
                try deleteInOtherDomain()
            } catch {
                // This domain's status wins when both refuse: it is the one
                // `clear()`'s callers were written against, and the other
                // domain's is the case this only just started raising.
                failureToRaise = failureToRaise ?? error
            }
        }
        if let failureToRaise { throw failureToRaise }
    }
}
