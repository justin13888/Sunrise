//! How the typed surface spells ids: lowercase hex, and the parsers that hold
//! every route to it.
//!
//! [`parse_hex16`] is the one parser for a 16-byte id carried as 32 lowercase
//! hex characters — a stream id, a device id, and the hex half of an upload or
//! blob id. [`parse_hash`] is its 32-byte twin for a BLAKE3. The newtypes give
//! an id in a URL path or a JSON array element the `pattern` its published
//! schema states, and [`hex_pattern`] builds that schema.
//!
//! # One spelling per id
//!
//! `hex::decode_to_slice` is case-insensitive and would accept `AB…` as the
//! same sixteen bytes as `ab…`. One value with two spellings is a liability in
//! a system that keys directories, cursor maps and content addresses off these
//! ids — `FsBlobs`'s upload directory carries the concrete cost — so the
//! canonical form is the only one admitted: nothing released emits anything
//! else, and every refusal here says "must be lowercase hex".

use crate::api::error::ApiError;
use kynos::openapi::Schema as OpenApiSchema;
use serde::{Deserialize, Serialize};

/// A string schema carrying nothing but `pattern`, for the newtypes below.
///
/// Anonymous — no `name()` — so each one inlines where it is used rather than
/// becoming a `$ref` to a component that is only a regex.
pub(crate) fn hex_pattern(
    registry: &mut kynos::schema::registry::Registry,
    pattern: &str,
) -> OpenApiSchema {
    // `Constraints` is `#[non_exhaustive]`, so it grows without breaking this
    // — which also means starting from `default` and assigning rather than
    // naming every field in a struct expression.
    let mut constraints = kynos::schema::constraints::Constraints::default();
    constraints.pattern = Some(pattern.to_owned());
    constraints.apply(registry.resolve::<String>())
}

/// One entry of a blob finalize request's `chunk_hashes`: a BLAKE3 as 64
/// lowercase hex characters.
///
/// A newtype rather than a `String` because `#[schema(...)]` is a *field*
/// grammar and its keywords land on the field's own schema — which for a
/// `Vec<String>` is the array, where `pattern` is a keyword JSON Schema
/// ignores. Constraining the element needs the element to be a type, which is
/// the remedy `kynos::schema` documents.
///
/// Transparent on the wire in both directions, so the JSON is the array of
/// strings it always was.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChunkHash(pub String);

impl kynos::schema::Schema for ChunkHash {
    fn schema(registry: &mut kynos::schema::registry::Registry) -> OpenApiSchema {
        hex_pattern(registry, "^[0-9a-f]{64}$")
    }
}

/// An upload id in a URL path: `up_` + 32 lowercase hex characters.
///
/// A newtype for the same reason [`ChunkHash`] is one, in a different corner
/// of the derive: `kynos::PathParams` builds each parameter's schema from the
/// field's *type* alone and never reads `#[schema(...)]`, so a constraint on a
/// path parameter has to be something its type states. Without it the one
/// place a client actually puts an id in a URL is the one place the published
/// description says nothing about its shape.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct UploadId(pub String);

impl kynos::schema::Schema for UploadId {
    fn schema(registry: &mut kynos::schema::registry::Registry) -> OpenApiSchema {
        hex_pattern(registry, "^up_[0-9a-f]{32}$")
    }
}

// `PathParams` reads a segment through `FromStr` and writes one through
// `Display`. Both are infallible and shape-blind on purpose: `parse_upload_id`
// is the one place that decides whether a segment is a well-formed id, and a
// second opinion here would be a second error path to keep in step with it.
impl std::str::FromStr for UploadId {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_owned()))
    }
}

impl std::fmt::Display for UploadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Its schema is one string, so one path segment carries it.
impl kynos::schema::ParamValue for UploadId {}

/// A blob id in a URL path: `blb_` + 32 lowercase hex characters. See
/// [`UploadId`] for why a path parameter needs a type to carry its pattern.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct BlobId(pub String);

impl kynos::schema::Schema for BlobId {
    fn schema(registry: &mut kynos::schema::registry::Registry) -> OpenApiSchema {
        hex_pattern(registry, "^blb_[0-9a-f]{32}$")
    }
}

impl std::str::FromStr for BlobId {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_owned()))
    }
}

impl std::fmt::Display for BlobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Its schema is one string, so one path segment carries it.
impl kynos::schema::ParamValue for BlobId {}

/// `up_` + 32 lowercase hex characters, and uppercase is a `400` rather than a
/// second spelling of the same id. Parsing rather than trusting is what keeps a
/// `../` out of a path built from a URL segment.
pub(crate) fn parse_upload_id(s: &str) -> Result<[u8; 16], ApiError> {
    let hexpart = s
        .strip_prefix("up_")
        .ok_or_else(|| ApiError::validation("upload_id must start with up_"))?;
    parse_hex16(hexpart, "upload_id")
}

/// `blb_` + 32 lowercase hex characters.
pub(crate) fn parse_blob_id(s: &str) -> Result<[u8; 16], ApiError> {
    let hexpart = s
        .strip_prefix("blb_")
        .ok_or_else(|| ApiError::validation("blob id must start with blb_"))?;
    parse_hex16(hexpart, "blob id")
}

/// Whether every character is a digit or `a`–`f`.
///
/// `hex::decode_to_slice` is case-insensitive, so this is the only thing
/// standing between the contract these parsers state and the one they enforce.
fn is_lowercase_hex(s: &str) -> bool {
    s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A 16-byte id from 32 **lowercase** hex characters, refused in the words
/// `field` names it by.
pub(crate) fn parse_hex16(s: &str, field: &str) -> Result<[u8; 16], ApiError> {
    let mut out = [0u8; 16];
    if s.len() != 32 {
        return Err(ApiError::validation(format!(
            "{field} must carry 32 hex characters"
        )));
    }
    if !is_lowercase_hex(s) {
        return Err(ApiError::validation(format!(
            "{field} must be lowercase hex"
        )));
    }
    hex::decode_to_slice(s, &mut out)
        .map_err(|_| ApiError::validation(format!("{field} must be lowercase hex")))?;
    Ok(out)
}

/// A BLAKE3 from 64 **lowercase** hex characters.
pub(crate) fn parse_hash(s: &str, field: &'static str) -> Result<[u8; 32], ApiError> {
    let mut out = [0u8; 32];
    if s.len() != 64 {
        return Err(ApiError::validation(format!(
            "{field} entries must be 64 hex characters"
        )));
    }
    if !is_lowercase_hex(s) {
        return Err(ApiError::validation(format!(
            "{field} must be lowercase hex"
        )));
    }
    hex::decode_to_slice(s, &mut out)
        .map_err(|_| ApiError::validation(format!("{field} must be lowercase hex")))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{parse_hash, parse_hex16};

    /// The parsers take lowercase hex and nothing else.
    ///
    /// `hex::decode_to_slice` is case-insensitive, so for a long time an
    /// uppercase `stream_id`, `device_id` or `upload_id` decoded to the same
    /// sixteen bytes and was accepted while the `400` body said "must be
    /// lowercase hex". The message was kept — and deliberately not loosened
    /// to "must be hex" — so that closing the gap needed no user-visible
    /// change to the contract. Each refuses in the words its `400` body has
    /// always used, so the message and the rule are the same statement.
    #[test]
    fn the_id_parsers_take_lowercase_hex_and_nothing_else() {
        for field in ["stream_id", "device_id", "upload_id"] {
            parse_hex16(&"ab".repeat(16), field).expect("lowercase is the canonical form");
            for id in ["AB".repeat(16), format!("{}AB", "ab".repeat(15))] {
                let err = parse_hex16(&id, field).expect_err("uppercase must not decode");
                assert_eq!(
                    err.to_string(),
                    format!("{field} must be lowercase hex"),
                    "for {id}"
                );
            }
        }

        parse_hash(&"ab".repeat(32), "content_hash").expect("lowercase is the canonical form");
        for hash in ["AB".repeat(32), format!("{}AB", "ab".repeat(31))] {
            let err = parse_hash(&hash, "content_hash").expect_err("uppercase must not decode");
            assert_eq!(
                err.to_string(),
                "content_hash must be lowercase hex",
                "for {hash}"
            );
        }
    }
}
