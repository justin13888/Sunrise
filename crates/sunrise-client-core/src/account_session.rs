//! The account's OIDC session: sign-in, silent renewal and sign-out, as pure
//! transitions over [`sunrise_auth::Credentials`].
//!
//! Signed out is a first-class state, not an error: a self-host relay takes
//! requests with no bearer, and the core works with no relay at all. What is
//! decided here is what the Apple app's `AccountModel` decided in Swift:
//!
//! * **When to renew.** At the credential's own renewal point — 75% of its
//!   life, [`sunrise_auth::RENEW_AT_FRACTION`] — looked at every
//!   [`RENEWAL_CHECK_INTERVAL`]. With no refresh token there is nothing to do
//!   until the bearer expires, and then the session ends.
//! * **What a failed renewal means.** The issuer refusing the refresh token in
//!   OAuth's own words ends the session (it will refuse again on every tick);
//!   anything else — an unreachable issuer above all — says nothing about the
//!   refresh token, so the bearer is kept until it expires and the refresh
//!   token after that.
//! * **Which answers still count.** A login suspends twice and a renewal once,
//!   and nothing cancels either when the user signs out. Every sign-out bumps
//!   a generation; a [`SessionTicket`] from before it is stale, and its answer
//!   writes nothing — not the token, not a spinner, not an error.
//! * **What a refused sign-out leaves.** The local half of a sign-out is
//!   unconditional; when the store will not let go of the credential,
//!   [`SignOutResidue`] says so until a save or a clear replaces it.
//!
//! The store is injected: the synchronous calls (load, save, clear) are
//! closures, and the asynchronous ones (the browser login, the refresh) are
//! the caller's, answered through the ticket they were started under.

use std::time::Duration;

use sunrise_auth::Credentials;

/// How often a running app looks at the clock to see whether renewal is due.
/// A look that is not due is one comparison; thirty seconds is far inside the
/// quarter of a token's life that is left when it comes due.
pub const RENEWAL_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// How long a login waits for the browser redirect: long enough for a
/// password manager and a second factor, short enough that an abandoned login
/// does not hold a loopback port.
pub const REDIRECT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Why the account is in [`AccountState::Failed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountFailure {
    /// No issuer or no client id is configured.
    NotConfigured,
    /// The issuer refused the refresh token: the session cannot be renewed.
    RefusedByIssuer,
    /// The credential store was reached and refused, with its message.
    StoreRefused(String),
    /// The login or the renewal failed, with the message.
    Failed(String),
}

/// The account's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountState {
    /// No token in hand.
    SignedOut,
    /// The browser is open and the loopback listener is parked.
    AwaitingBrowser,
    /// A bearer that expires at `expires_at_ms`.
    SignedIn {
        /// Wall-clock ms the bearer expires at.
        expires_at_ms: u64,
    },
    /// See [`AccountFailure`].
    Failed(AccountFailure),
}

/// What the last sign-out could not do, and how much of it the user has
/// acknowledged. Anything but `None` means the stored credential survived it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignOutResidue {
    /// Nothing left behind.
    None,
    /// The store refused to clear, with its message; not yet read.
    Unread(String),
    /// Read; the retry is still offered.
    Acknowledged,
    /// Dismissed altogether; a plain sign-out is still offered.
    Retired,
}

/// The generation a login or renewal was started under.
///
/// Public so a seam can carry it across an FFI as an integer. A forged one is
/// harmless: it is compared with the current generation, never trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionTicket(pub u64);

/// A refusal from [`AccountSession::restore`]'s store read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreRefusal {
    /// The store's message.
    pub message: String,
    /// Whether the refusal may have left a copy of the credential somewhere
    /// nobody read — the one condition under which a sign-in would write a
    /// second secret beside a first that survives. False only for a refusal
    /// that read and compared both copies (the Apple keychain's
    /// `migrationUnverified`), where signing in is the repair.
    pub may_have_left_copy_unread: bool,
}

/// What [`AccountSession::begin_sign_in`] asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInStep {
    /// Nothing to do: the state says why.
    Rest,
    /// Read the store again first, and answer with
    /// [`AccountSession::looked_again`].
    LookAgain,
    /// Run the login under this ticket: begin, then
    /// [`AccountSession::browser_opening`], then complete, then
    /// [`AccountSession::login_finished`].
    Login(SessionTicket),
}

/// What [`AccountSession::renewal_due`] asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenewalStep {
    /// Not due, or no session.
    NotDue,
    /// Expired with no refresh token: call [`AccountSession::sign_out`].
    SignOut,
    /// Exchange `refresh_token` under this ticket, then answer with
    /// [`AccountSession::renewed`].
    Refresh {
        /// The ticket.
        ticket: SessionTicket,
        /// The refresh token.
        refresh_token: String,
    },
}

/// A failed refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenewalFailure {
    /// The message.
    pub message: String,
    /// Whether the issuer refused the refresh token itself (OAuth
    /// `invalid_grant`), rather than not answering.
    pub refused_by_issuer: bool,
}

/// The account session. See the module comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSession {
    state: AccountState,
    credentials: Option<Credentials>,
    access_token: Option<String>,
    residue: SignOutResidue,
    store_refused_to_answer: bool,
    generation: u64,
}

impl Default for AccountSession {
    fn default() -> Self {
        Self::new()
    }
}

impl AccountSession {
    /// Signed out, nothing read yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: AccountState::SignedOut,
            credentials: None,
            access_token: None,
            residue: SignOutResidue::None,
            store_refused_to_answer: false,
            generation: 0,
        }
    }

    /// The state.
    #[must_use]
    pub const fn state(&self) -> &AccountState {
        &self.state
    }

    /// The bearer to present, or `None`.
    #[must_use]
    pub fn access_token(&self) -> Option<&str> {
        self.access_token.as_deref()
    }

    /// The credential in hand.
    #[must_use]
    pub const fn credentials(&self) -> Option<&Credentials> {
        self.credentials.as_ref()
    }

    /// What the last sign-out left behind.
    #[must_use]
    pub const fn residue(&self) -> &SignOutResidue {
        &self.residue
    }

    fn ticket(&self) -> SessionTicket {
        SessionTicket(self.generation)
    }

    const fn is_current(&self, ticket: SessionTicket) -> bool {
        ticket.0 == self.generation
    }

    fn publish(&mut self) {
        if let Some(c) = &self.credentials {
            self.access_token = Some(c.access_token.clone());
            self.state = AccountState::SignedIn {
                expires_at_ms: c.expires_at_ms,
            };
        } else {
            self.access_token = None;
            self.state = AccountState::SignedOut;
        }
    }

    /// Take the store's answer: a credential, none, or a refusal. A refusal
    /// is reported, never read as "signed out" — that collapse is what made a
    /// locked store look like an invitation to sign in again.
    pub fn restore(&mut self, loaded: Result<Option<Credentials>, StoreRefusal>) {
        match loaded {
            Ok(credentials) => {
                self.credentials = credentials;
                self.store_refused_to_answer = false;
                self.publish();
            }
            Err(refusal) => {
                self.credentials = None;
                self.access_token = None;
                self.store_refused_to_answer = refusal.may_have_left_copy_unread;
                self.state = AccountState::Failed(AccountFailure::StoreRefused(refusal.message));
            }
        }
    }

    /// Start a sign-in. While the store may hold a token nobody read, look
    /// again first: a sign-in then would write a second one.
    pub fn begin_sign_in(&mut self, issuer: &str, client_id: &str) -> SignInStep {
        if self.store_refused_to_answer {
            return SignInStep::LookAgain;
        }
        self.configured_sign_in(issuer, client_id)
    }

    /// The second look [`SignInStep::LookAgain`] asked for. Still refused, or
    /// a token after all, and there is no login; nothing there, and there is.
    pub fn looked_again(
        &mut self,
        loaded: Result<Option<Credentials>, StoreRefusal>,
        issuer: &str,
        client_id: &str,
    ) -> SignInStep {
        self.restore(loaded);
        if self.store_refused_to_answer || self.credentials.is_some() {
            return SignInStep::Rest;
        }
        self.configured_sign_in(issuer, client_id)
    }

    fn configured_sign_in(&mut self, issuer: &str, client_id: &str) -> SignInStep {
        if issuer.trim().is_empty() || client_id.trim().is_empty() {
            self.state = AccountState::Failed(AccountFailure::NotConfigured);
            return SignInStep::Rest;
        }
        SignInStep::Login(self.ticket())
    }

    /// The login has its authorize URL and is about to open the browser.
    /// `false` when a sign-out landed meanwhile: open nothing, show nothing.
    pub fn browser_opening(&mut self, ticket: SessionTicket) -> bool {
        if !self.is_current(ticket) {
            return false;
        }
        self.state = AccountState::AwaitingBrowser;
        true
    }

    /// The login finished. A stale one is dropped whole; a fresh credential
    /// is saved with `save` before it is used, and a save that fails is the
    /// login failing.
    pub fn login_finished(
        &mut self,
        ticket: SessionTicket,
        outcome: Result<Credentials, String>,
        save: impl FnOnce(&Credentials) -> Result<(), String>,
    ) {
        if !self.is_current(ticket) {
            return;
        }
        match outcome.and_then(|fresh| save(&fresh).map(|()| fresh)) {
            Ok(fresh) => {
                // The save replaced the credential a refused sign-out left.
                self.residue = SignOutResidue::None;
                self.credentials = Some(fresh);
                self.publish();
            }
            Err(message) => {
                self.credentials = None;
                self.access_token = None;
                self.state = AccountState::Failed(AccountFailure::Failed(message));
            }
        }
    }

    /// Whether renewal is due at `now_ms`, and what to do about it.
    pub fn renewal_due(&self, now_ms: u64) -> RenewalStep {
        let Some(current) = &self.credentials else {
            return RenewalStep::NotDue;
        };
        if !current.is_renewal_due_at(now_ms) {
            return RenewalStep::NotDue;
        }
        match &current.refresh_token {
            Some(token) => RenewalStep::Refresh {
                ticket: self.ticket(),
                refresh_token: token.clone(),
            },
            None if current.is_expired_at(now_ms) => RenewalStep::SignOut,
            None => RenewalStep::NotDue,
        }
    }

    /// The refresh finished. Silent by design: a renewed token changes the
    /// bearer and nothing on screen.
    pub fn renewed(
        &mut self,
        ticket: SessionTicket,
        outcome: Result<Credentials, RenewalFailure>,
        now_ms: u64,
        save: impl FnOnce(&Credentials) -> Result<(), String>,
        clear: impl FnOnce() -> Result<(), String>,
    ) {
        if !self.is_current(ticket) {
            return;
        }
        let saved = outcome.and_then(|fresh| match save(&fresh) {
            Ok(()) => Ok(fresh),
            Err(message) => Err(RenewalFailure {
                message,
                refused_by_issuer: false,
            }),
        });
        let failure = match saved {
            Ok(fresh) => {
                self.residue = SignOutResidue::None;
                self.credentials = Some(fresh);
                self.publish();
                return;
            }
            Err(failure) => failure,
        };
        let awaiting = self.state == AccountState::AwaitingBrowser;
        if failure.refused_by_issuer {
            // A login already in the browser is replacing it.
            if !awaiting {
                self.sign_out(clear);
                self.state = AccountState::Failed(AccountFailure::RefusedByIssuer);
            }
            return;
        }
        let expired = self
            .credentials
            .as_ref()
            .is_some_and(|c| c.is_expired_at(now_ms));
        if !expired {
            return;
        }
        // The relay refuses an expired bearer anyway; the refresh token stays
        // for the next tick, so a launch before the network recovers needs no
        // browser.
        self.access_token = None;
        if !awaiting {
            self.state = AccountState::Failed(AccountFailure::Failed(failure.message));
        }
    }

    /// Forget the token here, and in the store through `clear`. The local
    /// half is unconditional; a store that will not let go is recorded in
    /// [`Self::residue`]. Ends any login or renewal in flight.
    pub fn sign_out(&mut self, clear: impl FnOnce() -> Result<(), String>) {
        self.residue = match clear() {
            Ok(()) => SignOutResidue::None,
            Err(message) => SignOutResidue::Unread(message),
        };
        self.credentials = None;
        self.access_token = None;
        self.state = AccountState::SignedOut;
        self.generation = self.generation.wrapping_add(1);
    }

    /// The user read the incomplete sign-out; the retry stays offered.
    pub fn dismiss_sign_out_incomplete(&mut self) {
        if matches!(self.residue, SignOutResidue::Unread(_)) {
            self.residue = SignOutResidue::Acknowledged;
        }
    }

    /// Stop saying anything about the refused sign-out.
    pub fn dismiss_sign_out_retry(&mut self) {
        if matches!(
            self.residue,
            SignOutResidue::Unread(_) | SignOutResidue::Acknowledged
        ) {
            self.residue = SignOutResidue::Retired;
        }
    }
}

#[cfg(test)]
mod tests;
