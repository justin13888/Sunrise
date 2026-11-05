//! The account's OIDC session over UniFFI: see
//! [`sunrise_client_core::account_session`].

use std::sync::{Arc, Mutex};

use sunrise_client_core::account_session::{
    self as account, AccountFailure, AccountState, SessionTicket, SignOutResidue,
};

use super::lock;
use crate::LoginCredentials;

/// The credential store's synchronous half, which the foreign side owns.
///
/// Each returns `None` when it worked and the store's message when it did not.
#[uniffi::export(with_foreign)]
pub trait AccountCredentialStore: Send + Sync + 'static {
    /// Persist `credentials`, replacing what is there.
    fn save(&self, credentials: LoginCredentials) -> Option<String>;
    /// Remove the stored credential.
    fn clear(&self) -> Option<String>;
}

/// See [`sunrise_client_core::account_session::AccountFailure`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AccountSessionFailure {
    /// No issuer or no client id.
    NotConfigured,
    /// The issuer refused the refresh token.
    RefusedByIssuer,
    /// The store refused.
    StoreRefused {
        /// Its message.
        message: String,
    },
    /// The login or renewal failed.
    Failed {
        /// The message.
        message: String,
    },
}

/// See [`sunrise_client_core::account_session::AccountState`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AccountSessionState {
    /// No token.
    SignedOut,
    /// The browser is open.
    AwaitingBrowser,
    /// A bearer expiring at `expires_at_ms`.
    SignedIn {
        /// Wall-clock ms.
        expires_at_ms: u64,
    },
    /// Failed.
    Failed {
        /// Why.
        failure: AccountSessionFailure,
    },
}

/// See [`sunrise_client_core::account_session::SignOutResidue`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum CoreSignOutResidue {
    /// Nothing left.
    None,
    /// The store would not clear; unread.
    Unread {
        /// Its message.
        message: String,
    },
    /// Read.
    Acknowledged,
    /// Dismissed.
    Retired,
}

/// What the store's read answered.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum CredentialRead {
    /// A credential.
    Found {
        /// It.
        credentials: LoginCredentials,
    },
    /// None stored.
    Absent,
    /// Reached and refused.
    Refused {
        /// Its message.
        message: String,
        /// Whether a copy may be somewhere nobody read.
        may_have_left_copy_unread: bool,
    },
}

/// See [`sunrise_client_core::account_session::SignInStep`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum SignInStep {
    /// Nothing to do.
    Rest,
    /// Read the store again → [`AccountSession::looked_again`].
    LookAgain,
    /// Run the login under `ticket`.
    Login {
        /// The ticket.
        ticket: u64,
    },
}

/// See [`sunrise_client_core::account_session::RenewalStep`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum RenewalStep {
    /// Not due.
    NotDue,
    /// Expired with no refresh token: sign out.
    SignOut,
    /// Refresh under `ticket` → [`AccountSession::renewed`].
    Refresh {
        /// The ticket.
        ticket: u64,
        /// The refresh token.
        refresh_token: String,
    },
}

/// How a refresh ended.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum RenewalAnswer {
    /// A fresh credential.
    Renewed {
        /// It.
        credentials: LoginCredentials,
    },
    /// Failed.
    Failed {
        /// The message.
        message: String,
        /// Whether the issuer refused the refresh token (`invalid_grant`).
        refused_by_issuer: bool,
    },
}

impl From<LoginCredentials> for sunrise_auth::Credentials {
    fn from(c: LoginCredentials) -> Self {
        Self {
            access_token: c.access_token,
            refresh_token: c.refresh_token,
            expires_at_ms: c.expires_at_ms,
            renew_at_ms: c.renew_at_ms,
        }
    }
}

fn read(r: CredentialRead) -> Result<Option<sunrise_auth::Credentials>, account::StoreRefusal> {
    match r {
        CredentialRead::Found { credentials } => Ok(Some(credentials.into())),
        CredentialRead::Absent => Ok(None),
        CredentialRead::Refused {
            message,
            may_have_left_copy_unread,
        } => Err(account::StoreRefusal {
            message,
            may_have_left_copy_unread,
        }),
    }
}

fn sign_in_step(step: account::SignInStep) -> SignInStep {
    match step {
        account::SignInStep::Rest => SignInStep::Rest,
        account::SignInStep::LookAgain => SignInStep::LookAgain,
        account::SignInStep::Login(SessionTicket(ticket)) => SignInStep::Login { ticket },
    }
}

/// See [`sunrise_client_core::account_session::AccountSession`].
///
/// The store's `save` and `clear` are called while the session is held, so an
/// implementation must not call back into this object.
#[derive(uniffi::Object)]
pub struct AccountSession {
    session: Mutex<account::AccountSession>,
    store: Arc<dyn AccountCredentialStore>,
}

impl std::fmt::Debug for AccountSession {
    /// The state only: the session holds a live bearer.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountSession").finish_non_exhaustive()
    }
}

#[uniffi::export]
impl AccountSession {
    /// Signed out, over `store`.
    #[uniffi::constructor]
    #[must_use]
    pub fn new(store: Arc<dyn AccountCredentialStore>) -> Arc<Self> {
        Arc::new(Self {
            session: Mutex::new(account::AccountSession::new()),
            store,
        })
    }

    /// The state.
    #[must_use]
    pub fn state(&self) -> AccountSessionState {
        match lock(&self.session).state() {
            AccountState::SignedOut => AccountSessionState::SignedOut,
            AccountState::AwaitingBrowser => AccountSessionState::AwaitingBrowser,
            AccountState::SignedIn { expires_at_ms } => AccountSessionState::SignedIn {
                expires_at_ms: *expires_at_ms,
            },
            AccountState::Failed(f) => AccountSessionState::Failed {
                failure: match f {
                    AccountFailure::NotConfigured => AccountSessionFailure::NotConfigured,
                    AccountFailure::RefusedByIssuer => AccountSessionFailure::RefusedByIssuer,
                    AccountFailure::StoreRefused(message) => AccountSessionFailure::StoreRefused {
                        message: message.clone(),
                    },
                    AccountFailure::Failed(message) => AccountSessionFailure::Failed {
                        message: message.clone(),
                    },
                },
            },
        }
    }

    /// The bearer to present.
    #[must_use]
    pub fn access_token(&self) -> Option<String> {
        lock(&self.session).access_token().map(str::to_owned)
    }

    /// What the last sign-out left behind.
    #[must_use]
    pub fn residue(&self) -> CoreSignOutResidue {
        match lock(&self.session).residue() {
            SignOutResidue::None => CoreSignOutResidue::None,
            SignOutResidue::Unread(message) => CoreSignOutResidue::Unread {
                message: message.clone(),
            },
            SignOutResidue::Acknowledged => CoreSignOutResidue::Acknowledged,
            SignOutResidue::Retired => CoreSignOutResidue::Retired,
        }
    }

    /// Take the store's read.
    pub fn restore(&self, loaded: CredentialRead) {
        lock(&self.session).restore(read(loaded));
    }

    /// Start a sign-in.
    #[must_use]
    pub fn begin_sign_in(&self, issuer: String, client_id: String) -> SignInStep {
        sign_in_step(lock(&self.session).begin_sign_in(&issuer, &client_id))
    }

    /// The second look [`SignInStep::LookAgain`] asked for.
    #[must_use]
    pub fn looked_again(
        &self,
        loaded: CredentialRead,
        issuer: String,
        client_id: String,
    ) -> SignInStep {
        sign_in_step(lock(&self.session).looked_again(read(loaded), &issuer, &client_id))
    }

    /// The login is about to open the browser; `false` to open nothing.
    #[must_use]
    pub fn browser_opening(&self, ticket: u64) -> bool {
        lock(&self.session).browser_opening(SessionTicket(ticket))
    }

    /// The login finished: a credential, or the message.
    pub fn login_finished(
        &self,
        ticket: u64,
        credentials: Option<LoginCredentials>,
        error: Option<String>,
    ) {
        let outcome = match (credentials, error) {
            (_, Some(message)) => Err(message),
            (Some(c), None) => Ok(c.into()),
            (None, None) => Err("the login returned no credential".to_owned()),
        };
        let store = Arc::clone(&self.store);
        lock(&self.session).login_finished(SessionTicket(ticket), outcome, |c| {
            store.save(c.clone().into()).map_or(Ok(()), Err)
        });
    }

    /// Whether renewal is due at `now_ms`.
    #[must_use]
    pub fn renewal_due(&self, now_ms: u64) -> RenewalStep {
        match lock(&self.session).renewal_due(now_ms) {
            account::RenewalStep::NotDue => RenewalStep::NotDue,
            account::RenewalStep::SignOut => RenewalStep::SignOut,
            account::RenewalStep::Refresh {
                ticket: SessionTicket(ticket),
                refresh_token,
            } => RenewalStep::Refresh {
                ticket,
                refresh_token,
            },
        }
    }

    /// The refresh finished.
    pub fn renewed(&self, ticket: u64, answer: RenewalAnswer, now_ms: u64) {
        let outcome = match answer {
            RenewalAnswer::Renewed { credentials } => Ok(credentials.into()),
            RenewalAnswer::Failed {
                message,
                refused_by_issuer,
            } => Err(account::RenewalFailure {
                message,
                refused_by_issuer,
            }),
        };
        let (save, clear) = (Arc::clone(&self.store), Arc::clone(&self.store));
        lock(&self.session).renewed(
            SessionTicket(ticket),
            outcome,
            now_ms,
            |c| save.save(c.clone().into()).map_or(Ok(()), Err),
            || clear.clear().map_or(Ok(()), Err),
        );
    }

    /// Sign out, here and in the store.
    pub fn sign_out(&self) {
        let store = Arc::clone(&self.store);
        lock(&self.session).sign_out(|| store.clear().map_or(Ok(()), Err));
    }

    /// The user read the incomplete sign-out.
    pub fn dismiss_sign_out_incomplete(&self) {
        lock(&self.session).dismiss_sign_out_incomplete();
    }

    /// Stop saying anything about the refused sign-out.
    pub fn dismiss_sign_out_retry(&self) {
        lock(&self.session).dismiss_sign_out_retry();
    }
}
