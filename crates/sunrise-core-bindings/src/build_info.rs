//! What core this app embeds: the core train version and the protocol surfaces.
//!
//! ADR-0064 splits Sunrise into two version trains. The app's own
//! `MARKETING_VERSION` is the **app** train; the Rust code linked into it is
//! the **core** train, whose single source of truth is `[workspace.package]
//! version` in the root `Cargo.toml`. The two move independently, so an About
//! box or a diagnostics bundle that printed only the app version would not say
//! which core a bug report came from.
//!
//! Neither number carries compatibility meaning (ADR-0042 §1, unchanged by
//! ADR-0064). Compatibility is carried by the versioned surfaces, so they are
//! reported beside the core version rather than left for a reader to look up
//! from it. A client MUST NOT branch on any field here; it is for display and
//! for diagnostics.

use sunrise_cbor::version::{
    CRYPTO_SUITE_V, DOC_SCHEMA_V, ENVELOPE_FORMAT_V, STORAGE_V, WIRE_PROTO_V,
};

/// The core train version this build embeds, and the protocol surface
/// versions it speaks.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CoreBuildInfo {
    /// The core train version, `CARGO_PKG_VERSION` of this crate, which every
    /// workspace crate inherits from `[workspace.package]`.
    pub core_version: String,
    /// `WIRE_PROTO_V`: frame layout, message kinds and error codes.
    pub wire_proto_v: u16,
    /// `ENVELOPE_FORMAT_V`: the envelope container format this build writes.
    pub envelope_format_v: u16,
    /// `DOC_SCHEMA_V`: the document schema this build writes.
    pub doc_schema_v: u16,
    /// `CRYPTO_SUITE_V`: the cryptographic suite.
    pub crypto_suite_v: u16,
    /// `STORAGE_V`: the local vault's on-disk schema.
    pub storage_v: u16,
}

/// The core version and protocol surfaces this build embeds.
///
/// A plain function rather than a method on the core handle: it describes the
/// compiled library, so it is answerable before any vault is open, which is
/// when an About box or a failed-open diagnostic needs it.
#[uniffi::export]
#[must_use]
pub fn core_build_info() -> CoreBuildInfo {
    CoreBuildInfo {
        core_version: env!("CARGO_PKG_VERSION").to_owned(),
        wire_proto_v: WIRE_PROTO_V,
        envelope_format_v: ENVELOPE_FORMAT_V,
        doc_schema_v: DOC_SCHEMA_V,
        crypto_suite_v: CRYPTO_SUITE_V,
        storage_v: STORAGE_V,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each field is the constant it names, read from where the protocol
    /// defines it, so a bump there reaches the app without an edit here.
    #[test]
    fn every_field_is_the_constant_it_names() {
        let info = core_build_info();
        assert_eq!(info.wire_proto_v, WIRE_PROTO_V);
        assert_eq!(info.envelope_format_v, ENVELOPE_FORMAT_V);
        assert_eq!(info.doc_schema_v, DOC_SCHEMA_V);
        assert_eq!(info.crypto_suite_v, CRYPTO_SUITE_V);
        assert_eq!(info.storage_v, STORAGE_V);
    }

    /// The core version is the workspace's, read out of the root manifest
    /// rather than restated, so this does not need editing on every bump.
    #[test]
    fn the_core_version_is_the_workspace_version() {
        let manifest = include_str!("../../../Cargo.toml");
        let workspace_version = manifest
            .split("[workspace.package]")
            .nth(1)
            .and_then(|section| {
                section
                    .lines()
                    .take_while(|line| !line.starts_with('['))
                    .find_map(|line| line.strip_prefix("version = \""))
            })
            .and_then(|rest| rest.strip_suffix('"'))
            .expect("[workspace.package] version in the root Cargo.toml");
        assert_eq!(core_build_info().core_version, workspace_version);
    }
}
