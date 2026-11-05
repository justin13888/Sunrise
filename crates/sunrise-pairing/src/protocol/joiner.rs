//! [`PairingJoiner`]: the joiner's pending state between message 2 and
//! message 3.

use sunrise_crypto::device_id_from_pub;
use sunrise_crypto::keys::{DeviceDhKeyPair, DeviceSigningKeyPair};
use zeroize::Zeroize;

use super::{decode_pairing_offer, PairingGrant, PairingOffer, PairingRequest};
use crate::codec::{arr32, decode_message, int};
use crate::payload::{PairingPayload, PairingPayloadError};

/// The joiner's half of a pairing, between message 2 and message 3.
///
/// The thing a one-shot protocol did not need and a round trip cannot do
/// without: somewhere for the device keys to live after the request has gone out
/// and before the grant comes back. They cannot be re-minted at message 3 —
/// the cert names the ones that were in the request — and they cannot be sent,
/// which is the whole distinction from the rejected shape where the sponsor
/// mints them.
///
/// Every driver needs this and each would otherwise grow its own: the Apple seam
/// holds one inside `DevicePairing` for the duration of a handshake, and the CLI
/// writes one to a mode-0600 file because its two steps are two processes.
///
/// # Why the seeds are the caller's
///
/// `sunrise-pairing` has no RNG and deliberately keeps none: every other secret
/// in this crate comes from `snow`'s CSPRNG through the Noise session, and a
/// second source of randomness in a crypto crate is a second thing to get wrong.
/// The two 32-byte seeds come from whatever the caller already draws from —
/// `sunrise-core`'s injected `Rng` on both drivers — which also makes a seeded
/// test reproducible end to end.
///
/// **They must be unpredictable.** `D_S_priv` is what every op this device ever
/// writes is signed under.
pub struct PairingJoiner {
    offer: PairingOffer,
    request: PairingRequest,
    d_s_priv: [u8; 32],
    d_d_priv: [u8; 32],
}

impl Zeroize for PairingJoiner {
    fn zeroize(&mut self) {
        self.d_s_priv.zeroize();
        self.d_d_priv.zeroize();
    }
}

impl Drop for PairingJoiner {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl std::fmt::Debug for PairingJoiner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingJoiner")
            .field("device_id", &hex::encode(self.request.device_id))
            .finish_non_exhaustive()
    }
}

impl PairingJoiner {
    /// Mint `D_S`/`D_D` from `seed_s`/`seed_d` and build the request that asks
    /// this offer's account to certify them.
    #[must_use]
    pub fn new(
        offer: PairingOffer,
        nickname: String,
        platform: String,
        seed_s: [u8; 32],
        seed_d: [u8; 32],
    ) -> Self {
        let d_s = DeviceSigningKeyPair::from_secret_bytes(&seed_s);
        let d_d = DeviceDhKeyPair::from_secret_bytes(seed_d);
        let request = PairingRequest {
            device_id: device_id_from_pub(&d_s.public_bytes()),
            d_s_pub: d_s.public_bytes(),
            d_d_pub: d_d.public_bytes(),
            // Echoed from the offer, so the sponsor can refuse a request that
            // answers a different account's.
            identity_id: offer.identity_id,
            nickname,
            platform,
        };
        Self {
            offer,
            request,
            d_s_priv: d_s.secret_bytes(),
            d_d_priv: d_d.secret_bytes(),
        }
    }

    /// Message 2, to hand the sponsor.
    #[must_use]
    pub const fn request(&self) -> &PairingRequest {
        &self.request
    }

    /// The offer this answers.
    #[must_use]
    pub const fn offer(&self) -> &PairingOffer {
        &self.offer
    }

    /// Consume the grant and assemble what `Keychain::create` takes.
    ///
    /// # Errors
    /// [`PairingPayloadError::BadCert`] when the cert does not verify under the
    /// offer's identity or names a different device — see
    /// [`PairingGrant::accept`], which is where the checks are.
    pub fn accept(self, grant: PairingGrant) -> Result<PairingPayload, PairingPayloadError> {
        // Moved out rather than borrowed so `Drop` does not scrub the seeds the
        // payload is about to own.
        let mut me = self;
        let d_s_priv = std::mem::take(&mut me.d_s_priv);
        let d_d_priv = std::mem::take(&mut me.d_d_priv);
        grant.accept(&me.offer, d_s_priv, d_d_priv, &me.request)
    }

    /// Encode the whole pending state: the offer, the request and the two
    /// secrets.
    ///
    /// For a driver whose two steps are two processes. Owner-readable storage is
    /// the caller's problem and it is a real one: **this is `D_S_priv` in the
    /// clear**, and a device is whoever holds it.
    ///
    /// ```cddl
    /// PairingJoiner = {
    ///     1: bstr,            ; encoded PairingOffer
    ///     2: bstr .size 32,   ; D_S_priv
    ///     3: bstr .size 32,   ; D_D_priv
    ///     4: tstr,            ; nickname
    ///     5: tstr,            ; platform
    /// }
    /// ```
    ///
    /// The request is **not** stored: it is entirely a derivation of the other
    /// four, so storing it would be a second copy that could disagree with the
    /// keys it names. [`Self::decode`] rebuilds it through the same constructor
    /// a fresh joiner uses.
    ///
    /// # Errors
    /// [`PairingPayloadError::Cbor`] on an encode failure.
    pub fn encode(&self) -> Result<Vec<u8>, PairingPayloadError> {
        use ciborium::value::Value;
        let map = vec![
            (int(1), Value::Bytes(self.offer.encode()?)),
            (int(2), Value::Bytes(self.d_s_priv.to_vec())),
            (int(3), Value::Bytes(self.d_d_priv.to_vec())),
            (int(4), Value::Text(self.request.nickname.clone())),
            (int(5), Value::Text(self.request.platform.clone())),
        ];
        let mut out = Vec::with_capacity(320);
        ciborium::ser::into_writer(&Value::Map(map), &mut out)
            .map_err(|e| PairingPayloadError::Cbor(e.to_string()))?;
        Ok(out)
    }

    /// Rebuild a [`PairingJoiner`] [`Self::encode`] wrote.
    ///
    /// # Errors
    /// [`PairingPayloadError::Cbor`] or [`PairingPayloadError::BadField`], plus
    /// whatever [`decode_pairing_offer`] refuses in the stored offer.
    pub fn decode(bytes: &[u8]) -> Result<Self, PairingPayloadError> {
        use ciborium::value::Value;
        let Value::Map(map) = decode_message(bytes)? else {
            return Err(PairingPayloadError::Cbor("joiner must be a map".into()));
        };
        let mut offer = None;
        let mut d_s_priv = None;
        let mut d_d_priv = None;
        let mut nickname = None;
        let mut platform = None;
        for (k, v) in map {
            let Value::Integer(i) = k else {
                return Err(PairingPayloadError::Cbor("non-int key".into()));
            };
            match (i128::from(i), v) {
                (1, Value::Bytes(b)) => offer = Some(decode_pairing_offer(&b)?),
                (2, Value::Bytes(b)) => d_s_priv = Some(arr32(&b, "joiner d_s_priv")?),
                (3, Value::Bytes(b)) => d_d_priv = Some(arr32(&b, "joiner d_d_priv")?),
                (4, Value::Text(s)) => nickname = Some(s),
                (5, Value::Text(s)) => platform = Some(s),
                (id, _) if (1..=5).contains(&id) => {
                    return Err(PairingPayloadError::BadField("joiner field shape"));
                }
                _ => {}
            }
        }
        Ok(Self::new(
            offer.ok_or(PairingPayloadError::BadField("joiner offer"))?,
            nickname.ok_or(PairingPayloadError::BadField("joiner nickname"))?,
            platform.ok_or(PairingPayloadError::BadField("joiner platform"))?,
            d_s_priv.ok_or(PairingPayloadError::BadField("joiner d_s_priv"))?,
            d_d_priv.ok_or(PairingPayloadError::BadField("joiner d_d_priv"))?,
        ))
    }
}
