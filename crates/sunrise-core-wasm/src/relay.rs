//! The relay URL rule, for the web client: [`sunrise_client_core::relay_url`].
//!
//! The web client checks a relay URL on the page's main thread, synchronously,
//! at start-up — before, and without, the worker this crate runs in — so
//! `apps/web/src/server-url.ts` keeps a TypeScript statement of the rule and
//! `server-url.test.ts` holds it to `crates/sunrise-client-core/vectors/
//! relay-url.json`, the cases the Rust rule is tested against. This export is
//! the rule itself, for any caller that already has the module loaded.

use sunrise_client_core::relay_url::{parse_relay_url, RelayUrlIssue};

/// `{"origin":"https://relay.example"}` for a relay URL, or
/// `{"issue":"insecure_remote"}` — the vectors' own spelling — for one that is
/// not.
#[must_use]
pub fn check_relay_url_json(input: &str) -> String {
    let value = match parse_relay_url(input) {
        Ok(origin) => serde_json::json!({ "origin": origin.as_str() }),
        Err(issue) => serde_json::json!({ "issue": issue_name(issue) }),
    };
    value.to_string()
}

/// The vectors' name for `issue`.
#[must_use]
pub const fn issue_name(issue: RelayUrlIssue) -> &'static str {
    match issue {
        RelayUrlIssue::Empty => "empty",
        RelayUrlIssue::TooLong => "too_long",
        RelayUrlIssue::NotAUrl => "not_a_url",
        RelayUrlIssue::WebSocket => "web_socket",
        RelayUrlIssue::UnsupportedScheme => "unsupported_scheme",
        RelayUrlIssue::InsecureRemote => "insecure_remote",
        RelayUrlIssue::NotAnOrigin => "not_an_origin",
    }
}

/// The browser export.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub mod web {
    use wasm_bindgen::prelude::*;

    /// `checkRelayUrl(input)`: the JSON [`super::check_relay_url_json`] writes.
    #[wasm_bindgen(js_name = checkRelayUrl)]
    #[must_use]
    pub fn check_relay_url(input: &str) -> String {
        super::check_relay_url_json(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every case in the shared vectors, through the JSON the web reads.
    #[test]
    fn the_export_answers_the_shared_vectors_in_their_own_words() {
        let vectors: serde_json::Value = serde_json::from_str(include_str!(
            "../../sunrise-client-core/vectors/relay-url.json"
        ))
        .expect("vectors");
        for case in vectors["cases"].as_array().expect("cases") {
            let input = case["input"]
                .as_str()
                .expect("input")
                .replace("{256}", &"a".repeat(256));
            let got: serde_json::Value =
                serde_json::from_str(&check_relay_url_json(&input)).expect("json");
            match (case.get("origin"), case.get("issue")) {
                (Some(origin), None) => assert_eq!(got["origin"], *origin, "{input}"),
                (None, Some(issue)) => assert_eq!(got["issue"], *issue, "{input}"),
                _ => panic!("{case}"),
            }
        }
    }
}
