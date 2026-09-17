import Foundation

/// Moves one secret from the keychain an older build wrote it to, into the one
/// this build addresses — without ever leaving zero readable copies.
///
/// ## Why this exists
///
/// Items already in the macOS login keychain do not move by themselves. An
/// installation that predates the data-protection keychain holds its vault root
/// there, and a build that silently starts reading the data-protection keychain
/// finds nothing and looks, to the user, **exactly like a lost vault** — the
/// failure `docs/07-clients/desktop.md` names. The vault root is the key every
/// Stream key and the SQLCipher key hang off, so that is not a recoverable
/// mistake.
///
/// ## The invariant
///
/// **A readable copy exists at every instant.** The write to the destination
/// happens before the delete of the source, never the other way round, which is
/// the same argument that rejected delete-then-re-add for the accessibility
/// raise this runs ahead of.
///
/// Five steps, each resumable, because the process can be killed between any
/// two of them:
///
/// | step | action | a crash here leaves | the next launch does |
/// |---|---|---|---|
/// | 0 | read the destination | — | non-nil: done; source still present: verify and delete |
/// | 1 | read the source | source only | restarts at 0 |
/// | 2 | write the destination | both, unverified | 0 finds the destination, then verifies and deletes |
/// | 3 | read the destination back and compare | both, verified | 0 finds the destination, then deletes the source |
/// | 4 | delete the source | destination only | 0 finds the destination; nothing to delete |
///
/// ## What it refuses to do, and what it shrugs at
///
/// It throws for exactly one thing: a destination that exists and whose bytes
/// **differ** from the source's, which means two different secrets claim one
/// `(service, account)` and this code cannot know which one the vault was
/// sealed with. Every other `Security.framework` failure falls back to the
/// source item, because locking a user out over a *destination* problem while
/// the secret sits perfectly readable in the source is a strictly worse trade
/// than the one the accessibility raise takes.
///
/// Nothing on either side is also not an error: neither side holding anything
/// is first run, and ``KeychainItem/read()`` records why turning that into an
/// error would generate a new root and orphan the existing vault.
struct KeychainMigration: Sendable {
    /// Where an older build put the secret.
    let source: KeychainItem
    /// Where this build reads it from.
    let destination: KeychainItem

    /// The points ``run(before:)`` can be interrupted at.
    ///
    /// Public only so a test can inject a failure at each one and then resume
    /// the real call against the real partial keychain state. Simulating the
    /// interruption instead would test the simulation.
    enum Step: Sendable, Equatable {
        case readDestination
        case readSource
        case writeDestination
        case verify
        case deleteSource
    }

    /// What the ``run(before:)`` hook's error comes back as.
    ///
    /// The hook stands in for the process being killed, so its error must
    /// escape the fallback below — otherwise the test would observe the
    /// fallback's answer while believing it observed a partial keychain state.
    /// Wrapping it **at the throw site** is what makes that exemption
    /// structural rather than accidental: the catch is on ``KeychainError``,
    /// and before this the exemption held only because the tests happened to
    /// raise a type of their own. The next case to throw
    /// ``KeychainError/unexpected(_:)`` from the hook — the natural way to pin
    /// what a `Security` failure at a given step does — would have been
    /// swallowed, silently.
    struct Interruption: Error {
        /// The step the hook refused to let happen.
        let step: Step
        /// What the hook raised.
        let cause: any Error
    }

    /// Whether the two items are two names for one piece of storage.
    ///
    /// The case that makes this load-bearing rather than defensive: on every
    /// Apple platform but macOS there is one keychain, so `.login` and
    /// `.dataProtection` address the same item. A migration that did not
    /// notice would write the destination over itself, verify it against
    /// itself, and then delete it — turning the one thing this type exists to
    /// prevent into the thing it does.
    ///
    /// The accessibility class is deliberately not part of the comparison: two
    /// items differing only in the class they declare are the same stored item,
    /// and raising it is ``KeychainItem/upgradeAccessibilityIfNeeded()``'s job,
    /// which each store calls immediately after this one.
    var sourceAndDestinationAreOneItem: Bool {
        guard source.service == destination.service,
              source.account == destination.account
        else { return false }
        guard source.domain != destination.domain else { return true }
        return !KeychainDomain.domainsAreDistinctStores
    }

    /// Carry out whatever part of the migration is still outstanding, and
    /// answer with the secret if either side holds one.
    ///
    /// The returned value is **not** discardable, and that is deliberate: the
    /// fallback arm below answers with the secret still readable in the
    /// *source*, so a caller that discards it and reads the destination
    /// instead sees nothing and reports a lost vault. See
    /// ``loadMigratingIfNeeded()``, which is what every store calls.
    ///
    /// - Parameter before: called with each ``Step`` immediately before it is
    ///   taken. A production caller passes nothing; a test throws from it to
    ///   kill the migration at a chosen point. Whatever it raises comes back
    ///   wrapped in an ``Interruption``, which is not a ``KeychainError`` and
    ///   so cannot be caught by the fallback below — that is what lets the
    ///   test observe the real partial state, structurally rather than by the
    ///   test's error type happening to be distinct.
    /// - Returns: the secret, or `nil` when neither side holds one.
    /// - Throws: ``KeychainError/migrationUnverified`` when the destination
    ///   holds bytes that are not the source's; an ``Interruption`` wrapping
    ///   whatever `before` raises; and whatever reading the *source* raises,
    ///   which is the pre-existing "a key may exist and cannot be reached"
    ///   failure this must not soften.
    func run(before: (Step) throws -> Void = { _ in }) throws -> Data? {
        guard !sourceAndDestinationAreOneItem else {
            try interrupting(.readDestination, before)
            return try destination.read()
        }
        do {
            return try migrate(before: before)
        } catch let error as KeychainError {
            if case .migrationUnverified = error { throw error }
            // The destination is unreachable, malformed or refusing writes.
            // The source is still exactly where the last build left it, so the
            // app keeps working on the old location and tries again next
            // launch. The raise that follows this call is what still refuses to
            // hand back a secret under a weaker class than the build claims.
            //
            // This value is the whole point of the arm, and
            // `loadMigratingIfNeeded()` is what consumes it. Discarding it and
            // reading the destination instead — which is what every caller did
            // before — dropped the secret this line had just rescued.
            return try source.read()
        }
    }

    /// Everything a store's `load` does with a migration, in the one order
    /// that is correct.
    ///
    /// One function rather than the same three lines in
    /// ``KeychainVaultRootStore``, ``KeychainCredentialStore`` and
    /// ``KeychainRelayDeviceIDStore``, because the order is load-bearing in
    /// two directions and three copies is three chances to get it wrong:
    ///
    /// 1. **Migrate, then raise.** Raising the class of an item about to be
    ///    replaced by a copy does nothing, and raising one that does not exist
    ///    yet hides the migration's failure.
    /// 2. **Answer with the destination, then with whatever ``run(before:)``
    ///    returned.** On a destination failure the fallback arm hands back the
    ///    bytes still readable in the *source*, and the destination read below
    ///    finds nothing; dropping that value drops the vault root, signs the
    ///    user out and un-binds the relay device id, while the secret sits
    ///    perfectly readable where the last build left it.
    /// 3. **Read across domains.** See ``KeychainItem/readAcrossDomains()``.
    /// 4. **Keep the rescued bytes alive across both destination steps.** The
    ///    value ``run(before:)`` handed back used to be reachable only through
    ///    the `??`, with nothing between it and the two destination calls that
    ///    can throw. So a destination that was *unreachable* or *malformed* —
    ///    two of the three failure classes ``run(before:)``'s own fallback arm
    ///    names — threw straight past the bytes it had just rescued, and only
    ///    the third, a destination refusing *writes*, ever reached the caller.
    ///    On an entitled Mac whose data-protection store is locked or refusing
    ///    while the login keychain answers, that is the `.locked(…)` screen the
    ///    fallback exists to prevent, shown to a user whose vault is intact.
    ///
    /// Two failures deliberately still propagate:
    ///
    /// - ``KeychainError/accessibilityNotRaised(_:)``. The item is readable and
    ///   the *guarantee* is what failed, and refusing the load over it is the
    ///   trade ``KeychainItem/upgradeAccessibilityIfNeeded()`` was written to
    ///   take. A blanket `try?` there would let a secret out under a weaker
    ///   class than this build claims, silently — a security downgrade that
    ///   survives its own fix. Only the *lookup* half of that method raises
    ///   ``KeychainError/unexpected(_:)``, and a lookup that cannot reach the
    ///   destination is a destination failure like any other.
    /// - Anything at all when `migrated` is `nil`. Substituting `nil` for an
    ///   unreachable destination is precisely the "no item means first run"
    ///   confusion ``KeychainItem/read()`` records: it would generate a new
    ///   root and orphan the existing vault. Better a retry next launch.
    ///
    /// ``KeychainError/migrationUnverified`` needs no case here — `run()` is
    /// outside the `do`, so its one refusal is never in reach of this `catch`.
    ///
    /// Untested by construction, and it is worth saying why rather than leaving
    /// the gap to be re-discovered: no configuration this repository builds can
    /// make either destination step throw. A `.dataProtection` query answers
    /// `errSecItemNotFound`, the login keychain reports no `kSecAttrAccessible`
    /// to disagree with and accepts every update, and `SecItemCopyMatching`
    /// always hands back `CFData`. Reaching it needs an entitled, signed build —
    /// or a fault-injection seam inside ``KeychainItem``, which would be a
    /// second implementation of `Security.framework` to get wrong.
    ///
    /// - Returns: the secret, or `nil` when nothing anywhere holds one.
    func loadMigratingIfNeeded() throws -> Data? {
        let migrated = try run()
        do {
            try destination.upgradeAccessibilityIfNeeded()
            if let atDestination = try destination.readAcrossDomains() { return atDestination }
            return migrated
        } catch let error as KeychainError {
            if case .accessibilityNotRaised = error { throw error }
            guard let migrated else { throw error }
            return migrated
        }
    }

    /// Call the hook, and turn anything it raises into an ``Interruption``.
    private func interrupting(_ step: Step, _ before: (Step) throws -> Void) throws {
        do {
            try before(step)
        } catch {
            throw Interruption(step: step, cause: error)
        }
    }

    private func migrate(before: (Step) throws -> Void) throws -> Data? {
        try interrupting(.readDestination, before)
        let alreadyThere = try destination.read()

        try interrupting(.readSource, before)
        guard let fromSource = try source.read() else {
            // Nothing left to move: either the destination already holds it and
            // step 4 completed, or this is first run and nobody holds anything.
            return alreadyThere
        }

        if alreadyThere == nil {
            try interrupting(.writeDestination, before)
            try destination.write(fromSource)
        }

        try interrupting(.verify, before)
        guard let verified = try destination.read(), verified == fromSource else {
            throw KeychainError.migrationUnverified
        }

        try interrupting(.deleteSource, before)
        try source.delete()
        return verified
    }
}
