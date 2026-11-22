//! The registry check: every bench `benches/*.rs` names has a row in
//! [`BENCHES`], and every row names a bench. It reads the bench sources as
//! text, so the readers here change with the ways those files spell a name
//! rather than with how a run is collected or judged.

use std::path::Path;

use super::{Bench, BENCHES};

/// The names `benches/*.rs` give Criterion, read from the source, each
/// with the file it came from.
#[derive(Debug, Default)]
struct SourceNames {
    /// Every `benchmark_group` name and ungrouped `bench_function` name:
    /// the first segment of every directory the file writes.
    heads: Vec<(String, String)>,
    /// Every directory the source spells out in full: an ungrouped
    /// literal, or a group's literal name, or its
    /// `BenchmarkId::from_parameter` over a loop's array literal.
    dirs: Vec<(String, String)>,
    /// Each start a `format!` name can take, as `<group>/<prefix>` (see
    /// [`format_starts`]): the rest is a size, so only the start is known
    /// here.
    prefixes: Vec<(String, String)>,
}

/// The text of the string literal `s` opens with, if it opens with one.
fn quoted(s: &str) -> Option<&str> {
    let rest = s.strip_prefix('"')?;
    rest.find('"').map(|end| &rest[..end])
}

/// The names a `for` loop over an array literal binds to `ident`, for a
/// `BenchmarkId::from_parameter(ident)` call in its body. `before` is the
/// source up to that call. The binding is `ident` or a tuple whose first
/// element is `ident`; each array element's first string literal is the
/// name it binds.
fn loop_names(file: &str, before: &str, ident: &str) -> Vec<String> {
    let at = before
        .rfind("for ")
        .unwrap_or_else(|| panic!("{file}: from_parameter({ident}) is in no `for` loop"));
    let (binding, rest) = before[at + 4..].split_once(" in [").unwrap_or_else(|| {
        panic!("{file}: the loop binding `{ident}` is not over an array literal")
    });
    let first = binding.trim().trim_start_matches('(').split(',').next();
    assert_eq!(
        first.map(str::trim),
        Some(ident),
        "{file}: from_parameter({ident}) does not name the loop's first binding"
    );
    let mut names = Vec::new();
    let (mut depth, mut start) = (0usize, 0usize);
    for (i, ch) in rest.char_indices() {
        let element_ends = match ch {
            '(' | '[' => {
                depth += 1;
                false
            }
            ']' if depth == 0 => true,
            ')' | ']' => {
                depth -= 1;
                false
            }
            ',' => depth == 0,
            _ => false,
        };
        if element_ends {
            let element = rest[start..i].trim().trim_start_matches('(').trim_start();
            if !element.is_empty() {
                let name = quoted(element).unwrap_or_else(|| {
                    panic!("{file}: an element of `{ident}`'s array opens with no string literal")
                });
                names.push(name.to_string());
            }
            if ch == ']' {
                return names;
            }
            start = i + 1;
        }
    }
    panic!("{file}: `{ident}`'s array literal never closes")
}

/// The literal starts a `format!` name `template` can take, up to its
/// first size placeholder. `before` is the source up to the call. A
/// template that opens with a placeholder, such as `{arm}/{rows}`, starts
/// with whatever its last `let arm = …;` before the call binds: each
/// string literal in that statement is one start, followed by the
/// template's literal text up to its next `{`. A start that is still
/// empty would match every row in the group, so it fails here.
fn format_starts(file: &str, before: &str, template: &str) -> Vec<String> {
    let Some(rest) = template.strip_prefix('{') else {
        assert!(!template.is_empty(), "{file}: a format! name is empty");
        return vec![template[..template.find('{').unwrap_or(template.len())].to_string()];
    };
    let (ident, tail) = rest
        .split_once('}')
        .unwrap_or_else(|| panic!("{file}: format!(\"{template}\") never closes its `{{`"));
    let tail = &tail[..tail.find('{').unwrap_or(tail.len())];
    assert!(
        !ident.is_empty() && ident.chars().all(|c| c.is_alphanumeric() || c == '_'),
        "{file}: format!(\"{template}\") opens with `{{{ident}}}`, which names no binding \
         this test can read"
    );
    let binding = format!("let {ident} = ");
    let at = before.rfind(&binding).unwrap_or_else(|| {
        panic!("{file}: format!(\"{template}\") opens with `{{{ident}}}`, bound by no `let`")
    });
    let statement = &before[at + binding.len()..];
    let statement = &statement[..statement
        .find(';')
        .unwrap_or_else(|| panic!("{file}: `let {ident}` never ends"))];
    let starts: Vec<String> = statement
        .split('"')
        .skip(1)
        .step_by(2)
        .map(|value| format!("{value}{tail}"))
        .collect();
    assert!(
        !starts.is_empty() && starts.iter().all(|s| !s.is_empty()),
        "{file}: format!(\"{template}\") opens with `{{{ident}}}`, and `let {ident}` binds \
         no non-empty string literal, so its names have no start to hold a row to"
    );
    starts
}

/// Read every `benchmark_group` and `bench_function` name in
/// `benches/*.rs`. A file holds at most one group, and every
/// `bench_function` in a file with one belongs to it. A name built in a
/// way this does not read fails here rather than escaping the check.
fn bench_source_names() -> SourceNames {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("benches");
    let mut out = SourceNames::default();
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("read benches/")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "no bench sources under {}",
        dir.display()
    );
    for path in files {
        let text = std::fs::read_to_string(&path).expect("read bench source");
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        let groups: Vec<&str> = text
            .match_indices("benchmark_group(")
            .map(|(at, call)| {
                quoted(text[at + call.len()..].trim_start()).unwrap_or_else(|| {
                    panic!("{file}: a benchmark_group name is not a string literal")
                })
            })
            .collect();
        assert!(
            groups.len() <= 1,
            "{file} opens more than one benchmark_group"
        );
        let group = groups.first().copied();
        if let Some(g) = group {
            out.heads.push((file.clone(), g.to_string()));
        }
        for (at, call) in text.match_indices("bench_function(") {
            let arg = text[at + call.len()..].trim_start();
            // Criterion writes a function's own `/` as `_`.
            let in_group = |name: &str| {
                let g = group.unwrap_or_else(|| {
                    panic!("{file}: `{name}` is built, so it needs a benchmark_group")
                });
                format!("{g}/{}", name.replace('/', "_"))
            };
            if let Some(name) = quoted(arg) {
                if group.is_some() {
                    out.dirs.push((file.clone(), in_group(name)));
                } else {
                    out.heads.push((file.clone(), name.to_string()));
                    out.dirs.push((file.clone(), name.to_string()));
                }
            } else if let Some(inner) = arg.strip_prefix("format!(") {
                let template = quoted(inner.trim_start())
                    .unwrap_or_else(|| panic!("{file}: a format! name with no literal"));
                for start in format_starts(&file, &text[..at], template) {
                    out.prefixes.push((file.clone(), in_group(&start)));
                }
            } else if let Some(inner) = arg.strip_prefix("BenchmarkId::from_parameter(") {
                let ident = inner[..inner.find(')').expect("closing paren")].trim();
                for name in loop_names(&file, &text[..at], ident) {
                    out.dirs.push((file.clone(), in_group(&name)));
                }
            } else {
                let shown: String = arg.chars().take(40).collect();
                panic!(
                    "{file}: bench_function({shown}…) is a name this test cannot read; \
                     teach bench_source_names to read it"
                );
            }
        }
        // A file that names no bench would escape both directions of the
        // check below.
        assert!(
            out.heads.iter().any(|(f, _)| *f == file),
            "{file} names no bench and opens no benchmark_group"
        );
    }
    out
}

/// The defect #366 found: `sync_session` replaced `ws_handshake`, the
/// table kept the old name, and nothing noticed. Each direction is a
/// failure: a bench with no row is never compared, and a row with no bench
/// reads a directory nothing writes. #1309 found the same inside a group:
/// `plan_preview/occurrence` joined the loop of an existing group, so its
/// head already had rows and only the full directory was missing one.
#[test]
fn every_bench_has_a_row_and_every_row_a_bench() {
    let src = bench_source_names();
    let head = |b: &Bench| b.dir.split('/').next().unwrap_or(b.dir);
    for (file, name) in &src.heads {
        assert!(
            BENCHES.iter().any(|b| head(b) == name),
            "{file}: bench `{name}` has no row in BENCHES, so baseline.rs never reads it"
        );
    }
    for (file, dir) in &src.dirs {
        assert!(
            BENCHES.iter().any(|b| b.dir == dir),
            "{file}: bench `{dir}` has no row in BENCHES, so baseline.rs never reads it"
        );
    }
    for (file, prefix) in &src.prefixes {
        assert!(
            BENCHES.iter().any(|b| b.dir.starts_with(prefix.as_str())),
            "{file}: no BENCHES row starts `{prefix}`, so baseline.rs never reads that bench"
        );
    }
    for b in BENCHES {
        assert!(
            src.heads.iter().any(|(_, n)| n == head(b)),
            "BENCHES row `{}` names no bench in benches/*.rs",
            b.dir
        );
        // Each row must be a directory the source names in full, or one a
        // `format!` name writes by size, which only its start can hold.
        let sized = src
            .prefixes
            .iter()
            .any(|(_, p)| b.dir.starts_with(p.as_str()));
        assert!(
            sized || src.dirs.iter().any(|(_, d)| d == b.dir),
            "BENCHES row `{}` names no bench in benches/*.rs",
            b.dir
        );
    }
}
