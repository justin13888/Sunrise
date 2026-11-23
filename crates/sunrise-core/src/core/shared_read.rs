//! Reading a shared Stream as its recipient (ADR-0076,
//! `docs/11-adr/0076-a-shared-stream-carries-its-writer-certs-and-re-sealed-grants.md`).
//!
//! The recipient holds the granter's `ID_S_pub`, from the link, and the
//! Stream keys its accepted grants sealed (`held_share_keys`). Everything
//! else it needs travels in the Stream:
//!
//! - **Writer certs** (`StreamWriterCert`). One is believed when its chain
//!   verifies from its own genesis and holds the granter's identity, and the
//!   cert verifies under an identity on that chain. The envelope that carries
//!   it verifies under the cert itself where the cert names the envelope's
//!   device, and otherwise under a cert already believed.
//! - **Every other op** verifies under a believed cert of its device, or
//!   waits for one ([`SharedPending::UnknownWriter`]).
//! - **Grant copies** (`StreamShareGrant`) to this account are opened with
//!   its `ID_D_priv`, under an identity of a chain that holds the granter,
//!   and the epoch they seal is held beside the others, under the granter
//!   the share was accepted from.
//!
//! Nothing is read once the share has expired on this account's clock
//! ([`SharedRead::Expired`]), whatever the relay sends.
//!
//! [`SharedStreamReader`] is what a recipient believes about one shared
//! Stream, kept in memory by its caller. Applying what it reads, in a
//! replica of the recipient's own (ADR-0076 §3), is not built here: an op
//! read is reported, not materialized.

use super::{Core, CoreError};
use crate::control_op::{StreamShareGrantPayload, StreamWriterCertPayload};
use crate::engine::EngineError;
use crate::inner_op::{decode_inner_op, InnerOp, InnerOpError};
use crate::keychain::{held_shares, HoldShareKey, KeychainError};
use std::collections::BTreeMap;
use sunrise_crypto::{
    decode_envelope, decode_share_grant, identity_id_from_pub, open_envelope_unverified,
    verify_envelope, DeviceCert, IdentityChain, OpEnvelope, ShareGrantError,
};

/// An identity of the owner's: its id and `ID_S_pub`.
type Identity = ([u8; 16], [u8; 32]);

/// What a recipient believes about one shared Stream: the identity it
/// accepted the share from, the owner's identities that chain to it, and
/// the writers whose certs it believes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedStreamReader {
    granter_identity_id: [u8; 16],
    granter_id_s_pub: [u8; 32],
    stream_id: [u8; 16],
    /// Identity id to `ID_S_pub`, for every identity on a chain that held
    /// the granter's.
    identities: BTreeMap<[u8; 16], [u8; 32]>,
    /// Device id to `D_S_pub`, for every writer cert believed.
    writers: BTreeMap<[u8; 16], [u8; 32]>,
}

/// What reading one envelope of a shared Stream found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SharedRead {
    /// An op of the owner's, verified under its writer's cert and opened
    /// under a held key.
    Op {
        /// The device that wrote it.
        device_id: [u8; 16],
        /// Its `seq` in that device's chain.
        seq: u64,
        /// Its op-log kind (`task.create`, `stream.digest`, …).
        kind: &'static str,
    },
    /// A writer cert, now believed: the device's ops verify from here on.
    WriterCert {
        /// The device the cert names.
        device_id: [u8; 16],
    },
    /// A re-sealed grant to this account, opened and held.
    EpochHeld {
        /// The epoch it sealed.
        epoch: u32,
    },
    /// A re-sealed grant to another recipient of the Stream.
    OtherRecipient,
    /// Not readable yet; read it again once what it waits for arrives.
    Pending(SharedPending),
    /// The share has expired on this account's clock, and nothing of it is
    /// read.
    Expired,
}

/// What an envelope of a shared Stream waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedPending {
    /// No key is held at the envelope's epoch.
    NoKey {
        /// The epoch.
        epoch: u32,
    },
    /// No cert of the envelope's signer is believed yet.
    UnknownWriter {
        /// The signer.
        device_id: [u8; 16],
    },
}

impl SharedStreamReader {
    /// A reader of `stream_id`, shared by the identity whose `ID_S_pub` the
    /// link carried, believing no writer yet.
    #[must_use]
    pub fn new(granter_id_s_pub: [u8; 32], stream_id: [u8; 16]) -> Self {
        let granter_identity_id = identity_id_from_pub(&granter_id_s_pub);
        Self {
            granter_identity_id,
            granter_id_s_pub,
            stream_id,
            identities: BTreeMap::from([(granter_identity_id, granter_id_s_pub)]),
            writers: BTreeMap::new(),
        }
    }

    /// The identity the share was accepted from.
    #[must_use]
    pub fn granter_identity_id(&self) -> [u8; 16] {
        self.granter_identity_id
    }

    /// The owner's Stream.
    #[must_use]
    pub fn stream_id(&self) -> [u8; 16] {
        self.stream_id
    }

    /// Whether a cert of `device_id` is believed.
    #[must_use]
    pub fn believes_writer(&self, device_id: &[u8; 16]) -> bool {
        self.writers.contains_key(device_id)
    }

    /// Every identity on `chain`, once it verifies from its own genesis and
    /// holds the granter's identity under the granter's key.
    fn chain_identities(&self, chain: &[u8]) -> Result<Vec<Identity>, String> {
        let chain = IdentityChain::decode(chain).map_err(|e| format!("chain: {e}"))?;
        chain
            .verify(&chain.genesis_identity_id())
            .map_err(|e| format!("chain: {e}"))?;
        let mut ids = vec![(chain.genesis_identity_id(), chain.genesis_id_s_pub)];
        ids.extend(
            chain
                .links
                .iter()
                .map(|l| (l.body.to_identity_id, l.body.to_id_s_pub)),
        );
        if !ids.contains(&(self.granter_identity_id, self.granter_id_s_pub)) {
            return Err("the chain does not hold the granter's identity".into());
        }
        Ok(ids)
    }

    /// Believe the writer cert `p` carried by `env`, or say whose cert the
    /// envelope waits for.
    fn believe_cert(
        &mut self,
        env: &OpEnvelope,
        p: &StreamWriterCertPayload,
    ) -> Result<SharedRead, String> {
        let ids = self.chain_identities(&p.chain)?;
        let cert = DeviceCert::from_cbor(&p.cert).map_err(|e| format!("cert: {e}"))?;
        if !ids
            .iter()
            .any(|(id, pk)| cert.verify_binding(pk, id).is_ok())
        {
            return Err("the cert verifies under no identity on its chain".into());
        }
        let signer = if cert.body.device_id == env.device_id {
            cert.body.d_s_pub
        } else if let Some(pk) = self.writers.get(&env.device_id) {
            *pk
        } else {
            return Ok(SharedRead::Pending(SharedPending::UnknownWriter {
                device_id: env.device_id,
            }));
        };
        verify_envelope(env, &signer).map_err(|e| format!("verify: {e}"))?;
        self.identities.extend(ids);
        self.writers.insert(cert.body.device_id, cert.body.d_s_pub);
        Ok(SharedRead::WriterCert {
            device_id: cert.body.device_id,
        })
    }
}

/// A shared Stream's envelope that does not read, as the engine names one.
fn invalid(why: impl Into<String>) -> CoreError {
    CoreError::Engine(EngineError::RemoteOpInvalid(why.into()))
}

impl Core {
    /// Read one envelope of the shared Stream `reader` follows, as this
    /// account: verify it, open it under a held key, believe a writer cert,
    /// and hold the epoch a grant copy to this account seals.
    ///
    /// # Errors
    /// [`CoreError::Closed`] after close; [`EngineError::Invalid`] where no
    /// share of the Stream from the reader's granter is held;
    /// [`EngineError::RemoteOpInvalid`] for an envelope of another Stream,
    /// one that does not decode, verify or open, a cert or grant copy whose
    /// chain does not hold the granter, and a grant copy that names another
    /// Stream or does not open; storage failures. A refused envelope changes
    /// nothing the reader believes.
    pub fn read_shared_envelope(
        &self,
        reader: &mut SharedStreamReader,
        envelope: &[u8],
    ) -> Result<SharedRead, CoreError> {
        self.ensure_open()?;
        let now_ms = self.now_ms();
        let db = self.db.lock();
        let held = held_shares(&db)?
            .into_iter()
            .find(|h| {
                h.granter_identity_id == reader.granter_identity_id
                    && h.stream_id == reader.stream_id
            })
            .ok_or_else(|| {
                CoreError::Engine(EngineError::Invalid(
                    "no share of the Stream is held from that granter".into(),
                ))
            })?;
        if held.expires_at_ms.is_some_and(|at| now_ms >= at) {
            return Ok(SharedRead::Expired);
        }
        let env = decode_envelope(envelope).map_err(|e| invalid(format!("decode: {e}")))?;
        if env.stream_id != reader.stream_id {
            return Err(invalid("the envelope is of another Stream"));
        }
        let keychain = self.engine.keychain();
        let Some(key) = keychain.held_share_key(
            &db,
            &reader.granter_identity_id,
            &reader.stream_id,
            env.epoch,
        )?
        else {
            return Ok(SharedRead::Pending(SharedPending::NoKey {
                epoch: env.epoch,
            }));
        };
        // The AEAD tag authenticates the payload to a holder of the key; the
        // signature, checked before anything is believed or reported, says
        // which writer it was.
        let inner_cbor =
            open_envelope_unverified(&env, &key).map_err(|e| invalid(format!("open: {e}")))?;
        let inner = match decode_inner_op(&inner_cbor) {
            Ok(inner) => Some(inner),
            // A newer build's family: reported once its writer verifies.
            Err(InnerOpError::UnknownKind(_)) => None,
            Err(e) => return Err(invalid(format!("inner op: {e}"))),
        };
        if let Some(InnerOp::StreamWriterCert(p)) = &inner {
            return reader.believe_cert(&env, p).map_err(invalid);
        }
        let Some(signer) = reader.writers.get(&env.device_id).copied() else {
            return Ok(SharedRead::Pending(SharedPending::UnknownWriter {
                device_id: env.device_id,
            }));
        };
        verify_envelope(&env, &signer).map_err(|e| invalid(format!("verify: {e}")))?;
        let Some(inner) = inner else {
            return Ok(SharedRead::Op {
                device_id: env.device_id,
                seq: env.seq,
                kind: "unknown",
            });
        };
        let InnerOp::StreamShareGrant(p) = &inner else {
            return Ok(SharedRead::Op {
                device_id: env.device_id,
                seq: env.seq,
                kind: inner.inner_kind(),
            });
        };
        let relay_grant_id = held.relay_grant_ids.first().copied().unwrap_or_default();
        self.hold_grant_copy(&db, reader, p, relay_grant_id, now_ms)
    }

    /// Open the grant copy `p` read from `reader`'s Stream, if it is to this
    /// account, and hold the epoch it seals under the reader's granter,
    /// beside `relay_grant_id`.
    fn hold_grant_copy(
        &self,
        db: &sunrise_storage::Db,
        reader: &mut SharedStreamReader,
        p: &StreamShareGrantPayload,
        relay_grant_id: [u8; 16],
        now_ms: u64,
    ) -> Result<SharedRead, CoreError> {
        let keychain = self.engine.keychain();
        let ids = reader.chain_identities(&p.chain).map_err(invalid)?;
        let grant = decode_share_grant(&p.grant).map_err(KeychainError::from)?;
        let terms = grant.terms().clone();
        if terms.stream_id != reader.stream_id {
            return Err(invalid("the grant copy names another Stream"));
        }
        if terms.recipient_identity_id != keychain.identity_id() {
            return Ok(SharedRead::OtherRecipient);
        }
        let mut refusal = ShareGrantError::BadSignature;
        for (_, id_s_pub) in &ids {
            match keychain.open_share_grant(&grant, id_s_pub, now_ms) {
                Ok(opened) => {
                    keychain.hold_share_key(
                        db,
                        &HoldShareKey {
                            granter_identity_id: reader.granter_identity_id,
                            stream_id: reader.stream_id,
                            epoch: terms.epoch,
                            relay_grant_id,
                            name: &opened.metadata.name,
                            color: &opened.metadata.color,
                            expires_at_ms: terms.expires_at_ms,
                            accepted_at_ms: now_ms,
                        },
                        &opened.key,
                        self.rng(),
                    )?;
                    reader.identities.extend(ids);
                    return Ok(SharedRead::EpochHeld { epoch: terms.epoch });
                }
                Err(KeychainError::ShareGrant(ShareGrantError::BadSignature)) => {}
                Err(KeychainError::ShareGrant(ShareGrantError::Expired)) => {
                    return Ok(SharedRead::Expired);
                }
                Err(KeychainError::ShareGrant(e)) => refusal = e,
                Err(e) => return Err(e.into()),
            }
        }
        Err(KeychainError::ShareGrant(refusal).into())
    }
}
