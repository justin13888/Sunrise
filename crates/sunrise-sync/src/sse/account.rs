//! The account surface: revoking a device, the key bundle and identity chain
//! pins, and the recovery blob. `session` holds the trait methods these back.

use super::http::protocol;
use super::SseTransport;
use crate::transport::{
    IdentityChainPinned, KeyBundlePin, KeyBundlePinned, RevokeOutcome, TransportError,
};
use base64::Engine as _;

impl SseTransport {
    pub(super) async fn revoke(
        &self,
        device_id: [u8; 16],
    ) -> Result<RevokeOutcome, TransportError> {
        // `by-vault-id`, not `/{device_id}`: that route names the ULID the
        // relay minted, which this vault has never held for any peer. Naming
        // the vault id is the whole reason the route exists.
        let reply = self
            .call(
                "DELETE",
                &format!(
                    "/api/v1/devices/by-vault-id/{}",
                    sunrise_id::crockford::encode_bytes(&device_id)
                ),
                None,
            )
            .await?;
        if reply.status.is_success() {
            return Ok(RevokeOutcome::Revoked);
        }
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
        })
    }

    pub(super) async fn latest_key_bundle(&self) -> Result<Option<KeyBundlePin>, TransportError> {
        let reply = self
            .call("GET", "/api/v1/accounts/me/key_bundle", None)
            .await?;
        // No pin is `404 BLOB_NOT_FOUND`. Any other 404 is a relay that
        // predates the route, and a publish to it would upload a blob that
        // nothing can ever pin.
        if reply.status.as_u16() == 404 {
            let code = serde_json::from_slice::<serde_json::Value>(&reply.bytes)
                .ok()
                .and_then(|v| v.get("code").and_then(|c| c.as_str()).map(str::to_owned));
            return match code.as_deref() {
                Some("BLOB_NOT_FOUND") => Ok(None),
                _ => Err(TransportError::Unsupported),
            };
        }
        if !reply.status.is_success() {
            return Err(self.refuse(&reply));
        }
        key_bundle_pin_of(&reply.bytes).map(Some)
    }

    pub(super) async fn pin_key_bundle(
        &self,
        blob_id: &[u8; 16],
        manifest_digest: &[u8; 32],
        replaces: Option<&[u8; 16]>,
    ) -> Result<KeyBundlePinned, TransportError> {
        let reply = self
            .call(
                "PUT",
                "/api/v1/accounts/me/key_bundle",
                Some(serde_json::json!({
                    "blob_id": format!("blb_{}", hex::encode(blob_id)),
                    "manifest_digest": hex::encode(manifest_digest),
                    "replaces": replaces.map(|r| format!("blb_{}", hex::encode(r))),
                })),
            )
            .await?;
        // `409 KEY_BUNDLE_SUPERSEDED`: another device pinned since this one
        // read. Not a failure; the caller reads the pin again.
        if reply.status.as_u16() == 409 {
            return Ok(KeyBundlePinned::Superseded);
        }
        if !reply.status.is_success() {
            return Err(self.refuse(&reply));
        }
        Ok(KeyBundlePinned::Pinned)
    }

    pub(super) async fn latest_identity_chain(&self) -> Result<Option<Vec<u8>>, TransportError> {
        let reply = self
            .call("GET", "/api/v1/accounts/me/identity_chain", None)
            .await?;
        // No record is `404 IDENTITY_CHAIN_NOT_FOUND`. Any other 404 is a
        // relay that predates the route, as for the key bundle.
        if reply.status.as_u16() == 404 {
            return match problem_code(&reply.bytes).as_deref() {
                Some("IDENTITY_CHAIN_NOT_FOUND") => Ok(None),
                _ => Err(TransportError::Unsupported),
            };
        }
        if !reply.status.is_success() {
            return Err(self.refuse(&reply));
        }
        let body: serde_json::Value =
            serde_json::from_slice(&reply.bytes).map_err(|e| protocol(&e))?;
        body.get("chain")
            .and_then(|c| c.as_str())
            .and_then(|c| {
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(c)
                    .ok()
            })
            .map(Some)
            .ok_or_else(|| TransportError::Protocol("an identity chain is malformed".to_owned()))
    }

    pub(super) async fn pin_identity_chain(
        &self,
        chain: &[u8],
        replaces: Option<&[u8; 32]>,
    ) -> Result<IdentityChainPinned, TransportError> {
        let reply = self
            .call(
                "PUT",
                "/api/v1/accounts/me/identity_chain",
                Some(serde_json::json!({
                    "chain": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(chain),
                    "replaces": replaces.map(hex::encode),
                })),
            )
            .await?;
        // `409 IDENTITY_CHAIN_SUPERSEDED`: another device pinned since this
        // one read. The caller reads again.
        if reply.status.as_u16() == 409 {
            return Ok(IdentityChainPinned::Superseded);
        }
        if !reply.status.is_success() {
            return Err(self.refuse(&reply));
        }
        Ok(IdentityChainPinned::Pinned)
    }

    pub(super) async fn replace_recovery_blob(&self, blob: &[u8]) -> Result<(), TransportError> {
        let reply = self
            .call(
                "PUT",
                "/api/v1/accounts/me/recovery_blob",
                Some(serde_json::json!({
                    "recovery_blob": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(blob),
                })),
            )
            .await?;
        if reply.status.as_u16() == 404 || reply.status.as_u16() == 405 {
            return Err(TransportError::Unsupported);
        }
        if !reply.status.is_success() {
            return Err(self.refuse(&reply));
        }
        Ok(())
    }
}

/// The `code` member of a problem document, if the body is one.
fn problem_code(bytes: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| v.get("code").and_then(|c| c.as_str()).map(str::to_owned))
}

/// A `KeyBundlePin` document's blob id and manifest digest.
fn key_bundle_pin_of(bytes: &[u8]) -> Result<KeyBundlePin, TransportError> {
    let body: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| protocol(&e))?;
    let malformed = || TransportError::Protocol("a key bundle pin is malformed".to_owned());
    let blob_id = body
        .get("blob_id")
        .and_then(|b| b.as_str())
        .and_then(|s| s.strip_prefix("blb_"))
        .and_then(|hexpart| {
            let mut out = [0u8; 16];
            hex::decode_to_slice(hexpart, &mut out).ok().map(|()| out)
        })
        .ok_or_else(malformed)?;
    let manifest_digest = body
        .get("manifest_digest")
        .and_then(|b| b.as_str())
        .and_then(|hexpart| {
            let mut out = [0u8; 32];
            hex::decode_to_slice(hexpart, &mut out).ok().map(|()| out)
        })
        .ok_or_else(malformed)?;
    Ok(KeyBundlePin {
        blob_id,
        manifest_digest,
    })
}
