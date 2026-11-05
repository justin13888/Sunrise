//! The vault session's lifecycle over UniFFI: see
//! [`sunrise_client_core::vault_session`].

use std::sync::{Arc, Mutex};

use sunrise_client_core::vault_session::{self as vault, LockReason, LockRepair, VaultStep};

use super::{lock, UnitAnswer};

/// See [`sunrise_client_core::vault_session::LockReason`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum CoreLockReason {
    /// The key store refused.
    KeyStoreUnavailable {
        /// Its message.
        detail: String,
    },
    /// A vault and no key.
    KeyMissingForExistingVault,
    /// The user closed it.
    LockedByUser,
}

/// See [`sunrise_client_core::vault_session::LockRepair`].
#[uniffi::remote(Enum)]
pub enum LockRepair {
    Unlock,
    TryAgain,
}

/// See [`sunrise_client_core::vault_session::VaultStep`].
#[uniffi::remote(Enum)]
pub enum VaultStep {
    Rest,
    Open,
    GenerateAndStore,
    StoreAdopted,
    Close,
}

/// See [`sunrise_client_core::vault_session::VaultPhase`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum VaultPhase {
    /// Reading the key store and the directory.
    Starting,
    /// Nothing at risk; offer to create.
    FirstRun,
    /// Cannot be opened right now.
    Locked {
        /// Why.
        reason: CoreLockReason,
        /// The button that resolves it.
        repair: LockRepair,
    },
    /// Open.
    Unlocked,
    /// A failure retrying will not fix.
    Failed {
        /// The message.
        message: String,
    },
}

/// What the key store answered for the vault's root.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum StoredRoot {
    /// A root.
    Found,
    /// Nothing.
    Absent,
    /// Reached and refused.
    Refused {
        /// Its message.
        detail: String,
    },
}

/// See [`sunrise_client_core::vault_session::VaultSession`].
#[derive(Debug, uniffi::Object)]
pub struct VaultSession(Mutex<vault::VaultSession>);

#[uniffi::export]
impl VaultSession {
    /// A session; `configuration_error` is a failure no retry fixes.
    #[uniffi::constructor]
    #[must_use]
    pub fn new(configuration_error: Option<String>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(vault::VaultSession::new(
            configuration_error,
        ))))
    }

    /// The phase.
    #[must_use]
    pub fn phase(&self) -> VaultPhase {
        match lock(&self.0).phase() {
            vault::VaultPhase::Starting => VaultPhase::Starting,
            vault::VaultPhase::FirstRun => VaultPhase::FirstRun,
            vault::VaultPhase::Unlocked => VaultPhase::Unlocked,
            vault::VaultPhase::Failed(message) => VaultPhase::Failed {
                message: message.clone(),
            },
            vault::VaultPhase::Locked(reason) => VaultPhase::Locked {
                repair: reason.repair(),
                reason: match reason {
                    LockReason::KeyStoreUnavailable(detail) => {
                        CoreLockReason::KeyStoreUnavailable {
                            detail: detail.clone(),
                        }
                    }
                    LockReason::KeyMissingForExistingVault => {
                        CoreLockReason::KeyMissingForExistingVault
                    }
                    LockReason::LockedByUser => CoreLockReason::LockedByUser,
                },
            },
        }
    }

    /// Decide the launch state.
    #[must_use]
    pub fn start(&self, stored: StoredRoot, vault_exists: bool) -> VaultStep {
        let stored = match stored {
            StoredRoot::Found => vault::StoredRoot::Found,
            StoredRoot::Absent => vault::StoredRoot::Absent,
            StoredRoot::Refused { detail } => vault::StoredRoot::Refused(detail),
        };
        lock(&self.0).start(&stored, vault_exists)
    }

    /// Create the vault; only from first run.
    #[must_use]
    pub fn create(&self) -> VaultStep {
        lock(&self.0).create()
    }

    /// Whether the created root was stored.
    #[must_use]
    pub fn created(&self, stored: UnitAnswer) -> VaultStep {
        lock(&self.0).created(stored.into())
    }

    /// Take a paired root.
    #[must_use]
    pub fn adopt(&self, root: Vec<u8>) -> VaultStep {
        lock(&self.0).adopt(&root)
    }

    /// Whether the adopted root was stored.
    #[must_use]
    pub fn adopted(&self, stored: UnitAnswer) -> VaultStep {
        lock(&self.0).adopted(stored.into())
    }

    /// Whether the core opened.
    pub fn opened(&self, opened: UnitAnswer) {
        lock(&self.0).opened(opened.into());
    }

    /// Close the vault.
    #[must_use]
    pub fn lock(&self) -> VaultStep {
        lock(&self.0).lock()
    }

    /// Switch to another vault.
    #[must_use]
    pub fn switch(&self, same_vault: bool, binding_error: Option<String>) -> VaultStep {
        lock(&self.0).switch(same_vault, binding_error)
    }
}
