//! Source normalisation, and the whole-word search every scanner shares.

/// One source in two aligned forms.
///
/// Both are byte-for-byte the same length as the original, so an offset found
/// in one indexes the other. `structural` has comments and string *contents*
/// blanked, which is what makes brace matching, argument splitting and word
/// search safe. `literal` blanks the same regions but keeps string contents,
/// which is where an event name is read from.
///
/// `#[cfg(test)]` is *not* blanked. A scan for the end of the attributed item
/// assumed a brace-balanced one, and on a struct field or an enum variant it
/// ran on — measured, it blanked 130 lines of `api/sync.rs` (now `api/sync/`) and hid every
/// `tracing::` call inside from every gate. Deleting it costs a constraint
/// (a unit test in a shipped file must use allowlisted field names) that the
/// workspace already met.
pub(crate) struct Source {
    pub(crate) structural: String,
    pub(crate) literal: String,
}

/// Comments blanked, and string contents blanked when `blank_strings`.
///
/// One long function on purpose: it is a single lexer state machine, and
/// splitting it into arms that each need the cursor and the mode would make it
/// harder to check, not easier.
///
/// Byte length is preserved exactly: a multi-byte char that is blanked becomes
/// that many spaces. Both halves of a [`Source`] therefore share one offset
/// space.
#[allow(clippy::too_many_lines)]
fn blank(text: &str, blank_strings: bool) -> String {
    let src: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let push_blank = |out: &mut String, c: char| {
        if c == '\n' {
            out.push('\n');
        } else {
            for _ in 0..c.len_utf8() {
                out.push(' ');
            }
        }
    };
    while i < src.len() {
        let c = src[i].1;
        // Raw string: `r`, zero or more `#`, then a quote.
        if c == 'r' {
            let mut h = i + 1;
            while h < src.len() && src[h].1 == '#' {
                h += 1;
            }
            if h < src.len() && src[h].1 == '"' {
                let hashes = h - i - 1;
                for (_, ch) in &src[i..=h] {
                    out.push(*ch);
                }
                i = h + 1;
                while i < src.len() {
                    if src[i].1 == '"' {
                        let mut n = 0;
                        while n < hashes && src.get(i + 1 + n).is_some_and(|c| c.1 == '#') {
                            n += 1;
                        }
                        if n == hashes {
                            out.push('"');
                            (0..hashes).for_each(|_| out.push('#'));
                            i += 1 + hashes;
                            break;
                        }
                    }
                    if blank_strings {
                        push_blank(&mut out, src[i].1);
                    } else {
                        out.push(src[i].1);
                    }
                    i += 1;
                }
                continue;
            }
        }
        if c == '"' {
            out.push('"');
            i += 1;
            while i < src.len() && src[i].1 != '"' {
                if src[i].1 == '\\' {
                    if blank_strings {
                        push_blank(&mut out, src[i].1);
                    } else {
                        out.push(src[i].1);
                    }
                    i += 1;
                }
                if i < src.len() {
                    if blank_strings {
                        push_blank(&mut out, src[i].1);
                    } else {
                        out.push(src[i].1);
                    }
                    i += 1;
                }
            }
            if i < src.len() {
                out.push('"');
                i += 1;
            }
            continue;
        }
        // A char literal (`'x'`, `'\n'`, `'\''`) can hold a quote or a paren; a
        // lifetime (`'a`, `'static`) cannot, and must be copied through.
        if c == '\'' {
            let end = if src.get(i + 1).is_some_and(|c| c.1 == '\\') {
                src.get(i + 3).filter(|c| c.1 == '\'').map(|_| i + 3)
            } else {
                src.get(i + 2).filter(|c| c.1 == '\'').map(|_| i + 2)
            };
            if let Some(end) = end {
                out.push('\'');
                for (_, ch) in &src[i + 1..end] {
                    push_blank(&mut out, *ch);
                }
                out.push('\'');
                i = end + 1;
                continue;
            }
        }
        if c == '/' && src.get(i + 1).is_some_and(|c| c.1 == '/') {
            while i < src.len() && src[i].1 != '\n' {
                push_blank(&mut out, src[i].1);
                i += 1;
            }
            continue;
        }
        if c == '/' && src.get(i + 1).is_some_and(|c| c.1 == '*') {
            while i + 1 < src.len() && !(src[i].1 == '*' && src[i + 1].1 == '/') {
                push_blank(&mut out, src[i].1);
                i += 1;
            }
            if i < src.len() {
                push_blank(&mut out, src[i].1);
                i += 1;
            }
            if i < src.len() {
                push_blank(&mut out, src[i].1);
                i += 1;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

pub(crate) fn normalise(text: &str) -> Source {
    let structural = blank(text, true);
    let literal = blank(text, false);
    assert_eq!(structural.len(), text.len(), "blanking changed byte length");
    assert_eq!(literal.len(), text.len(), "blanking changed byte length");
    Source {
        structural,
        literal,
    }
}

/// Whether `b` can appear inside a Rust identifier.
pub(crate) fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Every whole-word occurrence of `word` followed — across any whitespace — by
/// a `!`, as `(start of the word, index of the bang)`.
///
/// The whitespace tolerance is the point. `tracing::warn !(…)` and
/// `tracing::warn` + newline + `!(…)` are both legal, both record their fields,
/// and both are invisible to a scanner looking for the literal `warn!`.
pub(crate) fn word_bang_hits(text: &str, word: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut hits = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find(word).map(|r| from + r) {
        from = at + word.len();
        if at > 0 && is_ident_byte(bytes[at - 1]) {
            continue;
        }
        let mut j = at + word.len();
        if bytes.get(j).copied().is_some_and(is_ident_byte) {
            continue;
        }
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if bytes.get(j) == Some(&b'!') {
            hits.push((at, j));
        }
    }
    hits
}

/// Every whole-word occurrence of `word`, as a byte offset.
///
/// The plain-word sibling of [`word_bang_hits`], used where the question is
/// whether a name appears at all rather than whether a macro is invoked —
/// reading a build tool's eleven lines for `mod` and for `tracing`.
pub(crate) fn word_hits(text: &str, word: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    let mut hits = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find(word).map(|r| from + r) {
        from = at + word.len();
        if at > 0 && is_ident_byte(bytes[at - 1]) {
            continue;
        }
        if bytes
            .get(at + word.len())
            .copied()
            .is_some_and(is_ident_byte)
        {
            continue;
        }
        hits.push(at);
    }
    hits
}
