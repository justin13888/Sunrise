//! The shared single-line text editor behind every prompt.
//!
//! Every typed surface in the TUI — capture, title edit, defer, schedule,
//! search, the `:` line — writes into one [`InputLine`]. Before this existed
//! the buffer was a bare `String` with `push` / `pop`, which meant the caret
//! was pinned to the end: a typo three words back cost the whole line. A
//! terminal client whose users live in readline and vim cannot ship that.
//!
//! Kept as its own module, with no dependency on the view or the keymap, so
//! the editing rules are unit-testable in isolation (the workspace rule in
//! `CLAUDE.md`) and so the cursor arithmetic — the part that is easy to get
//! wrong on multi-byte input — is exercised directly rather than through a
//! keypress.
//!
//! # Invariants
//!
//! * `cursor` is always a **byte index** on a `char` boundary of `text`, and
//!   always in `0..=text.len()`. Every method restores this before returning,
//!   so no caller can produce a panicking slice.
//! * A word, for the word-motions, is a run of non-whitespace. That is
//!   readline's rule rather than vim's finer `w`/`W` split, and it is the one
//!   users of `Ctrl-W` in a shell expect.

use std::fmt;

/// A single line of text with a caret.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputLine {
    text: String,
    cursor: usize,
}

impl InputLine {
    /// An empty line.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
        }
    }

    /// A line pre-filled with `text`, caret at the end (what "edit this
    /// existing title" wants).
    #[must_use]
    pub fn seeded(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.len();
        Self { text, cursor }
    }

    /// The current text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Caret position as a byte index.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// Caret position as a **character** offset — what a renderer needs to
    /// place a block cursor, since a terminal column is not a byte.
    #[must_use]
    pub fn cursor_chars(&self) -> usize {
        self.text[..self.cursor].chars().count()
    }

    /// Whether the line is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Length in bytes (the cap every prompt enforces is a byte budget).
    #[must_use]
    pub fn len(&self) -> usize {
        self.text.len()
    }

    /// The text with surrounding whitespace removed.
    #[must_use]
    pub fn trimmed(&self) -> &str {
        self.text.trim()
    }

    /// Replace the whole line, caret to the end.
    pub fn set(&mut self, text: impl Into<String>) {
        *self = Self::seeded(text);
    }

    /// Empty the line and reset the caret.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// Insert one character at the caret and step over it.
    pub fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Insert a whole string at the caret (a bracketed paste).
    ///
    /// Newlines and control characters are dropped rather than inserted: this
    /// is a one-line editor, and a pasted `\n` that survived into the buffer
    /// would render as a hole in the status line and submit as part of a title.
    pub fn insert_str(&mut self, s: &str) {
        let cleaned: String = s
            .chars()
            .map(|c| if c == '\t' { ' ' } else { c })
            .filter(|c| !c.is_control())
            .collect();
        self.text.insert_str(self.cursor, &cleaned);
        self.cursor += cleaned.len();
    }

    /// Delete the character before the caret.
    pub fn backspace(&mut self) {
        if let Some(prev) = self.prev_boundary() {
            self.text.replace_range(prev..self.cursor, "");
            self.cursor = prev;
        }
    }

    /// Delete the character under the caret (`Delete`).
    pub fn delete_forward(&mut self) {
        if let Some(next) = self.next_boundary() {
            self.text.replace_range(self.cursor..next, "");
        }
    }

    /// Move one character left.
    pub fn left(&mut self) {
        if let Some(prev) = self.prev_boundary() {
            self.cursor = prev;
        }
    }

    /// Move one character right.
    pub fn right(&mut self) {
        if let Some(next) = self.next_boundary() {
            self.cursor = next;
        }
    }

    /// Move to the start of the line (`Ctrl-A` / `Home` / vim `0`).
    pub fn home(&mut self) {
        self.cursor = 0;
    }

    /// Move to the end of the line (`Ctrl-E` / `End` / vim `$`).
    pub fn end(&mut self) {
        self.cursor = self.text.len();
    }

    /// Move to the start of the previous word.
    pub fn word_left(&mut self) {
        self.cursor = self.word_start();
    }

    /// Move to the start of the next word.
    pub fn word_right(&mut self) {
        let rest = &self.text[self.cursor..];
        let skipped_word: usize = rest
            .char_indices()
            .find(|(_, c)| c.is_whitespace())
            .map_or(rest.len(), |(i, _)| i);
        let after = &rest[skipped_word..];
        let skipped_space: usize = after
            .char_indices()
            .find(|(_, c)| !c.is_whitespace())
            .map_or(after.len(), |(i, _)| i);
        self.cursor += skipped_word + skipped_space;
    }

    /// Delete the word before the caret (`Ctrl-W`).
    pub fn delete_word_back(&mut self) {
        let start = self.word_start();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Delete from the caret to the start of the line (`Ctrl-U`).
    pub fn kill_to_start(&mut self) {
        self.text.replace_range(..self.cursor, "");
        self.cursor = 0;
    }

    /// Delete from the caret to the end of the line (`Ctrl-K`).
    pub fn kill_to_end(&mut self) {
        self.text.truncate(self.cursor);
    }

    /// Byte index of the previous `char` boundary, or `None` at the start.
    fn prev_boundary(&self) -> Option<usize> {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
    }

    /// Byte index of the next `char` boundary, or `None` at the end.
    fn next_boundary(&self) -> Option<usize> {
        self.text[self.cursor..]
            .chars()
            .next()
            .map(|c| self.cursor + c.len_utf8())
    }

    /// Start of the word the caret is in or just past — skipping any
    /// whitespace immediately behind it first, so `Ctrl-W` at "foo bar  |"
    /// removes `bar` and its trailing run rather than only the spaces.
    fn word_start(&self) -> usize {
        let head = &self.text[..self.cursor];
        let trimmed = head.trim_end();
        match trimmed.rfind(char::is_whitespace) {
            Some(i) => i + head[i..].chars().next().map_or(1, char::len_utf8),
            None => 0,
        }
    }
}

impl From<&str> for InputLine {
    fn from(s: &str) -> Self {
        Self::seeded(s)
    }
}

impl From<String> for InputLine {
    fn from(s: String) -> Self {
        Self::seeded(s)
    }
}

impl PartialEq<str> for InputLine {
    fn eq(&self, other: &str) -> bool {
        self.text == other
    }
}

impl PartialEq<&str> for InputLine {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

impl PartialEq<String> for InputLine {
    fn eq(&self, other: &String) -> bool {
        self.text == *other
    }
}

impl fmt::Display for InputLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, cursor_chars: usize) -> InputLine {
        let mut l = InputLine::seeded(text);
        l.home();
        for _ in 0..cursor_chars {
            l.right();
        }
        l
    }

    #[test]
    fn seeded_puts_the_caret_at_the_end() {
        let l = InputLine::seeded("hello");
        assert_eq!(l.cursor(), 5);
        assert_eq!(l.text(), "hello");
    }

    #[test]
    fn insert_happens_at_the_caret_not_the_end() {
        let mut l = line("helo", 3);
        l.insert_char('l');
        assert_eq!(l.text(), "hello");
        assert_eq!(l.cursor_chars(), 4);
    }

    #[test]
    fn backspace_removes_before_the_caret() {
        let mut l = line("hello", 3);
        l.backspace();
        assert_eq!(l.text(), "helo");
        assert_eq!(l.cursor_chars(), 2);
    }

    #[test]
    fn backspace_at_the_start_is_a_no_op() {
        let mut l = line("hi", 0);
        l.backspace();
        assert_eq!(l.text(), "hi");
        assert_eq!(l.cursor(), 0);
    }

    #[test]
    fn delete_forward_removes_under_the_caret_and_holds_position() {
        let mut l = line("hello", 1);
        l.delete_forward();
        assert_eq!(l.text(), "hllo");
        assert_eq!(l.cursor_chars(), 1);
    }

    #[test]
    fn delete_forward_at_the_end_is_a_no_op() {
        let mut l = InputLine::seeded("hi");
        l.delete_forward();
        assert_eq!(l.text(), "hi");
    }

    #[test]
    fn multibyte_motions_land_on_char_boundaries() {
        // Four chars, ten bytes. Byte-stepping would slice mid-codepoint.
        let mut l = InputLine::seeded("naïve—x");
        l.home();
        for _ in 0..4 {
            l.right();
        }
        assert_eq!(l.cursor_chars(), 4);
        l.insert_char('!');
        assert_eq!(l.text(), "naïv!e—x");
        l.backspace();
        assert_eq!(l.text(), "naïve—x");
    }

    #[test]
    fn word_motions_step_over_runs_of_non_space() {
        let mut l = line("buy milk today", 0);
        l.word_right();
        assert_eq!(l.cursor_chars(), 4);
        l.word_right();
        assert_eq!(l.cursor_chars(), 9);
        l.word_right();
        assert_eq!(l.cursor_chars(), 14);
        l.word_left();
        assert_eq!(l.cursor_chars(), 9);
    }

    #[test]
    fn delete_word_back_eats_the_word_and_the_space_behind_it() {
        let mut l = InputLine::seeded("buy milk  ");
        l.delete_word_back();
        assert_eq!(l.text(), "buy ");
        l.delete_word_back();
        assert_eq!(l.text(), "");
    }

    #[test]
    fn kill_to_start_and_end_split_at_the_caret() {
        let mut l = line("hello world", 5);
        l.kill_to_end();
        assert_eq!(l.text(), "hello");
        let mut l = line("hello world", 6);
        l.kill_to_start();
        assert_eq!(l.text(), "world");
        assert_eq!(l.cursor(), 0);
    }

    #[test]
    fn paste_strips_newlines_and_control_characters() {
        let mut l = InputLine::new();
        l.insert_str("two\nlines\there\u{7}");
        assert_eq!(l.text(), "twolines here");
        assert_eq!(l.cursor(), l.len());
    }

    #[test]
    fn set_replaces_and_reseats_the_caret() {
        let mut l = line("abcdef", 1);
        l.set("xy");
        assert_eq!(l.text(), "xy");
        assert_eq!(l.cursor(), 2);
    }

    #[test]
    fn home_and_end_are_idempotent_at_the_edges() {
        let mut l = InputLine::seeded("abc");
        l.end();
        l.end();
        assert_eq!(l.cursor(), 3);
        l.home();
        l.home();
        assert_eq!(l.cursor(), 0);
        l.left();
        assert_eq!(l.cursor(), 0);
        l.end();
        l.right();
        assert_eq!(l.cursor(), 3);
    }
}
