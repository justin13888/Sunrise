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
    /// - Parameter before: called with each ``Step`` immediately before it is
    ///   taken. A production caller passes nothing; a test throws from it to
    ///   kill the migration at a chosen point. An error raised here is **not**
    ///   a Keychain failure and is not caught by the fallback below — that is
    ///   what lets the test observe the real partial state.
    /// - Returns: the secret, or `nil` when neither side holds one.
    /// - Throws: ``KeychainError/migrationUnverified`` when the destination
    ///   holds bytes that are not the source's; whatever `before` raises;
    ///   and whatever reading the *source* raises, which is the pre-existing
    ///   "a key may exist and cannot be reached" failure this must not soften.
    @discardableResult
    func run(before: (Step) throws -> Void = { _ in }) throws -> Data? {
        guard !sourceAndDestinationAreOneItem else {
            try before(.readDestination)
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
            return try source.read()
        }
    }

    private func migrate(before: (Step) throws -> Void) throws -> Data? {
        try before(.readDestination)
        let alreadyThere = try destination.read()

        try before(.readSource)
        guard let fromSource = try source.read() else {
            // Nothing left to move: either the destination already holds it and
            // step 4 completed, or this is first run and nobody holds anything.
            return alreadyThere
        }

        if alreadyThere == nil {
            try before(.writeDestination)
            try destination.write(fromSource)
        }

        try before(.verify)
        guard let verified = try destination.read(), verified == fromSource else {
            throw KeychainError.migrationUnverified
        }

        try before(.deleteSource)
        try source.delete()
        return verified
    }
}
