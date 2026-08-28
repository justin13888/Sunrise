//! Capability bitfield.
//!
//! Per `docs/10-cross-cutting/protocol-versioning.md` §5: 64-bit field;
//! bits 0..32 are server-side, bits 32..64 are client-side.

/// Wrapped 64-bit capability bitfield.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CapabilityBits(pub u64);

impl CapabilityBits {
    /// All zero.
    pub const EMPTY: Self = Self(0);

    /// Set a capability.
    #[must_use]
    pub const fn with(self, c: Capability) -> Self {
        Self(self.0 | (1u64 << c.bit()))
    }

    /// Whether the bit for `c` is set.
    #[must_use]
    pub const fn has(self, c: Capability) -> bool {
        self.0 & (1u64 << c.bit()) != 0
    }

    /// Bitwise AND.
    #[must_use]
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

/// Named capability bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    // --- server bits (0..32) ---
    /// `0` Server can deliver APNs pushes.
    SrvPushApns,
    /// `1` Server can deliver FCM pushes.
    SrvPushFcm,
    /// `2` Server can deliver Web Push.
    SrvPushWeb,
    /// `3` Server signs presigned URLs for blobs (REQUIRED).
    SrvBlobPresign,
    /// `4` Server forwards Noise-XX pairing transport.
    SrvRelayPair,
    /// `5` Server provides Google Calendar OAuth proxy.
    SrvIntegrationGcal,
    /// `6` Server enforces Stripe-backed quotas.
    SrvBillingStripe,
    /// `7` Server accepts opt-in diagnostic bundles.
    SrvDiagnosticUpload,
    /// `8` Server accepts `0x12 RefreshToken` in a live session and answers
    /// with `0x13 RefreshTokenAck`.
    ///
    /// Optional, and the negotiation is the point. Without it a client cannot
    /// tell a server that accepted its refresh from one too old to know the
    /// frame, because both are silent — and the two call for opposite
    /// behaviour: keep the session, or let it die and reconnect. A client that
    /// does not see this bit agreed simply never sends the frame and renews by
    /// reconnecting, which every server understands.
    SrvTokenRefresh,
    // --- client bits (32..64) ---
    //
    // Bits 32, 33 and 34 originally asserted three Loro CRDT capabilities:
    // `CliLoroLwwRegister`, `CliLoroOrSet` and `CliFractionalIndex`. ADR-0014
    // replaced CRDT merge with entity-level LWW and deleted Loro, so for the
    // whole of v1 these REQUIRED bits asserted that a client implemented three
    // things nothing in the codebase does. A peer setting them was telling the
    // truth about nothing; a peer refusing them was refused for the wrong
    // reason. They are redefined here to what v1 actually requires of a client,
    // and the redefinition is safe precisely because nothing ever shipped that
    // read the old meanings.
    /// `32` Client resolves concurrent writes by entity-level LWW over
    /// `(hlc, device_id, seq)` (REQUIRED). ADR-0014, ADR-0016.
    CliEntityLww,
    /// `33` Client stamps every op with a hybrid logical clock and refuses one
    /// beyond the drift window (REQUIRED). ADR-0016.
    CliHlcTimestamps,
    /// `34` Client preserves and re-emits unknown CBOR map keys (REQUIRED).
    /// protocol-versioning.md §7.
    CliForwardCompat,
    /// `35` Client uses unicode61 + porter FTS5 tokenizer (REQUIRED).
    CliFts5PorterEn,
    /// `36` Client emits presence beacons.
    CliPresenceBeacons,
    /// `37` Client supports diagnostic-mode log uploads.
    CliDiagnosticMode,
}

impl Capability {
    /// Bit index in the 64-bit field.
    #[must_use]
    pub const fn bit(self) -> u32 {
        match self {
            Self::SrvPushApns => 0,
            Self::SrvPushFcm => 1,
            Self::SrvPushWeb => 2,
            Self::SrvBlobPresign => 3,
            Self::SrvRelayPair => 4,
            Self::SrvIntegrationGcal => 5,
            Self::SrvBillingStripe => 6,
            Self::SrvDiagnosticUpload => 7,
            Self::SrvTokenRefresh => 8,
            Self::CliEntityLww => 32,
            Self::CliHlcTimestamps => 33,
            Self::CliForwardCompat => 34,
            Self::CliFts5PorterEn => 35,
            Self::CliPresenceBeacons => 36,
            Self::CliDiagnosticMode => 37,
        }
    }
}

/// Required client bits: 32 (`CliEntityLww`), 33 (`CliHlcTimestamps`),
/// 34 (`CliForwardCompat`), 35 (`CliFts5PorterEn`).
///
/// Each one is a property a peer must actually hold for sync to be correct
/// rather than merely quiet: agree on the merge rule, agree on the ordering
/// key, and do not destroy fields you cannot read.
pub const REQUIRED_CLIENT_BITS: CapabilityBits = CapabilityBits::EMPTY
    .with(Capability::CliEntityLww)
    .with(Capability::CliHlcTimestamps)
    .with(Capability::CliForwardCompat)
    .with(Capability::CliFts5PorterEn);

/// Required server bits: 3 (`SrvBlobPresign`).
pub const REQUIRED_SERVER_BITS: CapabilityBits =
    CapabilityBits::EMPTY.with(Capability::SrvBlobPresign);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_client_bits_set() {
        assert!(REQUIRED_CLIENT_BITS.has(Capability::CliEntityLww));
        assert!(REQUIRED_CLIENT_BITS.has(Capability::CliHlcTimestamps));
        assert!(REQUIRED_CLIENT_BITS.has(Capability::CliForwardCompat));
        assert!(REQUIRED_CLIENT_BITS.has(Capability::CliFts5PorterEn));
    }

    /// The bit POSITIONS are unchanged — only their meanings are. Pinning them
    /// keeps the redefinition from turning into a silent renumbering, which
    /// would be a genuine wire break rather than a documentation fix.
    #[test]
    fn the_redefined_client_bits_keep_their_positions() {
        assert_eq!(Capability::CliEntityLww.bit(), 32);
        assert_eq!(Capability::CliHlcTimestamps.bit(), 33);
        assert_eq!(Capability::CliForwardCompat.bit(), 34);
        assert_eq!(Capability::CliFts5PorterEn.bit(), 35);
    }

    #[test]
    fn required_server_bits_set() {
        assert!(REQUIRED_SERVER_BITS.has(Capability::SrvBlobPresign));
    }

    #[test]
    fn intersection_works() {
        let a = CapabilityBits::EMPTY
            .with(Capability::CliEntityLww)
            .with(Capability::CliHlcTimestamps);
        let b = CapabilityBits::EMPTY.with(Capability::CliEntityLww);
        let i = a.intersection(b);
        assert!(i.has(Capability::CliEntityLww));
        assert!(!i.has(Capability::CliHlcTimestamps));
    }
}
