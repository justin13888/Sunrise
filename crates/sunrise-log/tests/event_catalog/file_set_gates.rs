//! The gates over the file set: every manifest it derives exists, every
//! shipped target has current dep-info, every out-of-workspace target is an
//! argued build tool, and the scan reaches every tracked source cargo compiles.

use std::collections::{BTreeMap, BTreeSet};

use crate::events::scan_source;
use crate::file_set::{shipped_targets, sources, tracked_package_sources, BuildTool, BUILD_TOOLS};
use crate::manifests::extra_manifests;
use crate::source::{normalise, word_hits};
use crate::{rel, workspace_root};

#[test]
fn every_manifest_the_build_config_names_exists() {
    // The out-of-workspace half of the file set is derived from `mise.toml`, so
    // a renamed or moved manifest must fail here rather than quietly shrink the
    // set of files these gates read.
    for manifest in extra_manifests() {
        assert!(
            manifest.is_file(),
            "mise.toml names --manifest-path {} but no manifest is there",
            manifest.display()
        );
    }
}

#[test]
fn every_shipped_target_has_current_dep_info() {
    // The file set is a lookup of what the compiler recorded, so a missing or
    // stale `.d` means the gate would silently cover less than the code does.
    // That is the same defect as a walk that could not reach a file, wearing
    // build-system clothes, so it fails rather than shrinking quietly.
    let problems = &sources().1;
    assert!(
        problems.is_empty(),
        "the compiler's own record of what these targets read is missing or out \
         of date, so this gate cannot say which files it covers:\n  {}",
        problems.join("\n  ")
    );
}

#[test]
fn every_out_of_workspace_target_is_a_guarded_build_tool() {
    // `cargo test` never builds a crate outside the workspace, so there is no
    // dep-info for one and no honest way to derive its file set from here.
    // Each is therefore an argued exception with a guard: code that cannot
    // reach `tracing` cannot emit an event.
    let tools: BTreeMap<&str, &BuildTool> = BUILD_TOOLS.iter().map(|t| (t.package, t)).collect();
    let mut unclaimed = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (package, src_path, in_workspace) in shipped_targets() {
        if in_workspace {
            continue;
        }
        let Some(tool) = tools.get(package.as_str()) else {
            unclaimed.push(format!("{package} ({})", rel(&src_path, &workspace_root())));
            continue;
        };
        seen.insert(package.clone());

        // The claim being guarded is about this crate's *source*, so read it.
        //
        // What stood here read the package's declared `dependencies` for
        // `tracing`, which is a proxy for the claim rather than the claim: a
        // `tracing` reached transitively — through `uniffi`, say — satisfies
        // the proxy and refutes the claim. `cargo metadata --no-deps` cannot
        // see a transitive edge at all, and asking for one with deps means a
        // resolve of a manifest outside this workspace.
        //
        // Reading the file answers the question directly and needs nothing
        // resolved: code that does not invoke a `tracing` macro emits no event,
        // whatever it could have linked. The exception holds only while the
        // whole crate is this one file, so a `mod` declaration retires it —
        // there would be sources here that nothing reads, which is the shape
        // this file exists to refuse.
        let source = std::fs::read_to_string(&src_path).unwrap_or_else(|e| {
            panic!(
                "{}: {} is unreadable ({e}), so the guard on this BUILD_TOOLS entry \
                 ran on nothing",
                tool.package,
                rel(&src_path, &workspace_root())
            )
        });
        let normalised = normalise(&source);
        assert!(
            word_hits(&normalised.structural, "mod").is_empty(),
            "{} declares a module, so its target is no longer the single file this \
             entry's guard reads. Either fold it back into one file or make the \
             crate a workspace member so its sources come from dep-info like \
             everything else. ({})",
            tool.package,
            tool.why
        );
        let invocations = scan_source(&source);
        assert!(
            invocations.is_empty() && word_hits(&normalised.structural, "tracing").is_empty(),
            "{} names `tracing` in {}, so it can emit events no gate here has read \
             — and being outside the workspace, there is no dep-info to derive its \
             sources from. Either drop it, or make the crate a workspace member so \
             it is scanned like everything else. ({})",
            tool.package,
            rel(&src_path, &workspace_root()),
            tool.why
        );
    }
    assert!(
        unclaimed.is_empty(),
        "these are built outside the workspace, so this gate has no dep-info for \
         them and does not read a line of their source. If a crate is shipped, \
         make it a workspace member; if it is a build tool, add it to BUILD_TOOLS \
         with the argument for why its logs are nobody's:\n  {}",
        unclaimed.join("\n  ")
    );

    // Without this the guard above is vacuous: an entry whose target has left
    // the metadata — the build config stopped naming its manifest, say — is
    // checked by nothing, and a carve-out kept past the expiry of its own
    // justification is the defect this whole file exists to catch. The same
    // check `ALLOWED_INCLUDES` carried before dep-info made it unnecessary.
    let stale: Vec<&str> = BUILD_TOOLS
        .iter()
        .map(|t| t.package)
        .filter(|p| !seen.contains(*p))
        .collect();
    assert!(
        stale.is_empty(),
        "these BUILD_TOOLS entries name no out-of-workspace target this run \
         found, so their guards ran on nothing. Either the build config stopped \
         naming the manifest — in which case the crate is now unscanned by a \
         different route — or the entry is dead and should be deleted: {stale:?}"
    );
}

/// Sources behind `#[cfg(windows)]`, which no other host compiles. The scan
/// below skips them off Windows, and asserts they call no log macro.
const WINDOWS_ONLY: &[&str] = &["crates/sunrise-private-file/src/windows.rs"];

#[test]
fn the_scan_reaches_what_cargo_compiles() {
    let files = &sources().0;
    // Not a floor. Every `.rs` file git tracks under a workspace package's
    // `src/` has to be in the set, named individually when it is not.
    //
    // The constant this replaces was `>= 50` against a measured 185, so it
    // passed while 72% of the file set disappeared — the exact failure mode
    // #84 describes, sitting inside the test written to detect it. A count
    // cannot tell "someone deleted a module" from "the lookup broke"; a set
    // difference can, because it names what went missing and the author reads
    // the name and knows which it was.
    let missing: Vec<String> = tracked_package_sources()
        .into_iter()
        .filter(|f| !files.contains_key(f))
        .filter(|f| !(cfg!(not(windows)) && WINDOWS_ONLY.contains(&f.as_str())))
        .collect();
    // A file compiled only for Windows has no dep-info on this host, so the
    // scan cannot read it. That is safe only while it logs nothing.
    for path in WINDOWS_ONLY {
        let text = std::fs::read_to_string(workspace_root().join(path))
            .unwrap_or_else(|e| panic!("{path} is named in WINDOWS_ONLY: {e}"));
        for mac in ["trace!", "debug!", "info!", "warn!", "error!", "event!"] {
            assert!(
                !text.contains(mac),
                "{path} is skipped off Windows as logging nothing, but it calls {mac}"
            );
        }
    }
    assert!(
        missing.is_empty(),
        "these files are tracked under a workspace crate's `src/` and no dep-info \
         names them, so nothing in this file reads a line of them. Either the \
         workspace has not been built the way this gate needs — `mise run \
         log-fields`, which is `cargo test --workspace --all-targets --no-run` \
         and then this gate, compiling the feature and cfg combinations an \
         ordinary `cargo build --workspace` does not — or the file is reachable \
         from no module and is compiled by nothing:\n  {}",
        missing.join("\n  ")
    );
    for expected in [
        "crates/sunrise-server/src/api/sync/stream.rs",
        "crates/sunrise-core/src/sync_driver.rs",
        "crates/sunrise-log/src/field.rs",
        // Behind `#[cfg(feature = "sse")]`, off by default. Named here because
        // it is the instance #84 turned on: before the union it was covered by
        // `sunrise-bench`'s uplifted artefact record, which is a fact about a
        // benchmark crate rather than about this gate.
        "crates/sunrise-sync/src/sse/mod.rs",
        // Behind `#[cfg(test)]` at its `mod` declaration, so it is absent from
        // any `cargo build --workspace` record and present in a `cargo test`
        // one. The union holds both.
        "crates/sunrise-server/src/api/testing.rs",
    ] {
        assert!(
            files.contains_key(expected),
            "the dep-info lookup did not reach {expected}"
        );
    }
    // The diagnostic names files, never their contents: `{files:?}` on this map
    // printed the whole codebase — 2.94 MB — which is a gate someone turns off.
    let names: Vec<&String> = files.keys().collect();
    // An integration test is a target of its own and is out of scope. A
    // `src/tests/mod.rs` is not one — it is part of the library, and reading
    // `/tests/` anywhere in the path called that an integration test.
    assert!(
        !names
            .iter()
            .any(|f| f.split('/').nth(2) == Some("tests") && f.starts_with("crates/")),
        "integration tests are separate targets and are not shipped code: {names:?}"
    );
}
