//! The content-line layer of RFC 5545: unfolding and folding (§3.1),
//! splitting a line into name, parameters and value, reading one parameter,
//! and TEXT escaping (§3.3.11) in both directions.
//!
//! Nothing here knows what a component or a property means; the parser in
//! the parent module does.

/// Maximum octets in a content line before RFC 5545 §3.1 requires folding.
pub(super) const FOLD_AT: usize = 75;

/// RFC 5545 §3.1: a line beginning with SPACE or HTAB continues the previous
/// logical line, and the one leading whitespace octet is not part of it.
pub(super) fn unfold_lines(input: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in input.lines() {
        if let Some(prev) = out.last_mut() {
            if let Some(rest) = raw.strip_prefix(' ').or_else(|| raw.strip_prefix('\t')) {
                prev.push_str(rest);
                continue;
            }
        }
        out.push(raw.to_string());
    }
    out
}

/// Split `NAME;PARAM=v:value` into its three parts.
///
/// The first `:` outside a quoted parameter value ends the name-and-parameters
/// half. Quoting matters: `TZID="GMT+02:00"` is one parameter whose value
/// contains a colon, and splitting on the first bare `:` would truncate it.
pub(super) fn split_property(line: &str) -> (&str, &str, &str) {
    let mut in_quotes = false;
    let mut split = line.len();
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => {
                split = i;
                break;
            }
            _ => {}
        }
    }
    let head = &line[..split];
    let value = if split == line.len() {
        ""
    } else {
        &line[split + 1..]
    };
    let name_end = head.find(';').unwrap_or(head.len());
    (&head[..name_end], &head[name_end..], value)
}

/// One parameter's value, unquoted. `params` is the `;`-prefixed remainder
/// from [`split_property`].
pub(super) fn param<'a>(params: &'a str, want: &str) -> Option<&'a str> {
    for part in params.split(';') {
        // `params` keeps its leading `;`, so the first split is empty, and a
        // malformed parameter with no `=` must be stepped over rather than
        // ending the search.
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        if k.trim().eq_ignore_ascii_case(want) {
            return Some(v.trim().trim_matches('"'));
        }
    }
    None
}

/// Append one content line, folded per RFC 5545 §3.1.
///
/// Folds on a char boundary at or before 75 octets: a fold that split a
/// multi-byte character would produce two invalid UTF-8 fragments, and the
/// 75-octet limit is a maximum, so stopping short of it is always legal.
pub(super) fn push_line(out: &mut String, name: &str, params: &str, value: &str) {
    let line = format!("{name}{params}:{value}");
    let bytes = line.as_bytes();
    if bytes.len() <= FOLD_AT {
        out.push_str(&line);
        out.push_str("\r\n");
        return;
    }
    let mut start = 0;
    // The first line may use all 75 octets; a continuation spends one on the
    // leading space.
    let mut budget = FOLD_AT;
    while start < line.len() {
        let mut end = (start + budget).min(line.len());
        while end > start && !line.is_char_boundary(end) {
            end -= 1;
        }
        if start > 0 {
            out.push(' ');
        }
        out.push_str(&line[start..end]);
        out.push_str("\r\n");
        start = end;
        budget = FOLD_AT - 1;
    }
}

/// RFC 5545 §3.3.11 TEXT unescaping.
pub(super) fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(',') => out.push(','),
            Some(';') => out.push(';'),
            // An escape of anything else is not defined, and a trailing
            // backslash escapes nothing: keep the backslash rather than eating
            // a character the file did not intend to lose.
            Some('\\') | None => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

/// RFC 5545 §3.3.11 TEXT escaping. Backslash first, or it would double the
/// backslashes the later rules introduce.
pub(super) fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace(',', "\\,")
        .replace(';', "\\;")
        .replace('\r', "")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `TZID="GMT+02:00"` is one parameter containing a colon. Splitting on
    /// the first bare `:` would leave `TZID="GMT+02` and a truncated value.
    #[test]
    fn a_quoted_parameter_may_contain_a_colon() {
        let (name, params, value) = split_property("DTSTART;TZID=\"GMT+02:00\":20260301T090000");
        assert_eq!(name, "DTSTART");
        assert_eq!(params, ";TZID=\"GMT+02:00\"");
        assert_eq!(value, "20260301T090000");
        assert_eq!(param(params, "TZID"), Some("GMT+02:00"));
    }

    #[test]
    fn unescaping_leaves_an_undefined_escape_alone() {
        assert_eq!(unescape("a\\qb"), "a\\qb");
        assert_eq!(unescape("trailing\\"), "trailing\\");
    }
}
