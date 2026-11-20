//! The account surface: revoking a device, the key bundle and identity chain
//! pins, and the recovery blob. `session` holds the trait methods these back.
//!
//! Each route goes through the generated relay client (`rest`), so the bodies
//! it sends and the replies it reads are the description's types.

use super::rest::{bind, settle, Settled, DOCUMENT_CALL};
use super::SseTransport;
use crate::transport::{
    IdentityChainPinned, KeyBundlePin, KeyBundlePinned, RevokeOutcome, TransportError,
};
use base64::Engine as _;
use sunrise_relay_client::api::{self, types};

impl SseTransport {
    pub(super) async fn revoke(
        &self,
        device_id: [u8; 16],
    ) -> Result<RevokeOutcome, TransportError> {
        // `by-vault-id`, not `/{device_id}`: that route names the ULID the
        // relay minted, which this vault has never held for any peer. Naming
        // the vault id is the whole reason the route exists.
        let vault_id = sunrise_id::crockford::encode_bytes(&device_id);
        let path = format!("/api/v1/devices/by-vault-id/{vault_id}");
        let binding = self.sign_empty("DELETE", &path)?;
        let exchange = self.exchange(DOCUMENT_CALL)?;
        let result = exchange
            .client()
            .revoke_device_by_vault_id(
                vault_id,
                bind!(api::RevokeDeviceByVaultIdParams::default(), binding),
            )
            .await;
        let reply = match settle(&exchange, result)? {
            Settled::Value(()) => return Ok(RevokeOutcome::Revoked),
            Settled::Refused(reply) => reply,
        };
        // Terminal but not success. The relay holds no active row with this
        // vault id, and no amount of retrying will make one appear — but the
        // device may still be registered under a row from before
        // `vault_device_id` existed, in which case it is still being accepted.
        // The caller says so out loud rather than reporting a revocation.
        if reply.status.as_u16() == 404 {
            return Ok(RevokeOutcome::Unknown);
        }
        Err(TransportError::Server {
            code: "AUTH_DEVICE_REVOKE_FAILED",
            message: format!(
                "relay refused the revocation: {} {}{}",
                reply.status,
                String::from_utf8_lossy(&reply.bytes),
                // The one request whose entire purpose is to work when a device
                // has been lost, so a refusal that is really a clock has to say
                // so here too rather than reading as "the relay would not".
                super::http::binding_advice(
                    reply.date.as_deref(),
                    self.signer.as_ref().map(|s| s.now_ms())
                )
            ),
            retry_after: reply.retry_after,
        })
    }

    pub(super) async fn latest_key_bundle(&self) -> Result<Option<KeyBundlePin>, TransportError> {
        let binding = self.sign_empty("GET", "/api/v1/accounts/me/key_bundle")?;
        let exchange = self.exchange(DOCUMENT_CALL)?;
        let result = exchange
            .client()
            .fetch_key_bundle(bind!(api::FetchKeyBundleParams::default(), binding))
            .await;
        let reply = match settle(&exchange, result)? {
            Settled::Value(pin) => return key_bundle_pin_of(&pin).map(Some),
            Settled::Refused(reply) => reply,
        };
        // No pin is `404 BLOB_NOT_FOUND`. Any other 404 is a relay that
        // predates the route, and a publish to it would upload a blob that
        // nothing can ever pin.
        if reply.status.as_u16() == 404 {
            return match problem_code(&reply.bytes).as_deref() {
                Some("BLOB_NOT_FOUND") => Ok(None),
                _ => Err(TransportError::Unsupported),
            };
        }
        Err(self.refuse(&reply))
    }

    pub(super) async fn pin_key_bundle(
        &self,
        blob_id: &[u8; 16],
        manifest_digest: &[u8; 32],
        replaces: Option<&[u8; 16]>,
    ) -> Result<KeyBundlePinned, TransportError> {
        let path = "/api/v1/accounts/me/key_bundle";
        let body = types::PinKeyBundleRequest {
            blob_id: format!("blb_{}", hex::encode(blob_id)),
            manifest_digest: hex::encode(manifest_digest),
            replaces: replaces.map(|r| format!("blb_{}", hex::encode(r))),
        };
        let binding = self.sign_json("PUT", path, Some(&body))?;
        let exchange = self.exchange(DOCUMENT_CALL)?;
        let result = exchange
            .client()
            .pin_key_bundle(bind!(api::PinKeyBundleParams::default(), binding), &body)
            .await;
        let reply = match settle(&exchange, result)? {
            Settled::Value(_) => return Ok(KeyBundlePinned::Pinned),
            Settled::Refused(reply) => reply,
        };
        // `409 KEY_BUNDLE_SUPERSEDED`: another device pinned since this one
        // read. Not a failure; the caller reads the pin again.
        if reply.status.as_u16() == 409 {
            return Ok(KeyBundlePinned::Superseded);
        }
        Err(self.refuse(&reply))
    }

    pub(super) async fn latest_identity_chain(&self) -> Result<Option<Vec<u8>>, TransportError> {
        let binding = self.sign_empty("GET", "/api/v1/accounts/me/identity_chain")?;
        let exchange = self.exchange(DOCUMENT_CALL)?;
        let result = exchange
            .client()
            .fetch_identity_chain(bind!(api::FetchIdentityChainParams::default(), binding))
            .await;
        let reply = match settle(&exchange, result)? {
            Settled::Value(pin) => {
                return base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(&pin.chain)
                    .map(Some)
                    .map_err(|_| {
                        TransportError::Protocol("an identity chain is malformed".to_owned())
                    })
            }
            Settled::Refused(reply) => reply,
        };
        // No record is `404 IDENTITY_CHAIN_NOT_FOUND`. Any other 404 is a
        // relay that predates the route, as for the key bundle.
        if reply.status.as_u16() == 404 {
            return match problem_code(&reply.bytes).as_deref() {
                Some("IDENTITY_CHAIN_NOT_FOUND") => Ok(None),
                _ => Err(TransportError::Unsupported),
            };
        }
        Err(self.refuse(&reply))
    }

    pub(super) async fn pin_identity_chain(
        &self,
        chain: &[u8],
        replaces: Option<&[u8; 32]>,
    ) -> Result<IdentityChainPinned, TransportError> {
        let path = "/api/v1/accounts/me/identity_chain";
        let body = types::PinIdentityChainRequest {
            chain: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(chain),
            replaces: replaces.map(hex::encode),
        };
        let binding = self.sign_json("PUT", path, Some(&body))?;
        let exchange = self.exchange(DOCUMENT_CALL)?;
        let result = exchange
            .client()
            .pin_identity_chain(
                bind!(api::PinIdentityChainParams::default(), binding),
                &body,
            )
            .await;
        let reply = match settle(&exchange, result)? {
            Settled::Value(_) => return Ok(IdentityChainPinned::Pinned),
            Settled::Refused(reply) => reply,
        };
        // `409 IDENTITY_CHAIN_SUPERSEDED`: another device pinned since this
        // one read. The caller reads again.
        if reply.status.as_u16() == 409 {
            return Ok(IdentityChainPinned::Superseded);
        }
        Err(self.refuse(&reply))
    }

    pub(super) async fn replace_recovery_blob(&self, blob: &[u8]) -> Result<(), TransportError> {
        let path = "/api/v1/accounts/me/recovery_blob";
        let body = types::ReplaceRecoveryBlobRequest {
            recovery_blob: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(blob),
        };
        let binding = self.sign_json("PUT", path, Some(&body))?;
        let exchange = self.exchange(DOCUMENT_CALL)?;
        let result = exchange
            .client()
            .replace_recovery_blob(
                bind!(api::ReplaceRecoveryBlobParams::default(), binding),
                &body,
            )
            .await;
        let reply = match settle(&exchange, result)? {
            Settled::Value(_) => return Ok(()),
            Settled::Refused(reply) => reply,
        };
        if reply.status.as_u16() == 404 || reply.status.as_u16() == 405 {
            return Err(TransportError::Unsupported);
        }
        Err(self.refuse(&reply))
    }
}

/// The `code` member of a problem document, if the body is one.
fn problem_code(bytes: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| v.get("code").and_then(|c| c.as_str()).map(str::to_owned))
}

/// A `KeyBundlePin`'s blob id and manifest digest, as bytes.
fn key_bundle_pin_of(pin: &types::KeyBundlePin) -> Result<KeyBundlePin, TransportError> {
    let malformed = || TransportError::Protocol("a key bundle pin is malformed".to_owned());
    let blob_id = super::blobs::blob_id_of(&pin.blob_id).ok_or_else(malformed)?;
    let mut manifest_digest = [0u8; 32];
    hex::decode_to_slice(&pin.manifest_digest, &mut manifest_digest).map_err(|_| malformed())?;
    Ok(KeyBundlePin {
        blob_id,
        manifest_digest,
    })
}
