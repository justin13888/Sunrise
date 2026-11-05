//! The derivation's own boundary.
//!
//! The file set comes from `cargo metadata` plus the `--manifest-path` flags in
//! `mise.toml`. The first half is the compiler's own view and cannot drift; the
//! second is a *convention* — that everything this repo builds goes through a
//! mise task. A workflow that reached around mise and ran cargo against a
//! manifest of its own would build a crate no gate here reads, and nothing would
//! say so.
//!
//! Nothing does that today. These check it rather than trust it.

use std::path::Path;

use crate::events::snippet;
use crate::manifests::{
    build_config_files, derived_manifests, is_container_build, manifest_path_args,
};
use crate::source::is_ident_byte;
use crate::workspace_root;

/// Offsets where `cargo` is invoked as a *command*.
///
/// Not a substring search. `~/.cargo/registry`, `id=cargo-registry` and
/// `/usr/local/cargo` are not cargo invocations, and treating them as such
/// meant that adding the conventional Rust cache step would trip a gate whose
/// failure message talks about log vocabulary — a misdiagnosis of exactly the
/// kind this file keeps having to remove.
fn cargo_command_hits(text: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    let mut hits = Vec::new();
    for (at, _) in text.match_indices("cargo") {
        // A whole word, and not glued to a path or a longer token.
        if at > 0 && (is_ident_byte(bytes[at - 1]) || matches!(bytes[at - 1], b'/' | b'.' | b'-')) {
            continue;
        }
        if bytes
            .get(at + "cargo".len())
            .copied()
            .is_some_and(is_ident_byte)
        {
            continue;
        }
        // In command position: the start, a new line, a shell separator, or a
        // YAML `run:`/list dash.
        let mut j = at;
        while j > 0 && matches!(bytes[j - 1], b' ' | b'\t') {
            j -= 1;
        }
        if j == 0
            || matches!(
                bytes[j - 1],
                b'\n' | b'&' | b';' | b'|' | b'(' | b':' | b'-'
            )
        {
            hits.push(at);
        }
    }
    hits
}

/// Whether a build-config fragment invokes cargo.
fn mentions_cargo(text: &str) -> bool {
    !cargo_command_hits(text).is_empty()
}

/// Whether a fragment moves out of the directory it started in.
fn changes_directory(text: &str) -> bool {
    text.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("cd ")
            || t.starts_with("working-directory:")
            || t.starts_with("WORKDIR ")
            || l.contains("&& cd ")
    })
}

/// One build-config file split into the scopes a directory change applies to.
///
/// A YAML step, a mise task, or — where a file has no such structure — the
/// whole file. Splitting on any indentation, because hard-coding six spaces
/// collapsed an eight-space workflow into a single pseudo-step and made the
/// gate report the file's own `name:` line as the offender.
fn scopes(file: &str, text: &str) -> Vec<String> {
    let is_yaml = Path::new(file)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("yml") || e.eq_ignore_ascii_case("yaml"));
    let starts_scope: fn(&str) -> bool = if is_yaml {
        |l: &str| l.trim_start().starts_with("- ")
    } else if file.ends_with("mise.toml") {
        |l: &str| l.starts_with("[tasks.")
    } else {
        return vec![text.to_owned()];
    };
    let mut out = vec![String::new()];
    for line in text.lines() {
        if starts_scope(line) {
            out.push(String::new());
        }
        let last = out.last_mut().expect("one scope always exists");
        last.push_str(line);
        last.push('\n');
    }
    out
}

#[test]
fn no_build_config_names_a_manifest_the_scan_does_not_cover() {
    let root = workspace_root();
    let derived = derived_manifests();
    let mut offenders = Vec::new();
    for (file, text) in build_config_files() {
        for arg in manifest_path_args(&text) {
            // An interpolated path cannot be resolved here, and a manifest this
            // gate cannot name is a manifest it cannot check. Same rule as an
            // `include!`: refuse it and say so.
            if arg.contains("${{") || arg.contains('$') || arg.contains('`') {
                offenders.push(format!(
                    "{file}: `--manifest-path {arg}` is interpolated, so this gate cannot \
                     tell which manifest it resolves to"
                ));
                continue;
            }
            let path = root.join(&arg);
            let path = path.canonicalize().unwrap_or(path);
            if !derived.contains(&path) {
                offenders.push(format!(
                    "{file}: `--manifest-path {arg}` names a manifest the file set does \
                     not include"
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "these build a crate with a manifest these gates do not read, so its log \
         vocabulary is checked by nothing. Route the build through a `mise.toml` \
         task — which is where the out-of-workspace half of the file set is \
         derived from — or make the manifest a workspace member:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn no_build_config_runs_cargo_from_a_directory_of_its_own() {
    // The other way to reach a foreign manifest: leave the workspace root and
    // let cargo find whatever is there. This used to be filtered to workflows,
    // so a mise task, a composite action and a Dockerfile `WORKDIR` each got a
    // free pass from a check written precisely for them.
    //
    // A scope that both moves and runs cargo must say which manifest it means.
    // Only doing *both* is flagged: a `working-directory` on a bun step is
    // ordinary work, and a gate that fires on ordinary work is one someone
    // switches off.
    let mut offenders = Vec::new();
    for (file, text) in build_config_files() {
        // A container build has no directory structure this gate can model —
        // `/src` is a path inside an image. What decides its manifest is which
        // `Cargo.toml` is copied in, which
        // `the_container_build_copies_only_manifests_the_scan_covers` checks.
        if is_container_build(&file) {
            continue;
        }
        for scope in scopes(&file, &text) {
            let cargo = cargo_command_hits(&scope).len();
            if cargo == 0 || !changes_directory(&scope) {
                continue;
            }
            if manifest_path_args(&scope).len() < cargo {
                let name = scope
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .map_or_else(String::new, |l| snippet(l.trim()));
                offenders.push(format!(
                    "{file}: a scope that changes directory and runs cargo without \
                     naming a manifest ({name})"
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "these run cargo somewhere other than the workspace root, so which \
         manifest they build is not something this gate can determine. Give the \
         invocation an explicit `--manifest-path`, or route it through a \
         `mise.toml` task:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn the_container_build_copies_only_manifests_the_scan_covers() {
    // The Dockerfile's cargo build runs against whatever was copied into the
    // image, so `COPY` is the thing that decides its manifest — not `WORKDIR`,
    // and not anything this gate could learn by modelling container paths.
    let root = workspace_root();
    let derived = derived_manifests();
    let containers: Vec<(String, String)> = build_config_files()
        .into_iter()
        .filter(|(f, _)| is_container_build(f))
        .collect();
    // Read from the same set the skip above consumes, so the two rules cannot
    // come to disagree about which files each is responsible for.
    assert!(
        !containers.is_empty(),
        "no container build was read, so the rule that skips them in \
         `no_build_config_runs_cargo_from_a_directory_of_its_own` covers nothing"
    );
    let mut offenders = Vec::new();
    for (file, text) in containers {
        for line in text.lines() {
            let t = line.trim_start();
            if !t.starts_with("COPY ") {
                continue;
            }
            for token in t.split_whitespace().skip(1) {
                if !token.ends_with("Cargo.toml") {
                    continue;
                }
                let path = root.join(token);
                let path = path.canonicalize().unwrap_or(path);
                if !derived.contains(&path) {
                    offenders.push(format!("{file}: `COPY {token}`"));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "the container build copies a manifest these gates do not read, so the \
         crate it builds has its log vocabulary checked by nothing:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn the_build_config_scan_actually_reads_cargo_invocations() {
    // A floor near the real count, not far below it. The workflows, the
    // Dockerfile and mise.toml carry dozens of cargo invocations between them;
    // a scanner that quietly stopped matching would otherwise pass both gates
    // above by finding nothing to check.
    let files = build_config_files();
    let with_cargo = files.iter().filter(|(_, t)| mentions_cargo(t)).count();
    assert!(
        with_cargo >= 2,
        "expected cargo invocations in at least 2 build-config files, found \
         {with_cargo} — the command scan has stopped matching: {:?}",
        files.iter().map(|(f, _)| f).collect::<Vec<_>>()
    );
    // And the one `--manifest-path` the repo actually has must be found, or the
    // argument parser has stopped working and `extra_manifests` with it.
    let found: Vec<String> = files
        .iter()
        .flat_map(|(_, t)| manifest_path_args(t))
        .collect();
    assert!(
        found.iter().any(|p| p.contains("uniffi-bindgen")),
        "the `--manifest-path` parser found {found:?}, which does not include the \
         one manifest mise.toml names — `extra_manifests` reads the same parser, \
         so the file set would be short a crate"
    );
}

#[test]
fn a_commented_out_manifest_path_is_not_read_as_an_invocation() {
    // #147: `mise.toml` carries a long prose comment about why the bindgen
    // manifest is written out literally, and a rewording of it that spelled the
    // flag was enough to fail this gate — the scan could not tell a sentence
    // about the flag from a cargo call using it. The comment had to be written
    // around the word instead, which is the tail wagging the dog.
    let text = "\
# cargo metadata --manifest-path tools/nonexistent/Cargo.toml is what this used
  # to say, indented, and it is still only a comment
cargo build --manifest-path tools/uniffi-bindgen/Cargo.toml
";
    assert_eq!(
        manifest_path_args(text),
        vec!["tools/uniffi-bindgen/Cargo.toml".to_owned()],
        "a `#` comment naming the flag must not be read as an invocation"
    );

    // The converse, so the skip cannot quietly swallow real calls: a trailing
    // `#` after content leaves the line readable.
    assert_eq!(
        manifest_path_args("cargo build --manifest-path a/Cargo.toml # note"),
        vec!["a/Cargo.toml".to_owned()]
    );
}

#[test]
fn a_manifest_path_at_the_end_of_a_toml_array_element_is_read() {
    // #166: the token after the flag is whitespace-delimited, so the `",` that
    // closes a TOML array element rode along with the path and the gate then
    // failed closed on `fuzz/Cargo.toml",` — a manifest that is not there.
    // `mise.toml` worked around it by keeping the flag off the end of every
    // element, under a comment asking the next person not to tidy the order
    // back, which is an argument a formatter eventually wins.
    assert_eq!(
        manifest_path_args("  \"cargo fmt --all --manifest-path fuzz/Cargo.toml\",\n"),
        vec!["fuzz/Cargo.toml".to_owned()],
        "a manifest path at the end of a TOML array element must lose the `\",`"
    );

    // The last element of an array carries no comma and the bracket closes on
    // the same line; a single-quoted TOML string is the same shape again.
    assert_eq!(
        manifest_path_args("run = [\"cargo doc --manifest-path fuzz/Cargo.toml\"]"),
        vec!["fuzz/Cargo.toml".to_owned()]
    );
    assert_eq!(
        manifest_path_args("run = ['cargo doc --manifest-path fuzz/Cargo.toml']"),
        vec!["fuzz/Cargo.toml".to_owned()]
    );

    // And a path that is not at the end is still read, so the fix cannot be a
    // rule that only fires on the last token of a line.
    assert_eq!(
        manifest_path_args("cargo fmt --manifest-path fuzz/Cargo.toml --all -- --check"),
        vec!["fuzz/Cargo.toml".to_owned()]
    );
}
