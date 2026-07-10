//! Asserts that `codes.toml` and the hand-maintained Rust enum stay aligned.
//!
//! Per `docs/10-cross-cutting/error-handling.md`, the TOML manifest is the
//! source of truth and the Rust enum is a generated mirror; until the
//! build-script lands, this test catches drift.

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
