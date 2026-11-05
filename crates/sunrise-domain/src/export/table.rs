//! The generic table and its two renderers: a [`Table`] of [`Cell`]s to CSV or
//! JSON, with the escaping the parent module's documentation explains.

use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

/// One exported value. Deliberately a small closed set: an export is a table
/// of scalars, never a nested document.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    /// Free text — the only variant that needs escaping.
    Text(String),
    /// Signed integer (counts, deltas, epoch milliseconds).
    Int(i64),
    /// Ratio or factor. Non-finite values render as empty / `null`.
    Float(f64),
    /// Boolean.
    Bool(bool),
    /// Absent value.
    Null,
}

impl Cell {
    /// Text cell from anything string-like.
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }

    /// Integer cell from a `u64`, saturating at [`i64::MAX`].
    #[must_use]
    pub fn u64(v: u64) -> Self {
        Self::Int(i64::try_from(v).unwrap_or(i64::MAX))
    }

    /// Integer cell from a `u32`.
    #[must_use]
    pub fn u32(v: u32) -> Self {
        Self::Int(i64::from(v))
    }

    /// Optional epoch-millisecond cell.
    #[must_use]
    pub fn opt_ms(v: Option<u64>) -> Self {
        v.map_or(Self::Null, Self::u64)
    }

    /// The cell's unquoted CSV text. `Null` is the empty field.
    fn raw(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Int(v) => v.to_string(),
            Self::Float(v) => {
                if v.is_finite() {
                    format!("{v}")
                } else {
                    String::new()
                }
            }
            Self::Bool(b) => b.to_string(),
            Self::Null => String::new(),
        }
    }
}

/// One export dataset: a name, a header, and rows of [`Cell`]s.
///
/// The invariant every renderer relies on is that each row has exactly
/// `columns.len()` cells. [`Table::push`] enforces it by padding short rows
/// with [`Cell::Null`] and truncating long ones.
///
/// The enforcement lives in `push`, not in the type: `columns` and `rows` are
/// public, so a caller that appends to `rows` directly bypasses it. A ragged
/// row built that way is not caught here -- [`Table::to_json`] indexes
/// `columns` by the row's own cell position and panics past its end, and
/// [`Table::to_csv`] silently writes an over-wide record. Build rows with
/// `push`.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    /// Dataset name (`"trends"`, `"activity"`, …).
    pub name: String,
    /// Column headers, in order.
    pub columns: Vec<String>,
    /// Rows, each exactly `columns.len()` long.
    pub rows: Vec<Vec<Cell>>,
}

impl Table {
    /// Empty table with the given name and columns.
    pub fn new(name: impl Into<String>, columns: &[&str]) -> Self {
        Self {
            name: name.into(),
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            rows: Vec::new(),
        }
    }

    /// Append a row, normalizing it to the header width.
    pub fn push(&mut self, mut row: Vec<Cell>) {
        row.resize(self.columns.len(), Cell::Null);
        self.rows.push(row);
    }

    /// Render in the requested format.
    #[must_use]
    pub fn render(&self, format: ExportFormat) -> String {
        match format {
            ExportFormat::Csv => self.to_csv(),
            ExportFormat::Json => self.to_json(),
        }
    }

    /// RFC 4180 CSV: header row, CRLF record terminators, minimal quoting.
    #[must_use]
    pub fn to_csv(&self) -> String {
        let mut out = String::new();
        let header: Vec<String> = self.columns.iter().map(|c| csv_field(c)).collect();
        out.push_str(&header.join(","));
        out.push_str("\r\n");
        for row in &self.rows {
            let cells: Vec<String> = row.iter().map(|c| csv_field(&c.raw())).collect();
            out.push_str(&cells.join(","));
            out.push_str("\r\n");
        }
        out
    }

    /// JSON: `{"table":…,"columns":[…],"rows":[{…}]}`.
    ///
    /// Rows are objects keyed by column name so a consumer never has to trust
    /// positional order, and typed cells keep their type (an integer count
    /// stays a number, not a string).
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\"table\":");
        json_string(&self.name, &mut out);
        out.push_str(",\"columns\":[");
        for (i, c) in self.columns.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            json_string(c, &mut out);
        }
        out.push_str("],\"rows\":[");
        for (i, row) in self.rows.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('{');
            for (j, cell) in row.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                json_string(&self.columns[j], &mut out);
                out.push(':');
                json_cell(cell, &mut out);
            }
            out.push('}');
        }
        out.push_str("]}");
        out
    }
}

/// Export serialization format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    /// RFC 4180 CSV.
    Csv,
    /// JSON object with typed row objects.
    Json,
}

impl ExportFormat {
    /// Lowercase wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Json => "json",
        }
    }

    /// Parse from the lowercase wire name.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "csv" => Some(Self::Csv),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

/// Quote a CSV field when — and only when — the RFC requires it.
///
/// Leading / trailing whitespace is quoted too. The RFC does not demand it,
/// but an unquoted `" ,"` is indistinguishable from a parser's optional
/// whitespace trimming, and a title that begins with a space must survive the
/// round trip byte-for-byte.
pub(super) fn csv_field(s: &str) -> String {
    let needs_quotes = s.contains(['"', ',', '\n', '\r'])
        || s.starts_with(char::is_whitespace)
        || s.ends_with(char::is_whitespace);
    if !needs_quotes {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// Append `s` as a JSON string literal, escaping per RFC 8259 §7.
fn json_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                // `write!` into a String is infallible.
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Append one cell as a JSON value.
fn json_cell(cell: &Cell, out: &mut String) {
    match cell {
        Cell::Text(s) => json_string(s, out),
        Cell::Int(v) => {
            let _ = write!(out, "{v}");
        }
        Cell::Float(v) => {
            if v.is_finite() {
                let _ = write!(out, "{v}");
            } else {
                out.push_str("null");
            }
        }
        Cell::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Cell::Null => out.push_str("null"),
    }
}
