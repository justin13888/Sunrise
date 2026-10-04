//! `require_device_sig` is on by default wherever an OIDC issuer is
//! configured, by every public way a `ServerConfig` is built, and no
//! normative document says otherwise (#363).
//!
//! The default is derived in one place, `ServerConfig::device_sig_required`.
//! Before it was, only the TOML overlay applied it: a config built as a value
//! — by an embedder, or a test — left the flag off beside an issuer. The
//! first half of this file holds the value paths to the default; the overlay's
//! half is `an_unset_device_sig_flag_follows_whether_an_issuer_is_configured`
//! in `config::file`.
//!
//! The second half is a gate over the documents. They said "default false"
//! long after the overlay defaulted it on, and the revocation write bound was
//! reasoned from that false default in several of them, so the posture they
//! described was weaker than the one a multi-tenant relay ships. It reads every
//! paragraph under `docs/` that names the key and refuses the phrasings that
//! state an unconditional off default. `docs/11-adr/` is skipped: a decision
//! record states what was true when it was taken, and is amended by a later
//! record rather than rewritten.

use std::path::{Path, PathBuf};

use sunrise_server::ServerConfig;

const ISSUER: &str = "https://idp.example";

/// `Default`, a struct literal and serde all leave the flag unset, and unset
/// resolves on with an issuer and off without one — and both configs still
/// pass `validate` for the verifier each one installs.
#[test]
fn an_unset_flag_is_required_exactly_where_an_issuer_is_configured() {
    let single = ServerConfig::default();
    assert_eq!(single.require_device_sig, None);
    assert!(!single.device_sig_required());
    assert!(single.validate(true).is_ok(), "self-host must still boot");

    let literal = ServerConfig {
        oidc_issuer: Some(ISSUER.into()),
        oidc_client_id: Some("sunrise".into()),
        ..ServerConfig::default()
    };
    assert!(literal.device_sig_required());
    assert!(literal.validate(false).is_ok());

    let deserialized: ServerConfig = serde_json::from_value(serde_json::json!({
        "bind": "127.0.0.1:8443",
        "server_app_v": "0",
        "oidc_issuer": ISSUER,
        "oidc_client_id": "sunrise",
        "sqlite_path": null,
        "blob_root": null,
    }))
    .expect("every other field has a serde default");
    assert_eq!(deserialized.require_device_sig, None);
    assert!(deserialized.device_sig_required());
}

/// An explicit setting wins in both directions, and only an explicit `false`
/// beside an issuer counts as weakening the default — the condition
/// `srv.start.device_sig_optional` is emitted on.
#[test]
fn an_explicit_flag_wins_and_only_turning_it_off_beside_an_issuer_is_flagged() {
    let mut c = ServerConfig {
        oidc_issuer: Some(ISSUER.into()),
        oidc_client_id: Some("sunrise".into()),
        ..ServerConfig::default()
    };
    assert!(
        !c.device_sig_explicitly_optional(),
        "silence is the default"
    );

    c.require_device_sig = Some(false);
    assert!(!c.device_sig_required());
    assert!(c.device_sig_explicitly_optional());

    c.require_device_sig = Some(true);
    assert!(c.device_sig_required());
    assert!(!c.device_sig_explicitly_optional());

    let single = ServerConfig {
        require_device_sig: Some(false),
        ..ServerConfig::default()
    };
    assert!(
        !single.device_sig_explicitly_optional(),
        "off is already the self-host state; restating it weakens nothing"
    );
}

/// Phrasings that claim the key is off by default, after normalisation:
/// lowercase, emphasis and code spans unwrapped, whitespace collapsed.
const FORBIDDEN: &[&str] = &[
    "default false",
    "defaults to false",
    "default of false",
    "default is false",
    "default, false",
    "false, the default",
    "false which is the default",
    "false — which is the default",
    "false - which is the default",
    "is off — which is the default",
    "is off, which is the default",
    "optional by default",
    "defaults to off",
    "off by default",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root resolves")
}

fn markdown_under(dir: &Path, skip: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read docs dir") {
        let path = entry.expect("dir entry").path();
        if path == skip {
            continue;
        }
        if path.is_dir() {
            markdown_under(&path, skip, out);
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
}

fn normalise(paragraph: &str) -> String {
    paragraph
        .to_lowercase()
        .replace(['`', '*'], "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Every offending paragraph in `text`, as `(first line number, phrase)`.
fn offending(text: &str) -> Vec<(usize, &'static str)> {
    let mut found = Vec::new();
    let mut start = 1;
    let mut paragraph = String::new();
    let mut flush = |paragraph: &mut String, start: usize| {
        if paragraph.contains("require_device_sig") {
            let norm = normalise(paragraph);
            if let Some(phrase) = FORBIDDEN.iter().find(|p| norm.contains(*p)) {
                found.push((start, *phrase));
            }
        }
        paragraph.clear();
    };
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            flush(&mut paragraph, start);
            start = i + 2;
        } else {
            paragraph.push_str(line);
            paragraph.push('\n');
        }
    }
    flush(&mut paragraph, start);
    found
}

#[test]
fn no_document_claims_the_device_binding_is_off_by_default() {
    let root = workspace_root();
    let mut files = Vec::new();
    markdown_under(&root.join("docs"), &root.join("docs/11-adr"), &mut files);
    files.sort();
    assert!(!files.is_empty(), "the walk must reach the documents");

    let mut failures = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read document");
        for (line, phrase) in offending(&text) {
            let rel = file.strip_prefix(&root).unwrap_or(file).display();
            failures.push(format!("  {rel}:{line}: \"{phrase}\""));
        }
    }
    assert!(
        failures.is_empty(),
        "these paragraphs name `require_device_sig` and say it is off by default; it is on \
         wherever an OIDC issuer is configured (`ServerConfig::device_sig_required`):\n{}",
        failures.join("\n")
    );
}

/// The gate is not vacuous: each shape the documents actually used is caught,
/// across a line break, and the true statement is not.
#[test]
fn the_phrasings_the_documents_used_are_caught() {
    for bad in [
        "require_device_sig = true                 # default false; requires an issuer",
        "- The binding is **optional by default**. `[auth] require_device_sig` defaults\n  to `false`, so",
        "so with `[auth] require_device_sig` at its default `false` a revoked device",
        "`[auth] require_device_sig` false — which is the default — no device",
        "`require_device_sig` is off — which\n  is the default, and",
    ] {
        assert!(!offending(bad).is_empty(), "missed: {bad}");
    }
    assert!(offending(
        "`[auth] require_device_sig`, left unset, is on wherever an issuer is configured"
    )
    .is_empty());
    assert!(
        offending("optional by default, unrelated to any key").is_empty(),
        "only paragraphs naming the key are read"
    );
}
