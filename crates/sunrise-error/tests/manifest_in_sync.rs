//! Asserts that `codes.toml` and the hand-maintained Rust enum stay aligned.
//!
//! Per `docs/10-cross-cutting/error-handling.md`, the TOML manifest is the
//! source of truth and the Rust enum is a hand-kept mirror of it; this test
//! is what catches drift between the two.

use sunrise_error::ErrorCode;

#[test]
fn rust_enum_lists_every_toml_name() {
    let toml_text = include_str!("../codes.toml");
    let names = extract_names(toml_text);
    let rust_names: std::collections::HashSet<&'static str> =
        ErrorCode::all().iter().map(|c| c.as_str()).collect();
    for n in &names {
        assert!(
            rust_names.contains(n.as_str()),
            "name {n:?} from codes.toml missing from Rust enum"
        );
    }
}

#[test]
fn toml_lists_every_rust_name() {
    let toml_text = include_str!("../codes.toml");
    let names = extract_names(toml_text);
    let toml_names: std::collections::HashSet<String> = names.into_iter().collect();
    for code in ErrorCode::all() {
        assert!(
            toml_names.contains(code.as_str()),
            "Rust ErrorCode::{code:?} ({}) missing from codes.toml",
            code.as_str()
        );
    }
}

#[test]
fn ids_are_monotonic_and_unique() {
    let toml_text = include_str!("../codes.toml");
    let mut ids: Vec<u32> = Vec::new();
    for line in toml_text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("id = ") {
            let id: u32 = rest.trim().parse().expect("id is u32");
            ids.push(id);
        }
    }
    let pre = ids.len();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(pre, sorted.len(), "duplicate ids in codes.toml");
    assert_eq!(
        ids, sorted,
        "ids in codes.toml are not monotonically increasing"
    );
}

#[test]
fn kind_and_retryable_match_the_toml() {
    let blocks = extract_blocks(include_str!("../codes.toml"));
    assert_eq!(
        blocks.len(),
        ErrorCode::all().len(),
        "codes.toml and the Rust enum list a different number of codes"
    );
    for block in &blocks {
        let code = ErrorCode::from_wire_str(&block.name)
            .unwrap_or_else(|| panic!("name {:?} from codes.toml missing", block.name));
        // The toml's `kind` is compared through `ErrorKind`'s serde spelling,
        // so this also holds that spelling to the manifest's.
        let rust_kind = serde_json::to_string(&code.default_kind()).expect("kind serializes");
        assert_eq!(
            rust_kind,
            format!("\"{}\"", block.kind),
            "kind of {} differs between codes.toml and default_kind()",
            block.name
        );
        assert_eq!(
            code.retryable(),
            block.retryable,
            "retryable of {} differs between codes.toml and retryable()",
            block.name
        );
    }
}

#[test]
fn as_str_matches_the_serde_spelling() {
    for code in ErrorCode::all() {
        let wire = serde_json::to_string(&code).expect("code serializes");
        assert_eq!(
            wire,
            format!("\"{}\"", code.as_str()),
            "ErrorCode::{code:?} serializes differently from its as_str()"
        );
        let parsed: ErrorCode = serde_json::from_str(&wire).expect("code deserializes");
        assert_eq!(parsed, code, "{wire} does not deserialize back to itself");
    }
}

/// The fields of one `[[code]]` block that the Rust accessors mirror.
struct ManifestCode {
    name: String,
    kind: String,
    retryable: bool,
}

/// Parses each `[[code]]` block, panicking on one that lacks a mirrored field
/// so a malformed block cannot pass by being skipped.
fn extract_blocks(toml: &str) -> Vec<ManifestCode> {
    let mut out = Vec::new();
    for block in toml.split("[[code]]").skip(1) {
        let field = |key: &str| {
            let prefix = format!("{key} = ");
            block
                .lines()
                .find_map(|line| line.trim().strip_prefix(prefix.as_str()))
                .map_or_else(
                    || panic!("a [[code]] block has no {key}: {block}"),
                    str::trim,
                )
        };
        out.push(ManifestCode {
            name: field("name").trim_matches('"').to_string(),
            kind: field("kind").trim_matches('"').to_string(),
            retryable: field("retryable").parse().expect("retryable is a bool"),
        });
    }
    out
}

/// Extracts the `name = "..."` value from each `[[code]]` block.
fn extract_names(toml: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in toml.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("name = ") {
            let v = rest.trim().trim_matches('"');
            out.push(v.to_string());
        }
    }
    out
}
