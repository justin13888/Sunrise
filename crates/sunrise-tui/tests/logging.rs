//! The TUI's log destination.
//!
//! `sunrise-tui` owns the alternate screen, so a record written to stdout or
//! stderr lands in the middle of the user's board and Ratatui's diffing
//! renderer never paints over it. The destination is therefore a file, and
//! this asserts that the file destination genuinely works end to end — an
//! `init` that silently no-ops would look identical from inside the app.

use std::path::PathBuf;

use sunrise_log::{build_subscriber, LogConfig, LogFormat, LogTarget};

fn temp_log(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sunrise-tui-log-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join("sunrise-tui.ndjson")
}

fn file_subscriber(path: &std::path::Path) -> tracing::Dispatch {
    build_subscriber(LogConfig {
        target: LogTarget::File(path.to_path_buf()),
        filter: "debug".to_string(),
        format: LogFormat::Ndjson,
    })
    .expect("file subscriber builds")
}

#[test]
fn records_reach_the_file_and_parse_as_ndjson() {
    let path = temp_log("roundtrip");
    tracing::dispatcher::with_default(&file_subscriber(&path), || {
        tracing::info!(
            target: "sunrise_tui",
            ev = "ui.start",
            app_v = "0.1.0",
            wire_v = 1u64,
            "sunrise-tui starting"
        );
        tracing::warn!(
            target: "sunrise_tui",
            ev = "ui.keymap.invalid",
            n_ops = 2u64,
            result = "skipped",
            "keys.toml entries ignored"
        );
    });

    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("log file {} not written: {e}", path.display()));
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 2, "{text}");

    let first: serde_json::Value = serde_json::from_str(lines[0]).expect("NDJSON");
    assert_eq!(first["ev"], "ui.start");
    assert_eq!(first["level"], "INFO");
    assert_eq!(first["app_v"], "0.1.0");
    assert!(
        first["timestamp"]
            .as_str()
            .is_some_and(|t| t.ends_with('Z')),
        "{first}"
    );

    let second: serde_json::Value = serde_json::from_str(lines[1]).expect("NDJSON");
    assert_eq!(second["ev"], "ui.keymap.invalid");
    assert_eq!(second["level"], "WARN");
    assert_eq!(second["n_ops"], 2);
}

#[test]
fn the_log_directory_is_created_on_first_run() {
    // A fresh install has no `~/.local/state/sunrise/log`, and a TUI that
    // refused to log until someone `mkdir`ed it would log nothing, ever.
    let path = temp_log("mkdir");
    assert!(!path.parent().unwrap().exists());
    tracing::dispatcher::with_default(&file_subscriber(&path), || {
        tracing::info!(target: "sunrise_tui", ev = "ui.start", "starting");
    });
    assert!(path.exists(), "{} was not created", path.display());
}

#[test]
fn relay_urls_are_reduced_to_a_host_before_logging() {
    // `livesync` logs `relay`, never the configured URL: a relay URL can carry
    // a query string, and logging.md §6.2 names the host as the sanctioned
    // connection-diagnostic identifier.
    use sunrise_tui::livesync::relay_host;
    let path = temp_log("relay");
    tracing::dispatcher::with_default(&file_subscriber(&path), || {
        tracing::info!(
            target: "sunrise_tui",
            ev = "sync.session.opening",
            relay = %relay_host("wss://user:pw@relay.example:9443/sync?access_token=SENTINEL"),
            result = "ok",
            "sync driver started"
        );
    });
    let text = std::fs::read_to_string(&path).expect("log file");
    assert!(!text.contains("SENTINEL"), "{text}");
    assert!(!text.contains("access_token"), "{text}");
    assert!(!text.contains("pw@"), "{text}");
    assert!(text.contains("\"relay\":\"relay.example:9443\""), "{text}");
}
