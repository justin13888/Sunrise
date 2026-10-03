//! No metric label can identify anyone, and no label's value set can grow
//! with traffic.
//!
//! `docs/06-server/metrics.md` §Label allowlist is the contract and this is
//! the gate it names. It drives every operation in the published description
//! through the public assembly — [`sunrise_server::build_service`], the one an
//! operator's deployment runs — with id-shaped sentinels in every path
//! parameter, then scrapes `/metrics` and holds three properties:
//!
//! 1. **Only allowlisted label names appear.** The registry refuses any other
//!    name at the call, so this is the scrape-side half of a rule the type
//!    already enforces.
//! 2. **Every label's values come from a closed set.** `endpoint` is a path
//!    template from the description, never a request path; `status` is a
//!    status code; the enum-valued labels take the values the catalogue lists.
//!    No value carries a sentinel, so no request input reached a label.
//! 3. **The series count does not grow with distinct inputs.** A second pass
//!    with *different* sentinels must leave the set of series unchanged: an
//!    id-valued label would add one series per id, which is exactly the
//!    unbounded cardinality the allowlist exists to prevent.
//!
//! Driving every route rather than a chosen few is the point. A route added
//! later is in the description, so it is driven here without anyone
//! remembering to add it.

use http_body_util::BodyExt as _;
use kynos::http::body::Body;
use kynos::http::{HeaderName, HeaderValue, Method, Request, StatusCode};
use kynos::router::service::Service;
use std::collections::{BTreeMap, BTreeSet};
use sunrise_server::{ServerConfig, ServerState};

/// The allowlist as `metrics.md` states it. Kept here as well as in the crate
/// so that widening the registry's list without the document fails a test.
const ALLOWLIST: &[&str] = &[
    "endpoint",
    "method",
    "status",
    "kind",
    "provider",
    "result",
    "reason",
    "scope",
    "direction",
    "state",
    "version",
    "commit",
    "wire_proto",
    "crypto_suite",
];

/// Drive one request and collect its whole body.
async fn send(service: &Service<ServerState>, method: Method, target: &str) -> StatusCode {
    let has_body = matches!(method, Method::POST | Method::PUT | Method::PATCH);
    let mut request = Request::new(if has_body {
        Body::from_bytes(b"{}".to_vec().into())
    } else {
        Body::empty()
    });
    *request.method_mut() = method;
    *request.uri_mut() = target.parse().expect("a well-formed target");
    let headers = request.headers_mut();
    headers.insert(
        HeaderName::from_static("authorization"),
        HeaderValue::from_static("Bearer label-safety"),
    );
    if has_body {
        headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
    }
    let response = service.call(request).await;
    let status = response.status();
    let _ = response.into_body().collect().await;
    status
}

/// Every `(method, path template)` the description publishes.
fn operations() -> Vec<(Method, String)> {
    let doc = sunrise_server::api::document().expect("the router describes");
    let doc: serde_json::Value =
        serde_json::from_str(&doc.to_json().expect("serializes")).expect("is JSON");
    let mut out = Vec::new();
    for (path, item) in doc["paths"].as_object().expect("a paths object") {
        for verb in item.as_object().expect("a path item").keys() {
            let method = match verb.as_str() {
                "get" => Method::GET,
                "post" => Method::POST,
                "put" => Method::PUT,
                "delete" => Method::DELETE,
                "patch" => Method::PATCH,
                _ => continue,
            };
            out.push((method, path.clone()));
        }
    }
    assert!(
        out.len() > 10,
        "the description lists too few operations: {out:?}"
    );
    out
}

/// `template` with every `{param}` replaced by `sentinel`.
fn fill(template: &str, sentinel: &str) -> String {
    let mut out = String::new();
    let mut in_param = false;
    for ch in template.chars() {
        match ch {
            '{' => {
                in_param = true;
                out.push_str(sentinel);
            }
            '}' => in_param = false,
            _ if in_param => {}
            c => out.push(c),
        }
    }
    out
}

/// Drive every operation once, plus a path nothing matches, with `sentinel`
/// in every parameter and in the query string.
async fn drive_everything(service: &Service<ServerState>, sentinel: &str) {
    for (method, template) in operations() {
        // The SSE stream is long-lived when it succeeds. Without a session it
        // refuses, which is still a recorded request.
        let target = format!("{}?probe={sentinel}", fill(&template, sentinel));
        send(service, method, &target).await;
    }
    send(service, Method::GET, &format!("/api/v1/nothing/{sentinel}")).await;
}

/// One sample line's metric name and labels.
fn parse_sample(line: &str) -> (String, Vec<(String, String)>) {
    let (head, _value) = line.rsplit_once(' ').expect("a sample has a value");
    let Some((name, rest)) = head.split_once('{') else {
        return (head.to_owned(), Vec::new());
    };
    let body = rest.strip_suffix('}').expect("a closed label set");
    let mut labels = Vec::new();
    let mut chars = body.chars().peekable();
    while chars.peek().is_some() {
        let key: String = chars.by_ref().take_while(|&c| c != '=').collect();
        assert_eq!(chars.next(), Some('"'), "a quoted value in {line}");
        let mut value = String::new();
        while let Some(c) = chars.next() {
            match c {
                '\\' => value.push(chars.next().expect("an escape")),
                '"' => break,
                c => value.push(c),
            }
        }
        if chars.peek() == Some(&',') {
            chars.next();
        }
        labels.push((key, value));
    }
    (name.to_owned(), labels)
}

/// Every series in a scrape, and every label's value set.
struct Scrape {
    series: BTreeSet<String>,
    values: BTreeMap<String, BTreeSet<String>>,
    families: BTreeSet<String>,
    typed: BTreeSet<String>,
}

async fn scrape(service: &Service<ServerState>) -> (Scrape, String) {
    let mut request = Request::new(Body::empty());
    *request.uri_mut() = "/metrics".parse().expect("a target");
    let response = service.call(request).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "/metrics must be served on loopback"
    );
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collects")
        .to_bytes();
    let text = String::from_utf8(bytes.to_vec()).expect("UTF-8 exposition");

    let mut out = Scrape {
        series: BTreeSet::new(),
        values: BTreeMap::new(),
        families: BTreeSet::new(),
        typed: BTreeSet::new(),
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            out.typed
                .insert(rest.split(' ').next().expect("a name").to_owned());
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, labels) = parse_sample(line);
        let family = ["_bucket", "_sum", "_count"]
            .iter()
            .find_map(|s| name.strip_suffix(s))
            .filter(|f| text.contains(&format!("# TYPE {f} histogram")))
            .unwrap_or(&name)
            .to_owned();
        // `le` is the exposition format's own bucket bound, written by the
        // renderer on a histogram's `_bucket` lines and nowhere else; its
        // values are the fixed bucket set, not anything a call site chose.
        let is_bucket = family != name && name.ends_with("_bucket");
        out.families.insert(family);
        out.series
            .insert(line.rsplit_once(' ').expect("value").0.to_owned());
        for (k, v) in labels {
            if is_bucket && k == "le" {
                continue;
            }
            out.values.entry(k).or_default().insert(v);
        }
    }
    (out, text)
}

/// Whether `value` looks like an identifier rather than a closed-set member:
/// a run of 16 or more letters and digits, the shape of every id this server
/// issues (26-char Crockford, 32-char hex) and of the sentinels below.
fn id_shaped(value: &str) -> bool {
    let mut run = 0;
    for c in value.chars() {
        run = if c.is_ascii_alphanumeric() {
            run + 1
        } else {
            0
        };
        if run >= 16 {
            return true;
        }
    }
    false
}

#[tokio::test]
async fn metric_labels_are_allowlisted_and_bounded() {
    assert_eq!(
        ALLOWLIST,
        sunrise_server::metrics::LABEL_ALLOWLIST,
        "the registry's allowlist and metrics.md's must be the same list"
    );

    let state = ServerState::new(ServerConfig {
        bind: "127.0.0.1:0".to_owned(),
        ..ServerConfig::default()
    });
    let service = sunrise_server::build_service(state).expect("the surface builds");

    let first = "01J8ZQ7X9K3M5N7P9R1T3V5W7Y";
    drive_everything(&service, first).await;
    let (before, text) = scrape(&service).await;

    // 1. Names.
    for name in before.values.keys() {
        assert!(
            ALLOWLIST.contains(&name.as_str()),
            "label `{name}` is not on the allowlist:\n{text}"
        );
    }
    for family in &before.families {
        assert!(
            before.typed.contains(family),
            "`{family}` has no # TYPE line:\n{text}"
        );
    }

    // 2. Values.
    let templates: BTreeSet<String> = operations().into_iter().map(|(_, p)| p).collect();
    for (label, values) in &before.values {
        for value in values {
            assert!(
                !value.contains(first) && !id_shaped(value),
                "label `{label}` carries an id-shaped value `{value}`:\n{text}"
            );
            let closed = match label.as_str() {
                "endpoint" => templates.contains(value) || value == "unmatched",
                "method" => {
                    ["GET", "POST", "PUT", "DELETE", "PATCH", "-"].contains(&value.as_str())
                }
                "status" => value.len() == 3 && value.bytes().all(|b| b.is_ascii_digit()),
                "provider" => ["apns", "fcm", "web"].contains(&value.as_str()),
                "result" => ["ok", "failed", "rejected", "rate_limited", "timeout"]
                    .contains(&value.as_str()),
                "direction" => ["upload", "download"].contains(&value.as_str()),
                "scope" => ["ip", "account", "device"].contains(&value.as_str()),
                "state" => ["active", "revoked"].contains(&value.as_str()),
                "version" => value == env!("CARGO_PKG_VERSION"),
                // A typed error code, or a per-metric closed enum.
                "reason" => value
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_lowercase() || b == b'_'),
                _ => values.len() == 1,
            };
            assert!(
                closed,
                "label `{label}` value `{value}` is outside its closed set:\n{text}"
            );
        }
    }
    assert!(
        before
            .values
            .get("endpoint")
            .is_some_and(|v| v.contains("unmatched")),
        "a path nothing matches must be recorded under `unmatched`:\n{text}"
    );

    // 3. Growth. A different id of the same shape, so every request takes the
    // path it took the first time and only the id differs.
    drive_everything(&service, "01J8ZQ7X9K3M5N7P9R1T3V5W80").await;
    let (after, text) = scrape(&service).await;
    let grown: Vec<&String> = after.series.difference(&before.series).collect();
    assert!(
        grown.is_empty(),
        "new inputs created new series, so some label is open: {grown:?}\n{text}"
    );
}
