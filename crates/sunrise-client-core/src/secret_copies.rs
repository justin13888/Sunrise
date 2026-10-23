//! What to do when one secret may sit in two keychains, as a decision table.
//!
//! The Apple app moves each of its secrets (the vault root, the relay device
//! id, the OIDC credential) out of the keychain an older build wrote and into
//! the one this build reads (`KeychainMigration.swift`). The I/O is the
//! platform's. The *decision*, meaning which copy survives and what is
//! written or deleted, is this module's. It is a pure function over what the
//! two reads found, so every case is pinned here and not left to a
//! `Security.framework` state no test can arrange.
//!
//! # The case this exists for (#258)
//!
//! A cross-domain credential `save` writes the fresh token into the keychain
//! this build reads. Then it tries to delete the stale copy from the other
//! keychain, and a locked keychain or a denied prompt can refuse that. Two
//! copies now sit under one `(service, account)`. Until this table existed,
//! the next launch's migration found two copies that differ and refused them
//! as `migrationUnverified`. The account restore read that as signed-out.
//! Signing in wrote the fresh copy again, the stale one stayed, and every
//! later launch refused the same pair again.
//!
//! The refusal is right for a **vault root** and a **relay device id**.
//! Neither carries anything that says which copy is current, and a wrong
//! guess about a vault root seals the user out of their vault. #242's
//! decision 3 chose to refuse there, and this keeps it.
//!
//! A **credential** is different in one way that settles it: each copy states
//! its own age. A sign-in or a renewal always mints a token that expires
//! later than the one it replaces, so the copy with the later
//! `expires_at_ms` is the fresher one. A wrong guess costs one sign-in, never
//! a vault. So two differing credential copies are ordered by their expiry,
//! the fresher is kept, and the other is deleted. A delete that is refused
//! again changes nothing: the next launch reaches the same answer from the
//! same two copies, and the user stays signed in throughout. That is what
//! ends the loop.
//!
//! The issue's three routes are all avoided. Nothing is persisted beside the
//! keychain (route a). No mismatch is guessed at for a secret that cannot be
//! ordered (route b). And the answer does not depend on in-process memory, so
//! it survives the relaunch where the loop actually happens (route c).

/// Which secret the two copies are of. It decides whether two copies that
/// differ can be told apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretKind {
    /// The vault root. Two different roots are two different vaults, and
    /// nothing in either says which one the vault on disk was sealed with.
    VaultRoot,
    /// The relay device id. Nothing in an id says which binding is current.
    RelayDeviceId,
    /// An OIDC credential. Each copy carries its own `expires_at_ms`, and the
    /// later one is the fresher.
    Credential,
}

/// What the migration's two reads found.
///
/// It carries facts and no secret bytes, so nothing secret has to cross into
/// this layer to be decided on. The platform compares the bytes itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretCopies {
    /// Neither keychain holds the secret: first run.
    Neither,
    /// Only the destination, the keychain this build reads, holds it.
    DestinationOnly,
    /// Only the source, the keychain an older build wrote, holds it.
    SourceOnly,
    /// Both hold it, byte for byte the same. A move was interrupted after its
    /// write.
    Identical,
    /// Both hold it, and the bytes differ.
    Differ {
        /// The destination copy's `expires_at_ms`, for a credential that
        /// decodes; `None` for any other secret, or for a copy that does not
        /// decode.
        destination_expires_at_ms: Option<u64>,
        /// The source copy's `expires_at_ms`, on the same terms.
        source_expires_at_ms: Option<u64>,
    },
}

/// What the migration does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretCopyPlan {
    /// Nothing to move and nothing to answer: first run. Never an error, since
    /// treating it as one would mint a new root and orphan the vault.
    Nothing,
    /// The move is complete. Answer with the destination.
    UseDestination,
    /// Write the source's bytes to the destination, read them back, and
    /// delete the source. A move that has not started, and a refused
    /// credential copy whose source is the fresher, both land here.
    MoveSource,
    /// Keep the destination and delete the source. This covers an interrupted
    /// move, where both copies are identical, and a credential whose
    /// destination is the fresher. Answer with the destination whether or not
    /// the delete succeeds.
    DeleteSource,
    /// Two different secrets claim one name, and nothing says which is
    /// current: refuse (`migrationUnverified`), and touch neither.
    Refuse,
}

/// Decide what a migration does with what its two reads found.
///
/// `copies` is read before anything is written. The verify step after a
/// write is [`verify_write`].
#[must_use]
pub fn plan_secret_copies(kind: SecretKind, copies: SecretCopies) -> SecretCopyPlan {
    match copies {
        SecretCopies::Neither => SecretCopyPlan::Nothing,
        SecretCopies::DestinationOnly => SecretCopyPlan::UseDestination,
        SecretCopies::SourceOnly => SecretCopyPlan::MoveSource,
        SecretCopies::Identical => SecretCopyPlan::DeleteSource,
        SecretCopies::Differ {
            destination_expires_at_ms,
            source_expires_at_ms,
        } => match kind {
            SecretKind::VaultRoot | SecretKind::RelayDeviceId => SecretCopyPlan::Refuse,
            SecretKind::Credential => {
                fresher_credential(destination_expires_at_ms, source_expires_at_ms)
            }
        },
    }
}

/// Which of two differing credential copies survives.
///
/// The later expiry wins. A copy that does not decode loses to one that does,
/// because the store's `load` reads an undecodable token as no token, so
/// keeping it would sign the user out. When the two cannot be ordered (the
/// same expiry, or neither decodes), the destination is kept: it is the
/// keychain `save` writes first, and a wrong guess costs one sign-in.
fn fresher_credential(destination: Option<u64>, source: Option<u64>) -> SecretCopyPlan {
    match (destination, source) {
        (Some(d), Some(s)) if s > d => SecretCopyPlan::MoveSource,
        (None, Some(_)) => SecretCopyPlan::MoveSource,
        _ => SecretCopyPlan::DeleteSource,
    }
}

/// What reading the destination back after a write found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WriteReadBack {
    /// The bytes written are there.
    Matches,
    /// Nothing is there: another process cleared it, or a person deleted it
    /// in Keychain Access, between the write and the read.
    Gone,
    /// Different bytes are there.
    Differs,
}

/// What a migration does after [`SecretCopyPlan::MoveSource`] wrote the
/// destination and read it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VerifyPlan {
    /// The write took. Delete the source and answer with the destination.
    DeleteSource,
    /// The destination vanished, which leaves no competing secret. Answer
    /// with the source, which is still where the last build left it, and
    /// leave it there for the next launch to move.
    FallBackToSource,
    /// Something other than what was written is there. Refuse, and touch
    /// neither.
    Refuse,
}

/// Decide what follows a write, from what reading it back found.
///
/// A destination that has gone is not the refusal case. Refusing over it
/// would lock the user out over a destination problem while the source is
/// still readable, which is the one trade the migration exists to avoid.
#[must_use]
pub fn verify_write(read_back: WriteReadBack) -> VerifyPlan {
    match read_back {
        WriteReadBack::Matches => VerifyPlan::DeleteSource,
        WriteReadBack::Gone => VerifyPlan::FallBackToSource,
        WriteReadBack::Differs => VerifyPlan::Refuse,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [SecretKind; 3] = [
        SecretKind::VaultRoot,
        SecretKind::RelayDeviceId,
        SecretKind::Credential,
    ];

    fn differ(destination: Option<u64>, source: Option<u64>) -> SecretCopies {
        SecretCopies::Differ {
            destination_expires_at_ms: destination,
            source_expires_at_ms: source,
        }
    }

    /// The four agreeing cases are the migration's resumable steps, and they
    /// do not depend on what the secret is.
    #[test]
    fn every_secret_moves_the_same_way_until_two_copies_disagree() {
        for kind in KINDS {
            assert_eq!(
                plan_secret_copies(kind, SecretCopies::Neither),
                SecretCopyPlan::Nothing
            );
            assert_eq!(
                plan_secret_copies(kind, SecretCopies::DestinationOnly),
                SecretCopyPlan::UseDestination
            );
            assert_eq!(
                plan_secret_copies(kind, SecretCopies::SourceOnly),
                SecretCopyPlan::MoveSource
            );
            assert_eq!(
                plan_secret_copies(kind, SecretCopies::Identical),
                SecretCopyPlan::DeleteSource
            );
        }
    }

    /// #242's decision 3, kept: a vault root or a device id that disagrees
    /// with itself is refused, however the expiry fields are filled.
    #[test]
    fn two_vault_roots_or_two_device_ids_that_differ_are_refused() {
        for kind in [SecretKind::VaultRoot, SecretKind::RelayDeviceId] {
            for copies in [
                differ(None, None),
                differ(Some(1), Some(2)),
                differ(Some(2), Some(1)),
                differ(None, Some(1)),
            ] {
                assert_eq!(
                    plan_secret_copies(kind, copies),
                    SecretCopyPlan::Refuse,
                    "{kind:?} {copies:?}"
                );
            }
        }
    }

    /// #258: `save` wrote the fresh token to the destination and could not
    /// delete the stale one from the source. The fresh one is kept, and the
    /// stale one is deleted. Asked again with the same pair, which is what a
    /// relaunch after a second refused delete does, the answer is the same.
    /// That is why there is no loop.
    #[test]
    fn a_refused_cleanup_after_a_save_keeps_the_fresh_destination_on_every_launch() {
        let after_save = differ(Some(2_000), Some(1_000));
        for _launch in 0..3 {
            assert_eq!(
                plan_secret_copies(SecretKind::Credential, after_save),
                SecretCopyPlan::DeleteSource
            );
        }
    }

    /// The mirror case that `CredentialStore.save` documents: a launch whose
    /// probe failed open wrote the fresh token to the source keychain. The
    /// source wins and is moved over the stale destination.
    #[test]
    fn a_fresher_source_credential_is_moved_over_a_stale_destination() {
        assert_eq!(
            plan_secret_copies(SecretKind::Credential, differ(Some(1_000), Some(2_000))),
            SecretCopyPlan::MoveSource
        );
    }

    /// A copy that does not decode is no session at all, so it never beats
    /// one that does. Two that cannot be ordered keep the destination rather
    /// than refusing, because refusing is the loop.
    #[test]
    fn credential_copies_that_cannot_be_ordered_keep_one_and_never_refuse() {
        let plan = |d, s| plan_secret_copies(SecretKind::Credential, differ(d, s));
        assert_eq!(plan(None, Some(5)), SecretCopyPlan::MoveSource);
        assert_eq!(plan(Some(5), None), SecretCopyPlan::DeleteSource);
        assert_eq!(plan(None, None), SecretCopyPlan::DeleteSource);
        assert_eq!(plan(Some(5), Some(5)), SecretCopyPlan::DeleteSource);
    }

    #[test]
    fn a_vanished_destination_falls_back_and_a_different_one_is_refused() {
        assert_eq!(
            verify_write(WriteReadBack::Matches),
            VerifyPlan::DeleteSource
        );
        assert_eq!(
            verify_write(WriteReadBack::Gone),
            VerifyPlan::FallBackToSource
        );
        assert_eq!(verify_write(WriteReadBack::Differs), VerifyPlan::Refuse);
    }
}
