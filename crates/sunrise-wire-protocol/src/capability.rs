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
    // --- client bits (32..64) ---
    /// `32` Client uses Loro LWW Register for scalar fields (REQUIRED).
    CliLoroLwwRegister,
    /// `33` Client uses Loro OR-Set semantics (REQUIRED).
    CliLoroOrSet,
    /// `34` Client emits fractional-index sort keys (REQUIRED).
    CliFractionalIndex,
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
            Self::CliLoroLwwRegister => 32,
            Self::CliLoroOrSet => 33,
            Self::CliFractionalIndex => 34,
            Self::CliFts5PorterEn => 35,
            Self::CliPresenceBeacons => 36,
            Self::CliDiagnosticMode => 37,
        }
    }
}

/// Required client bits per spec §5: 32 (`CliLoroLwwRegister`),
/// 33 (`CliLoroOrSet`), 34 (`CliFractionalIndex`), 35 (`CliFts5PorterEn`).
pub const REQUIRED_CLIENT_BITS: CapabilityBits = CapabilityBits::EMPTY
    .with(Capability::CliLoroLwwRegister)
    .with(Capability::CliLoroOrSet)
    .with(Capability::CliFractionalIndex)
    .with(Capability::CliFts5PorterEn);

/// Required server bits: 3 (`SrvBlobPresign`).
pub const REQUIRED_SERVER_BITS: CapabilityBits =
    CapabilityBits::EMPTY.with(Capability::SrvBlobPresign);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_client_bits_set() {
        assert!(REQUIRED_CLIENT_BITS.has(Capability::CliLoroLwwRegister));
        assert!(REQUIRED_CLIENT_BITS.has(Capability::CliLoroOrSet));
        assert!(REQUIRED_CLIENT_BITS.has(Capability::CliFractionalIndex));
        assert!(REQUIRED_CLIENT_BITS.has(Capability::CliFts5PorterEn));
    }

    #[test]
    fn required_server_bits_set() {
        assert!(REQUIRED_SERVER_BITS.has(Capability::SrvBlobPresign));
    }

    #[test]
    fn intersection_works() {
        let a = CapabilityBits::EMPTY
            .with(Capability::CliLoroLwwRegister)
            .with(Capability::CliLoroOrSet);
        let b = CapabilityBits::EMPTY.with(Capability::CliLoroLwwRegister);
        let i = a.intersection(b);
        assert!(i.has(Capability::CliLoroLwwRegister));
        assert!(!i.has(Capability::CliLoroOrSet));
    }
}
