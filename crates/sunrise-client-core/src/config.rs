//! The tiny TOML subset Sunrise's preference files are written in.
//!
//! Two per-device preference files use it — the key overrides a terminal
//! client reads, and the saved views in [`crate::views`]. Both are a flat list
//! of `name = "value"` pairs.
//!
//! Hand-rolled on purpose: the workspace has no TOML dependency, and pulling
//! one in to read a flat list of string pairs would be the largest dependency
//! in the tree for the smallest grammar in it. Anything outside the subset is
//! an [`Err`] naming the line, which the caller turns into a startup warning —
//! a malformed preference file must never be silently ignored, and must never
//! cost the user a working app either.

/// Parse the subset: comments, optional `[table]` headers, and
/// `name = "value"` pairs.
///
/// # Errors
///
/// Returns a message naming the offending line number.
pub fn parse_pairs(src: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (n, raw) in src.lines().enumerate() {
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            if line.ends_with(']') {
                // Table headers are accepted and ignored: `[keys]` and a flat
                // file mean the same thing here.
                continue;
            }
            return Err(format!("line {}: unterminated table header", n + 1));
        }
        let Some((name, value)) = line.split_once('=') else {
            return Err(format!("line {}: expected `name = \"value\"`", n + 1));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("line {}: missing name", n + 1));
        }
        let value = value.trim();
        let quoted = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')));
        let Some(key) = quoted else {
            return Err(format!("line {}: value must be a quoted string", n + 1));
        };
        out.push((name.to_string(), key.to_string()));
    }
    Ok(out)
}

/// Drop a trailing `#` comment, ignoring `#` inside a quoted value (so
/// `capture = "#"` survives).
fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (None, '"' | '\'') => quote = Some(c),
            (None, '#') => return &line[..i],
            _ => {}
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flat_list_of_pairs_parses() {
        let pairs = parse_pairs("capture = \"n\"\nquit = 'Q'\n").expect("parses");
        assert_eq!(
            pairs,
            vec![
                ("capture".to_string(), "n".to_string()),
                ("quit".to_string(), "Q".to_string()),
            ]
        );
    }

    #[test]
    fn comments_and_table_headers_are_ignored() {
        let src = "# a comment\n[keys]\ncapture = \"n\"  # trailing\n";
        assert_eq!(
            parse_pairs(src).expect("parses"),
            vec![("capture".to_string(), "n".to_string())]
        );
    }

    #[test]
    fn a_hash_inside_a_quoted_value_survives() {
        // `capture = "#"` is a legitimate binding, not a comment.
        assert_eq!(
            parse_pairs("capture = \"#\"").expect("parses"),
            vec![("capture".to_string(), "#".to_string())]
        );
    }

    #[test]
    fn anything_outside_the_subset_names_its_line() {
        for (src, needle) in [
            ("capture n\n", "line 1"),
            ("capture = n\n", "quoted string"),
            ("[keys\n", "unterminated"),
            (" = \"n\"\n", "missing name"),
        ] {
            let e = parse_pairs(src).unwrap_err();
            assert!(e.contains(needle), "{src:?} -> {e}");
        }
    }
}
