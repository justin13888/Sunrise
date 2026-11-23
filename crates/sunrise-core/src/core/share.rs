//! Sharing a Stream with another account, at the core's seam
//! (`docs/03-crypto/sharing-with-others.md` §Granting access).
//!
//! The owner's half is [`Core::share_stream`]: the grant as an op, returned
//! encoded for the `sunrise://share/<token>` link. The recipient's half is
//! [`Core::open_share_grant`]: the check a client makes before it asks its
//! user to accept, which verifies the granter's signature over this account's
//! own `ID_D_pub` and opens the sealed key and metadata.
//!
//! The relay viewer grant the link's claim token names is the caller's, over
//! `sunrise-relay-client`: this crate never calls the relay outside the sync
//! driver.

use super::{Core, CoreError};
use crate::keychain::KeychainError;
use sunrise_id::{EntityKind, EntityRef};

/// A share grant sent to this account, verified and opened.
///
/// What a client shows beside "Accept?": the Stream's name and color as the
/// owner sealed them, the identity that signed it, and its expiry. The Stream
/// key it opened is not carried out: holding it is not built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedShare {
    /// The owner's Stream.
    pub stream_id: [u8; 16],
    /// The Stream-key epoch the grant seals.
    pub epoch: u32,
    /// The identity that signed the grant: `identity_id_from_pub` of the
    /// `ID_S_pub` it verified under, which the recipient compares with the
    /// owner's out of band.
    pub granter_identity_id: [u8; 16],
    /// The Stream's name, as the owner sealed it.
    pub name: String,
    /// The Stream's color, in its wire spelling.
    pub color: String,
    /// When the share ends, on this account's clock; `None` for never.
    pub expires_at_ms: Option<u64>,
}

impl Core {
    /// Share `stream` as a viewer with the identity whose keys are
    /// `recipient_id_s_pub` and `recipient_id_d_pub`, until `expires_at_ms`,
    /// or for good when it is `None`. Returns the grant's canonical encoding:
    /// what the link carries to the recipient.
    ///
    /// The Person the op names is the recipient identity's, under the same
    /// sixteen bytes (`per_` beside `idn_`), so sharing with one identity
    /// twice names one Person. No `persons` row is written: nothing reads
    /// that table yet.
    ///
    /// # Errors
    /// [`CoreError::Closed`] after close; what `Engine::share_stream`
    /// refuses otherwise, writing nothing: a ref that is not a shareable
    /// Stream, the account's own identity, an expiry already past, a revoked
    /// device, or one that holds no `ID_S_priv`.
    pub fn share_stream(
        &self,
        stream: EntityRef,
        recipient_id_s_pub: &[u8; 32],
        recipient_id_d_pub: [u8; 32],
        expires_at_ms: Option<u64>,
    ) -> Result<Vec<u8>, CoreError> {
        self.ensure_open()?;
        let recipient = recipient_person(recipient_id_s_pub, self.now_ms());
        let grant = {
            let mut db = self.db.lock();
            self.engine.share_stream(
                &mut db,
                stream,
                &recipient,
                recipient_id_d_pub,
                expires_at_ms,
            )?
        };
        // The op is in the outbox; drain it now rather than at the next frame.
        if self.sync_shared.is_active() {
            self.sync_shared.poke_submit();
        }
        Ok(grant)
    }

    /// Verify and open `grant`, signed by `granter_id_s_pub`, as this
    /// account's identity at this core's clock.
    ///
    /// Opening checks, in order, that the grant names this identity, that
    /// `granter_id_s_pub` signed it over this identity's `ID_D_pub`, and that
    /// it has not expired; only then is anything decrypted. Nothing is
    /// written.
    ///
    /// # Errors
    /// [`CoreError::Closed`] after close; [`CoreError::Keychain`] wrapping
    /// `IdentitySecretAbsent` on a device that holds no `ID_D_priv` (every
    /// paired one), or `ShareGrant` for a grant that does not decode, names
    /// another identity, was not signed by that key, has expired, or does not
    /// open.
    pub fn open_share_grant(
        &self,
        granter_id_s_pub: &[u8; 32],
        grant: &[u8],
    ) -> Result<ReceivedShare, CoreError> {
        self.ensure_open()?;
        let decoded = sunrise_crypto::decode_share_grant(grant).map_err(KeychainError::from)?;
        let opened =
            self.engine
                .keychain()
                .open_share_grant(&decoded, granter_id_s_pub, self.now_ms())?;
        let terms = decoded.terms();
        Ok(ReceivedShare {
            stream_id: terms.stream_id,
            epoch: terms.epoch,
            granter_identity_id: sunrise_crypto::identity_id_from_pub(granter_id_s_pub),
            name: opened.metadata.name.clone(),
            color: opened.metadata.color.clone(),
            expires_at_ms: terms.expires_at_ms,
        })
    }
}

/// The Person a share to the identity under `id_s_pub` names.
fn recipient_person(id_s_pub: &[u8; 32], now_ms: u64) -> sunrise_domain::Person {
    let identity = sunrise_crypto::identity_id_from_pub(id_s_pub);
    let at = jiff::Timestamp::from_millisecond(i64::try_from(now_ms).unwrap_or(i64::MAX))
        .unwrap_or(jiff::Timestamp::UNIX_EPOCH);
    sunrise_domain::Person {
        id: EntityRef::new(EntityKind::Person, identity),
        created_at: at,
        updated_at: at,
        display_name: String::new(),
        identity_id: Some(EntityRef::new(EntityKind::Identity, identity)),
        deleted: false,
        unknown: sunrise_domain::Unknowns::new(),
    }
}
