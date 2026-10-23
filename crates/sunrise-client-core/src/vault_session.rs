//! What the app is at launch and after: the vault session's lifecycle.
//!
//! Three of these states are routinely collapsed into one "not signed in"
//! screen, and collapsing them is how a client loses a vault: told there is no
//! key when the platform's key store merely would not answer, the obvious
//! repair — make a new one — permanently orphans the encrypted data already
//! on disk. So a vault that exists, or a key that may, and cannot be opened
//! right now is [`VaultPhase::Locked`], and **never** a cue to generate a key;
//! creating one is refused from every phase but [`VaultPhase::FirstRun`].
//!
//! Ported from the Apple app's `SessionModel`. The transitions are pure: the
//! key store, the vault directory and the core are the caller's, and each
//! method takes what they answered.

use crate::host::{vault_root, BadVaultRoot};

/// Why a vault cannot be opened right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockReason {
    /// The key store refused, was locked, or returned something unusable.
    KeyStoreUnavailable(String),
    /// The vault is on disk and its key is not in this key store — a machine
    /// restored from a backup. The way out is pairing or the recovery code,
    /// never a key made here.
    KeyMissingForExistingVault,
    /// The user closed the vault. The key and the data are where they were;
    /// only this process let go.
    LockedByUser,
}

impl LockReason {
    /// What the button that resolves this reason does: unlock a vault the
    /// user closed, or try again for the rest.
    #[must_use]
    pub const fn repair(&self) -> LockRepair {
        match self {
            Self::LockedByUser => LockRepair::Unlock,
            Self::KeyStoreUnavailable(_) | Self::KeyMissingForExistingVault => LockRepair::TryAgain,
        }
    }
}

/// The repair a locked screen offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LockRepair {
    /// "Unlock": the key is in the store and nothing was lost.
    Unlock,
    /// "Try again": read the store again.
    TryAgain,
}

/// The session's phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultPhase {
    /// Reading the key store and the vault directory.
    Starting,
    /// No vault and no key. Nothing is at risk; offer to create one.
    FirstRun,
    /// A vault exists, or a key may, and it cannot be opened right now.
    Locked(LockReason),
    /// Open.
    Unlocked,
    /// Something failed in a way retrying will not fix on its own.
    Failed(String),
}

/// What the key store answered for this vault's root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredRoot {
    /// A root.
    Found,
    /// Nothing stored.
    Absent,
    /// The store was reached and refused, with its message.
    Refused(String),
}

/// What to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VaultStep {
    /// Nothing: the phase says it all.
    Rest,
    /// Open the vault under the stored root (or the one just stored) →
    /// [`VaultSession::opened`].
    Open,
    /// Generate a root and store it **before** opening: a vault opened under
    /// a key never persisted is unreadable on the next launch →
    /// [`VaultSession::created`].
    GenerateAndStore,
    /// Store the paired root, then open with the pairing's bundle →
    /// [`VaultSession::adopted`].
    StoreAdopted,
    /// Shut the open core down and release the vault's lock.
    Close,
}

/// The vault session. See the module comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultSession {
    phase: VaultPhase,
    configuration_error: Option<String>,
}

impl VaultSession {
    /// A session not yet started; `configuration_error` is a failure no
    /// retry fixes (no app-support directory, an unusable vault binding).
    #[must_use]
    pub const fn new(configuration_error: Option<String>) -> Self {
        Self {
            phase: VaultPhase::Starting,
            configuration_error,
        }
    }

    /// The phase.
    #[must_use]
    pub const fn phase(&self) -> &VaultPhase {
        &self.phase
    }

    /// Decide the launch state from the key store's answer and whether a
    /// vault directory exists.
    pub fn start(&mut self, stored: &StoredRoot, vault_exists: bool) -> VaultStep {
        if self.phase == VaultPhase::Unlocked {
            return VaultStep::Rest;
        }
        if let Some(error) = &self.configuration_error {
            self.phase = VaultPhase::Failed(error.clone());
            return VaultStep::Rest;
        }
        self.phase = match stored {
            StoredRoot::Found => return self.starting(VaultStep::Open),
            StoredRoot::Refused(detail) => {
                VaultPhase::Locked(LockReason::KeyStoreUnavailable(detail.clone()))
            }
            StoredRoot::Absent if vault_exists => {
                VaultPhase::Locked(LockReason::KeyMissingForExistingVault)
            }
            StoredRoot::Absent => VaultPhase::FirstRun,
        };
        VaultStep::Rest
    }

    fn starting(&mut self, step: VaultStep) -> VaultStep {
        self.phase = VaultPhase::Starting;
        step
    }

    /// Create the vault. Only from [`VaultPhase::FirstRun`]: this is the one
    /// operation that can make existing data unreadable, so it refuses to run
    /// where existing data might be.
    pub fn create(&mut self) -> VaultStep {
        if self.phase != VaultPhase::FirstRun {
            return VaultStep::Rest;
        }
        self.starting(VaultStep::GenerateAndStore)
    }

    /// The created root was stored, or why not. A root that could not be
    /// stored never opens a vault.
    pub fn created(&mut self, stored: Result<(), String>) -> VaultStep {
        self.after_store(stored)
    }

    /// Take a root a pairing delivered. Refused while a vault is open — a
    /// live core holds its lock and a working root — and for a root of the
    /// wrong width.
    pub fn adopt(&mut self, root: &[u8]) -> VaultStep {
        if self.phase == VaultPhase::Unlocked {
            return VaultStep::Rest;
        }
        if let Err(BadVaultRoot { len }) = vault_root(root) {
            self.phase = VaultPhase::Failed(format!("vault root must be 32 bytes, got {len}"));
            return VaultStep::Rest;
        }
        self.starting(VaultStep::StoreAdopted)
    }

    /// The adopted root was stored, or why not.
    pub fn adopted(&mut self, stored: Result<(), String>) -> VaultStep {
        self.after_store(stored)
    }

    fn after_store(&mut self, stored: Result<(), String>) -> VaultStep {
        match stored {
            Ok(()) => VaultStep::Open,
            Err(message) => {
                self.phase = VaultPhase::Failed(message);
                VaultStep::Rest
            }
        }
    }

    /// The core opened, or why not.
    pub fn opened(&mut self, opened: Result<(), String>) {
        self.phase = match opened {
            Ok(()) => VaultPhase::Unlocked,
            Err(message) => VaultPhase::Failed(message),
        };
    }

    /// Close the vault. Lands on [`LockReason::LockedByUser`], never on
    /// [`VaultPhase::Starting`]: starting is transient, driven out again by
    /// the call that entered it, and a lock that stopped there would leave a
    /// spinner nothing replaces.
    pub fn lock(&mut self) -> VaultStep {
        let was_open = self.phase == VaultPhase::Unlocked;
        self.phase = VaultPhase::Locked(LockReason::LockedByUser);
        if was_open {
            VaultStep::Close
        } else {
            VaultStep::Rest
        }
    }

    /// Switch to another vault. `same_vault` is whether it is the selected
    /// one; switching to the open vault is nothing. The caller closes the
    /// open core **before** opening the next — the vault lock admits one open
    /// vault per process — re-points the key store and the directory
    /// together, then calls [`Self::start`].
    pub fn switch(&mut self, same_vault: bool, binding_error: Option<String>) -> VaultStep {
        if same_vault && self.phase == VaultPhase::Unlocked {
            return VaultStep::Rest;
        }
        if let Some(error) = binding_error {
            // Resolving costs nothing and can fail, so a bad descriptor
            // leaves the open vault where it was.
            self.phase = VaultPhase::Failed(error);
            return VaultStep::Rest;
        }
        self.configuration_error = None;
        self.starting(VaultStep::Close)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(stored: &StoredRoot, exists: bool) -> (VaultSession, VaultStep) {
        let mut s = VaultSession::new(None);
        let step = s.start(stored, exists);
        (s, step)
    }

    #[test]
    fn no_key_and_no_vault_is_a_first_run() {
        let (s, step) = started(&StoredRoot::Absent, false);
        assert_eq!((s.phase(), step), (&VaultPhase::FirstRun, VaultStep::Rest));
    }

    #[test]
    fn a_refusing_key_store_locks_rather_than_looking_like_a_first_run() {
        let (s, _) = started(&StoredRoot::Refused("the Keychain is locked".into()), false);
        assert_eq!(
            s.phase(),
            &VaultPhase::Locked(LockReason::KeyStoreUnavailable(
                "the Keychain is locked".into()
            ))
        );
    }

    #[test]
    fn a_vault_with_no_key_locks_and_refuses_to_create_a_new_one() {
        let (mut s, _) = started(&StoredRoot::Absent, true);
        assert_eq!(
            s.phase(),
            &VaultPhase::Locked(LockReason::KeyMissingForExistingVault)
        );
        assert_eq!(s.create(), VaultStep::Rest, "creating must be refused");
        assert_eq!(
            s.phase(),
            &VaultPhase::Locked(LockReason::KeyMissingForExistingVault)
        );
    }

    #[test]
    fn creating_stores_the_key_before_the_vault_opens() {
        let (mut s, _) = started(&StoredRoot::Absent, false);
        assert_eq!(s.create(), VaultStep::GenerateAndStore);
        assert_eq!(s.phase(), &VaultPhase::Starting);
        assert_eq!(s.created(Ok(())), VaultStep::Open);
        s.opened(Ok(()));
        assert_eq!(s.phase(), &VaultPhase::Unlocked);
    }

    #[test]
    fn a_key_that_cannot_be_stored_does_not_produce_a_vault() {
        let (mut s, _) = started(&StoredRoot::Absent, false);
        s.create();
        assert_eq!(
            s.created(Err("the Keychain is locked".into())),
            VaultStep::Rest
        );
        assert_eq!(
            s.phase(),
            &VaultPhase::Failed("the Keychain is locked".into())
        );
    }

    #[test]
    fn a_stored_key_opens_without_asking() {
        let (mut s, step) = started(&StoredRoot::Found, true);
        assert_eq!(step, VaultStep::Open);
        s.opened(Ok(()));
        assert_eq!(
            s.start(&StoredRoot::Found, true),
            VaultStep::Rest,
            "already open"
        );
    }

    #[test]
    fn an_unusable_configuration_stays_a_failure_across_retries() {
        let mut s = VaultSession::new(Some("no Application Support directory".into()));
        for _ in 0..2 {
            assert_eq!(s.start(&StoredRoot::Found, true), VaultStep::Rest);
            assert_eq!(
                s.phase(),
                &VaultPhase::Failed("no Application Support directory".into())
            );
        }
    }

    #[test]
    fn locking_lands_on_a_screen_with_a_way_out() {
        let (mut s, _) = started(&StoredRoot::Found, true);
        s.opened(Ok(()));
        assert_eq!(s.lock(), VaultStep::Close);
        assert_eq!(s.phase(), &VaultPhase::Locked(LockReason::LockedByUser));
        assert_ne!(s.phase(), &VaultPhase::Starting);
        assert_eq!(
            s.start(&StoredRoot::Found, true),
            VaultStep::Open,
            "unlocking needs no ceremony"
        );
    }

    #[test]
    fn a_user_lock_offers_unlock_and_the_rest_try_again() {
        assert_eq!(LockReason::LockedByUser.repair(), LockRepair::Unlock);
        assert_eq!(
            LockReason::KeyMissingForExistingVault.repair(),
            LockRepair::TryAgain
        );
        assert_eq!(
            LockReason::KeyStoreUnavailable(String::new()).repair(),
            LockRepair::TryAgain
        );
    }

    #[test]
    fn a_vault_that_will_not_open_is_a_failure_not_a_first_run() {
        let (mut s, _) = started(&StoredRoot::Found, true);
        s.opened(Err("the Keychain is locked".into()));
        assert_eq!(
            s.phase(),
            &VaultPhase::Failed("the Keychain is locked".into())
        );
    }

    #[test]
    fn adopting_a_paired_root_is_refused_while_open_or_misshapen() {
        let (mut s, _) = started(&StoredRoot::Absent, true);
        assert_eq!(s.adopt(&[0; 31]), VaultStep::Rest);
        assert_eq!(
            s.phase(),
            &VaultPhase::Failed("vault root must be 32 bytes, got 31".into())
        );
        assert_eq!(s.adopt(&[0; 32]), VaultStep::StoreAdopted);
        assert_eq!(s.adopted(Ok(())), VaultStep::Open);
        s.opened(Ok(()));
        assert_eq!(
            s.adopt(&[0; 32]),
            VaultStep::Rest,
            "a live core holds the lock"
        );
        assert_eq!(s.phase(), &VaultPhase::Unlocked);
    }

    #[test]
    fn switching_closes_first_and_a_bad_binding_leaves_the_vault_open() {
        let (mut s, _) = started(&StoredRoot::Found, true);
        s.opened(Ok(()));
        assert_eq!(s.switch(true, None), VaultStep::Rest, "already on it");
        assert_eq!(
            s.switch(false, Some("no such vault".into())),
            VaultStep::Rest
        );
        assert_eq!(s.phase(), &VaultPhase::Failed("no such vault".into()));
        let (mut s, _) = started(&StoredRoot::Found, true);
        s.opened(Ok(()));
        assert_eq!(s.switch(false, None), VaultStep::Close);
        assert_eq!(s.phase(), &VaultPhase::Starting);
        assert_eq!(s.start(&StoredRoot::Absent, false), VaultStep::Rest);
        assert_eq!(
            s.phase(),
            &VaultPhase::FirstRun,
            "a new vault arrives at first run"
        );
    }
}
