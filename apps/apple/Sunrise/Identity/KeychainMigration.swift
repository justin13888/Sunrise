import Foundation
import Security

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
/// A destination that has *gone away* by step 3 is not that case and is not
/// refused: it raises ``KeychainError/unexpected(_:)`` carrying
/// `errSecItemNotFound`, which the fallback below absorbs like any other
/// destination failure. `KeychainMigrationVerifyTests` pins it.
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
    ///   holds bytes that are not the source's; ``KeychainError/unexpected(_:)``
    ///   carrying `errSecItemNotFound` when the destination no longer exists at
    ///   all, which the fallback below absorbs; an ``Interruption`` wrapping
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
    /// the gap to be re-discovered — but not for the reason an earlier revision
    /// of this comment gave. It said no configuration this repository builds can
    /// make either destination step throw, and that is too strong. It holds for
    /// the *ordinary* statuses, which is all that was measured: a
    /// `.dataProtection` query answers `errSecItemNotFound`, the login keychain
    /// reports no `kSecAttrAccessible` to disagree with and accepts every
    /// update, and a successful `SecItemCopyMatching` always hands back
    /// `CFData`. It does not hold for a *locked* login keychain or a denied
    /// prompt, which make that same query return a hard status on an ad-hoc
    /// build — so the arm is reachable here after all, through a lock or a
    /// denial landing between ``run(before:)``'s reads and either of the two
    /// destination calls below.
    ///
    /// The conclusion survives the premise: what no test in this repository can
    /// do is *arrange* that race, because locking the login keychain — or
    /// forcing a denied prompt — part-way through a running case is not
    /// something this suite can drive. Not, as an earlier revision of this
    /// comment claimed, because the suite "holds the keychain unlocked for its
    /// whole run by construction": it holds nothing and makes no keychain-state
    /// call at all, it inherits the host login session's unlocked keychain, and
    /// that is a property of the machine rather than of the suite. The mechanism
    /// is stated once, on ``KeychainItem/deleteAcrossDomains()`` — the lock's
    /// smallest scope is the machine's default keychain, no trait available here
    /// keeps it away from the cases running beside it, and getting back out of
    /// it needs a password no case has. So the arm still executes in no test,
    /// and it is one of the seven lines this change declares untestable, listed
    /// together in `docs/07-clients/desktop.md`, where it is item 6.
    ///
    /// **Item 6 is one item with three branches, and each is declared here**
    /// rather than left inside a single line-level claim — the set's own rule
    /// is per line, and applying it to the `catch` as a whole was how three
    /// behaviours came to be recorded as one:
    ///
    /// - `if case .accessibilityNotRaised = error { throw error }` needs
    ///   ``KeychainItem/upgradeAccessibilityIfNeeded()`` to reach its
    ///   `SecItemUpdate` and be refused. It cannot here: the lookup returns
    ///   early on `errSecItemNotFound`, which is what a `.dataProtection`
    ///   query answers on every build this repository makes, and on `.login`
    ///   the file-based keychain reports no `kSecAttrAccessible` at all, so
    ///   the second guard returns early too.
    /// - `guard let migrated else { throw error }` needs the `do` to throw
    ///   *and* `migrated` to be `nil`.
    /// - `return migrated` needs the `do` to throw *and* `migrated` to be
    ///   non-`nil`.
    ///
    /// The last two share the `do`'s throw, and it holds exactly two calls:
    /// the accessibility raise, which cannot raise here for the reason above,
    /// and ``KeychainItem/readAcrossDomains()``, whose only raising read is its
    /// first — a `.login` read never refuses on this build, and a
    /// `.dataProtection` read answers `errSecItemNotFound`, pinned by
    /// `theUnreachableDomainRefusesMutationsAndAnswersReadsAsEmpty`.
    /// `.malformedItem` is out of reach because `kSecReturnData` on a generic
    /// password always yields `CFData`. Both fallback cases that call this
    /// address their destination at `.dataProtection`, so neither enters the
    /// `catch` at all. All three branches execute in no test, and none of them
    /// is pinnable here. What closes them is not an entitled, signed build,
    /// which an earlier revision of this comment named: the hard status is
    /// already reachable on the ad-hoc build this repository produces, which is
    /// the paragraph above's whole point. Nor are they closed by a
    /// fault-injection seam inside ``KeychainItem``, which would be a second
    /// implementation of `Security.framework` to get wrong.
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
        // Two conditions, two answers, and collapsing them cost a session. A
        // destination that is *gone* by step 3 — a concurrent `clear()` from a
        // second process, or the row deleted in Keychain Access between step 0
        // and here — holds no competing secret at all. Raising
        // `migrationUnverified` for it locks the user out over a destination
        // problem while the source is still perfectly readable, which is the one
        // trade this type exists to refuse. `.unexpected` is absorbed by
        // `run()`'s fallback arm, which answers with the source's bytes.
        guard let verified = try destination.read() else {
            throw KeychainError.unexpected(errSecItemNotFound)
        }
        guard verified == fromSource else { throw KeychainError.migrationUnverified }

        try interrupting(.deleteSource, before)
        try source.delete()
        return verified
    }
}
