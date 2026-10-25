//! The account's identity chain record (`/api/v1/accounts/me/identity_chain`).
//!
//! Per [ADR-0070](../../../../docs/11-adr/0070-a-recovery-code-survives-a-rotation-that-carries-it.md)
//! and `docs/06-server/api.md` §Identity chain. A recovery code opens the
//! identity it was sealed under; after an identity rotation the transitions
//! that lead from it to the identity in force are ops the relay trims. A device
//! that signs under the identity in force pins the chain here, and a
//! recovering device reads it beside the recovery blob, on the stepped-up
//! `GET /accounts/me/recovery_blob`.
//!
//! # What the relay reads
//!
//! The five-byte magic prefix (kind `0x09`), and the size. The record is
//! signed transitions and carry shares HPKE-sealed to identity keys the relay
//! does not hold; it carries no roster, so it names no device. The relay
//! cannot forge a link, and serving an old record or none costs a recovery
//! from an older code its availability, nothing more.
//!
//! # A replacement names what it replaces
//!
//! As with a key bundle pin: `replaces` is the BLAKE3 digest of the record the
//! publisher read, and anything else is `409 IDENTITY_CHAIN_SUPERSEDED`.

use crate::api::error::{codes, ApiError};
use crate::api::key_bundle::DigestHex;
use crate::api::signed::{Signed, SignedParts};
use crate::state::ServerState;
use crate::store::{IdentityChainPinned, PinnedIdentityChain, MAX_CHAIN_BYTES};
use base64::Engine as _;
use kynos::di::inject::Inject;
use kynos::extract::body::json::Json;
use serde::{Deserialize, Serialize};
use sunrise_cbor::magic::{decode_prefix, MagicKind};

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// `PUT /api/v1/accounts/me/identity_chain` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct PinIdentityChainRequest {
    /// The record, base64url no-pad.
    pub chain: String,
    /// The BLAKE3 digest of the record this one replaces, as the publisher
    /// last read it; absent or `null` when it read none. Anything else in
    /// place is `409 IDENTITY_CHAIN_SUPERSEDED`.
    #[serde(default)]
    pub replaces: Option<DigestHex>,
}

/// The account's pinned identity chain record: the body of both operations.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct IdentityChainPin {
    /// The record, base64url no-pad.
    pub chain: String,
    /// Its BLAKE3 digest, which a replacement names.
    pub digest: DigestHex,
    /// When it was pinned, ms since the epoch, by the relay's clock.
    pub published_at_ms: u64,
}

impl IdentityChainPin {
    fn of(pin: &PinnedIdentityChain) -> Self {
        Self {
            chain: B64.encode(&pin.chain),
            digest: DigestHex(hex::encode(pin.digest)),
            published_at_ms: pin.published_at_ms,
        }
    }
}

fn parse_digest(s: &str) -> Result<[u8; 32], ApiError> {
    let mut out = [0u8; 32];
    if s.len() != 64 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(ApiError::validation(
            "replaces must be 64 lowercase hex characters",
        ));
    }
    hex::decode_to_slice(s, &mut out).map_err(|_| ApiError::validation("replaces is not hex"))?;
    Ok(out)
}

/// Pin the account's identity chain record.
///
/// The record must start with the identity chain's magic prefix and fit
/// [`MAX_CHAIN_BYTES`], and `replaces` must name the record in place.
#[kynos::put(
    "/api/v1/accounts/me/identity_chain",
    operation_id = "pinIdentityChain"
)]
pub async fn pin(
    Inject(state): Inject<ServerState>,
    Signed {
        caller,
        value: body,
    }: Signed<PinIdentityChainRequest>,
) -> Result<Json<IdentityChainPin>, ApiError> {
    let chain = B64
        .decode(body.chain.trim())
        .map_err(|_| ApiError::validation("chain is not base64url"))?;
    if chain.len() > MAX_CHAIN_BYTES
        || !matches!(decode_prefix(&chain), Ok(p) if p.kind == MagicKind::IdentityChain)
    {
        return Err(ApiError::validation(
            "chain is not an identity chain record",
        ));
    }
    let replaces = body
        .replaces
        .as_ref()
        .map(|d| parse_digest(&d.0))
        .transpose()?;
    crate::api::blobs::refuse_if_pending(&state, &caller).await?;
    let account_id = &caller.principal.account.account_id;
    let pinned = PinnedIdentityChain {
        digest: *blake3::hash(&chain).as_bytes(),
        chain,
        published_by: caller.device.as_ref().map(|d| d.device_id.clone()),
        published_at_ms: state.clock.now_ms(),
    };
    match state
        .store
        .pin_identity_chain(account_id, &pinned, replaces)
        .await?
    {
        IdentityChainPinned::Pinned => {}
        IdentityChainPinned::Superseded => {
            return Err(ApiError::conflict(
                codes::IDENTITY_CHAIN_SUPERSEDED,
                "another identity chain was pinned since this one was read",
            ))
        }
    }
    tracing::info!(
        ev = "srv.identity_chain.pinned",
        account_h = %crate::logging::account_h(account_id),
        n_bytes = pinned.chain.len() as u64,
        "the account's identity chain is pinned"
    );
    state.metrics.incr("sunrise_identity_chain_pin_total");
    Ok(Json(IdentityChainPin::of(&pinned)))
}

/// The account's pinned identity chain record, or `404` when none is.
#[kynos::get(
    "/api/v1/accounts/me/identity_chain",
    operation_id = "fetchIdentityChain"
)]
pub async fn latest(
    Inject(state): Inject<ServerState>,
    SignedParts(caller): SignedParts,
) -> Result<Json<IdentityChainPin>, ApiError> {
    let account_id = &caller.principal.account.account_id;
    let pin = state
        .store
        .pinned_identity_chain(account_id)
        .await?
        .ok_or_else(|| {
            ApiError::not_found(
                codes::IDENTITY_CHAIN_NOT_FOUND,
                "this account has no identity chain pinned",
            )
        })?;
    Ok(Json(IdentityChainPin::of(&pin)))
}

#[cfg(test)]
mod tests {
    use crate::api::error::codes;
    use crate::api::testing::{Client, SECOND_BEARER};
    use base64::Engine as _;
    use kynos::http::{Method, StatusCode};

    const ROUTE: &str = "/api/v1/accounts/me/identity_chain";

    fn record(tag: u8) -> String {
        let mut r = b"SR\x09\x00\x01".to_vec();
        r.extend(std::iter::repeat_n(tag, 40));
        super::B64.encode(r)
    }

    fn digest(encoded: &str) -> String {
        hex::encode(blake3::hash(&super::B64.decode(encoded).unwrap()).as_bytes())
    }

    /// A record is pinned over the record the publisher read and nothing else,
    /// and it is the account's own.
    #[tokio::test]
    async fn a_chain_replaces_only_the_record_it_read() {
        let (client, _guard) = Client::with_blob_root_and_verifier();
        let res = client.send(Method::GET, ROUTE, None).await;
        res.assert_status(StatusCode::NOT_FOUND);
        assert_eq!(res.json()["code"], codes::IDENTITY_CHAIN_NOT_FOUND);

        let first = record(1);
        let res = client
            .send(
                Method::PUT,
                ROUTE,
                Some(&serde_json::json!({ "chain": first })),
            )
            .await;
        res.assert_status(StatusCode::OK);
        assert_eq!(res.json()["digest"], digest(&first));

        let second = record(2);
        let res = client
            .send(
                Method::PUT,
                ROUTE,
                Some(&serde_json::json!({ "chain": second })),
            )
            .await;
        res.assert_status(StatusCode::CONFLICT);
        assert_eq!(res.json()["code"], codes::IDENTITY_CHAIN_SUPERSEDED);

        let res = client
            .send(
                Method::PUT,
                ROUTE,
                Some(&serde_json::json!({ "chain": second, "replaces": digest(&first) })),
            )
            .await;
        res.assert_status(StatusCode::OK);
        let res = client.send(Method::GET, ROUTE, None).await;
        res.assert_status(StatusCode::OK);
        assert_eq!(res.json()["chain"], second);

        let res = client
            .send_as(Method::GET, ROUTE, Some(SECOND_BEARER), None)
            .await;
        res.assert_status(StatusCode::NOT_FOUND);
    }

    /// Anything that is not an identity chain record is refused, by its magic.
    #[tokio::test]
    async fn only_an_identity_chain_record_is_pinned() {
        let (client, _guard) = Client::with_blob_root_and_verifier();
        let bundle = super::B64.encode(b"SR\x08\x00\x01xxxx");
        let res = client
            .send(
                Method::PUT,
                ROUTE,
                Some(&serde_json::json!({ "chain": bundle })),
            )
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
    }
}
