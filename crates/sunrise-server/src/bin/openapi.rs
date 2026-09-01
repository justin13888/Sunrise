//! Write the API description to a file.
//!
//! ```text
//! cargo run -p sunrise-server --bin openapi -- schemas/openapi.v1.json
//! ```
//!
//! The description is generated from the operations, so this binary makes no
//! decisions: it exists because `spargen` reads a *file*, and until now the
//! document had no on-disk form at all. Nothing in the repository wrote one,
//! which is why the ADR-0021 client work had no input to point at.
//!
//! Committing the output is what turns the document into something a reviewer
//! sees change. `the_committed_description_is_current` in [`sunrise_server::api`]
//! fails when this has not been re-run, so the file cannot drift from the
//! handlers that produce it.

// The workspace bans `print_stderr` because a server's output is NDJSON an
// ingest pipeline parses, and a stray print corrupts a record. This is not a
// server: it is a codegen tool run from `just`, its audience is a developer
// reading a terminal, and it installs no logger to write through instead.
#![allow(clippy::print_stderr)]

use std::process::ExitCode;

/// Where the description lands when no path is given.
const DEFAULT_PATH: &str = "schemas/openapi.v1.json";

fn main() -> ExitCode {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_PATH.to_owned());

    let document = match sunrise_server::api::document() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("the router cannot be described: {e}");
            return ExitCode::FAILURE;
        }
    };
    let json = match document.to_json() {
        Ok(j) => j,
        Err(e) => {
            eprintln!("the description cannot be serialized: {e}");
            return ExitCode::FAILURE;
        }
    };

    // A trailing newline so the file is a well-formed text file and a diff of
    // it does not report "\ No newline at end of file" on every change.
    if let Err(e) = std::fs::write(&path, format!("{json}\n")) {
        eprintln!("cannot write {path}: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
