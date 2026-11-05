//! Helpers the subcommand modules' unit tests share.

use serde_json::Value;

/// The keys of a JSON object, sorted, so a test can assert that a printed
/// object carries exactly the documented keys.
pub(super) fn keys(v: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = v
        .as_object()
        .unwrap_or_else(|| panic!("not an object: {v}"))
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}
