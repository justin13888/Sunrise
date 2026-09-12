//! Tabular export per `docs/08-features/reviews-and-stats.md` §Export:
//! "Stats are computable from the op log; we export them on request as JSON or
//! CSV via the export pipeline."
//!
//! Everything here is a **pure value → text** transform. A [`Table`] is built
//! by the stats/review folds, and the two renderers below turn it into either
//! RFC 4180 CSV or JSON. Neither renderer reads a clock, a database, or a
//! locale, so an export is a deterministic function of the numbers it was
//! handed.
//!
//! # Why the escaping is the interesting part
//!
//! A review export carries *user text* — task titles, stream names, note
//! excerpts — into a format with structural delimiters. A title containing a
//! comma, a double quote, a newline, or a lone `\r` is ordinary, and an export
//! that silently shifts a column when it meets one is worse than no export at
//! all. So:
//!
//! * **CSV** quotes any field containing `"`, `,`, `\r`, `\n`, or leading /
//!   trailing whitespace, and doubles embedded quotes ([RFC 4180] §2.6–2.7).
//!   Records are terminated with CRLF, as the RFC specifies.
//! * **JSON** escapes `"` and `\`, uses the short forms for the five
//!   whitespace controls, and `\u00XX` for every other C0 control — the set
//!   [RFC 8259] §7 requires to be escaped.
//!
//! Neither renderer can produce a document whose field count differs from its
//! header; that is what the hostile-content tests in this module pin down.
//!
//! [RFC 4180]: https://www.rfc-editor.org/rfc/rfc4180
//! [RFC 8259]: https://www.rfc-editor.org/rfc/rfc8259

use crate::activity::{ActivityEvent, ActivityKind};
use crate::focus::FocusStats;
use crate::stats::Trends;
use crate::streak::StreakRow;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use sunrise_id::EntityRef;

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
fn csv_field(s: &str) -> String {
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

// ---------------------------------------------------------------------------
// Dataset builders — the four tables `docs/08-features/reviews-and-stats.md`
// §Export makes available. Each is a plain projection of a fold's output; none
// of them recompute anything.
// ---------------------------------------------------------------------------

/// Which dataset an export request wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportDataset {
    /// Per-week completed / deferred / created, whole-vault and per Stream.
    Trends,
    /// The activity timeline.
    Activity,
    /// Time-in-focus and the estimate calibration, per Stream.
    Focus,
    /// Routine streaks.
    Streaks,
}

impl ExportDataset {
    /// Lowercase wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trends => "trends",
            Self::Activity => "activity",
            Self::Focus => "focus",
            Self::Streaks => "streaks",
        }
    }

    /// Parse from the lowercase wire name.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "trends" => Some(Self::Trends),
            "activity" => Some(Self::Activity),
            "focus" => Some(Self::Focus),
            "streaks" => Some(Self::Streaks),
            _ => None,
        }
    }
}

/// Long-format trend table: one row per `(week, stream)` plus the whole-vault
/// rows under the reserved stream name `"(all)"`.
///
/// Long format rather than a week-per-column pivot because the number of weeks
/// is a parameter: a pivot would give the CSV a variable header, which every
/// downstream tool has to special-case.
#[must_use]
pub fn trends_table(trends: &Trends, names: &BTreeMap<EntityRef, String>) -> Table {
    let mut t = Table::new(
        ExportDataset::Trends.as_str(),
        &[
            "week_start_ms",
            "stream_id",
            "stream_name",
            "completed",
            "deferred",
            "created",
        ],
    );
    for b in &trends.overall {
        t.push(vec![
            Cell::u64(b.week_start_ms),
            Cell::Null,
            Cell::text("(all)"),
            Cell::Int(b.completed),
            Cell::Int(b.deferred),
            Cell::Int(b.created),
        ]);
    }
    for line in &trends.per_stream {
        let name = names
            .get(&line.stream)
            .cloned()
            .unwrap_or_else(|| line.stream.to_str());
        for b in &line.weeks {
            t.push(vec![
                Cell::u64(b.week_start_ms),
                Cell::text(line.stream.to_str()),
                Cell::text(name.clone()),
                Cell::Int(b.completed),
                Cell::Int(b.deferred),
                Cell::Int(b.created),
            ]);
        }
    }
    t
}

/// Activity timeline table.
///
/// `detail` carries the one number or reference the verb needs — the field
/// count for an update, the destination for a move — so the table stays flat
/// without losing the payload.
#[must_use]
pub fn activity_table(events: &[ActivityEvent]) -> Table {
    let mut t = Table::new(
        ExportDataset::Activity.as_str(),
        &["at_ms", "entity_id", "label", "kind", "detail", "device"],
    );
    for e in events {
        let detail = match &e.kind {
            ActivityKind::TaskDeferred { count } => Cell::Int(*count),
            ActivityKind::TaskUpdated { fields } => Cell::u32(*fields),
            ActivityKind::TaskMoved { to, .. } => Cell::text(to.to_str()),
            ActivityKind::FocusStarted { planned_ms, .. } => Cell::opt_ms(*planned_ms),
            ActivityKind::FocusEnded { focused_ms, .. } => Cell::u64(*focused_ms),
            _ => Cell::Null,
        };
        t.push(vec![
            Cell::u64(e.at_ms),
            Cell::text(e.entity.to_str()),
            Cell::text(e.label.clone()),
            Cell::text(e.kind.verb()),
            detail,
            Cell::text(hex16(&e.device)),
        ]);
    }
    t
}

/// Per-Stream time-in-focus and estimate calibration, with a whole-vault row.
#[must_use]
pub fn focus_table(stats: &FocusStats, names: &BTreeMap<EntityRef, String>) -> Table {
    let mut t = Table::new(
        ExportDataset::Focus.as_str(),
        &[
            "stream_id",
            "stream_name",
            "sessions",
            "focused_ms",
            "calibration_factor",
            "calibration_samples",
        ],
    );
    t.push(vec![
        Cell::Null,
        Cell::text("(all)"),
        Cell::u32(stats.work_sessions),
        Cell::u64(stats.total_focused_ms),
        stats.overall.map_or(Cell::Null, |c| Cell::Float(c.factor)),
        stats.overall.map_or(Cell::Null, |c| Cell::u32(c.samples)),
    ]);
    for s in &stats.per_stream {
        let name = names
            .get(&s.stream)
            .cloned()
            .unwrap_or_else(|| s.stream.to_str());
        t.push(vec![
            Cell::text(s.stream.to_str()),
            Cell::text(name),
            Cell::u32(s.sessions),
            Cell::u64(s.focused_ms),
            s.calibration.map_or(Cell::Null, |c| Cell::Float(c.factor)),
            s.calibration.map_or(Cell::Null, |c| Cell::u32(c.samples)),
        ]);
    }
    t
}

/// Routine streak table.
#[must_use]
pub fn streaks_table(rows: &[StreakRow]) -> Table {
    let mut t = Table::new(
        ExportDataset::Streaks.as_str(),
        &["routine_id", "title", "streak", "last_completed_at_ms"],
    );
    for r in rows {
        t.push(vec![
            Cell::text(r.routine.to_str()),
            Cell::text(r.title.clone()),
            Cell::Int(r.streak),
            Cell::opt_ms(r.last_completed_at_ms),
        ]);
    }
    t
}

/// Lowercase hex of a 16-byte device id.
fn hex16(bytes: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Field content chosen to break a naive writer: every CSV metacharacter,
    /// a bare CR, leading/trailing space, and a C0 control.
    const HOSTILE: &str = " Ship \"v1\", then\r\nrest\ttoday\u{1}";

    fn hostile_table() -> Table {
        let mut t = Table::new("hostile", &["title", "count", "ratio", "done", "note"]);
        t.push(vec![
            Cell::text(HOSTILE),
            Cell::Int(-3),
            Cell::Float(1.5),
            Cell::Bool(true),
            Cell::Null,
        ]);
        t.push(vec![
            Cell::text("plain"),
            Cell::Int(0),
            Cell::Float(f64::NAN),
            Cell::Bool(false),
            Cell::text(""),
        ]);
        t
    }

    /// Minimal RFC 4180 reader, written independently of the writer, so the
    /// assertion is "this parses back to the same cells" rather than "this
    /// looks like the string I built".
    fn parse_csv(input: &str) -> Vec<Vec<String>> {
        let mut records = Vec::new();
        let mut record = Vec::new();
        let mut field = String::new();
        let mut in_quotes = false;
        let mut chars = input.chars().peekable();
        while let Some(c) = chars.next() {
            if in_quotes {
                if c == '"' {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        field.push('"');
                    } else {
                        in_quotes = false;
                    }
                } else {
                    field.push(c);
                }
                continue;
            }
            match c {
                '"' => in_quotes = true,
                ',' => record.push(std::mem::take(&mut field)),
                '\r' if chars.peek() == Some(&'\n') => {
                    chars.next();
                    record.push(std::mem::take(&mut field));
                    records.push(std::mem::take(&mut record));
                }
                c => field.push(c),
            }
        }
        if !field.is_empty() || !record.is_empty() {
            record.push(field);
            records.push(record);
        }
        records
    }

    #[test]
    fn csv_round_trips_hostile_field_content() {
        let table = hostile_table();
        let csv = table.to_csv();
        let parsed = parse_csv(&csv);

        assert_eq!(parsed.len(), 3, "header + two rows: {csv:?}");
        assert_eq!(
            parsed[0],
            vec!["title", "count", "ratio", "done", "note"],
            "header survives unquoted"
        );
        // The whole point: the comma, the quotes, the CRLF and the leading
        // space all come back in one field, and the row still has 5 columns.
        assert_eq!(parsed[1].len(), 5);
        assert_eq!(parsed[1][0], HOSTILE);
        assert_eq!(parsed[1][1], "-3");
        assert_eq!(parsed[1][2], "1.5");
        assert_eq!(parsed[1][3], "true");
        assert_eq!(parsed[1][4], "");
        assert_eq!(parsed[2], vec!["plain", "0", "", "false", ""]);
    }

    #[test]
    fn every_csv_record_has_the_header_width() {
        let csv = hostile_table().to_csv();
        let parsed = parse_csv(&csv);
        let width = parsed[0].len();
        for (i, r) in parsed.iter().enumerate() {
            assert_eq!(r.len(), width, "record {i} is ragged: {r:?}");
        }
    }

    #[test]
    fn csv_quotes_only_what_needs_quoting() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("with,comma"), "\"with,comma\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("line\nbreak"), "\"line\nbreak\"");
        assert_eq!(csv_field(" pad "), "\" pad \"");
        assert_eq!(csv_field(""), "");
    }

    #[test]
    fn short_and_long_rows_are_normalized_to_the_header() {
        let mut t = Table::new("t", &["a", "b", "c"]);
        t.push(vec![Cell::Int(1)]);
        t.push(vec![Cell::Int(1), Cell::Int(2), Cell::Int(3), Cell::Int(4)]);
        assert_eq!(t.rows[0], vec![Cell::Int(1), Cell::Null, Cell::Null]);
        assert_eq!(t.rows[1], vec![Cell::Int(1), Cell::Int(2), Cell::Int(3)]);
    }

    #[test]
    fn json_is_parseable_and_preserves_hostile_text_and_types() {
        let json = hostile_table().to_json();
        let v: serde_json::Value = serde_json::from_str(&json)
            .unwrap_or_else(|e| panic!("emitted invalid JSON ({e}): {json}"));

        assert_eq!(v["table"], "hostile");
        assert_eq!(
            v["columns"],
            serde_json::json!(["title", "count", "ratio", "done", "note"])
        );
        let rows = v["rows"].as_array().expect("rows array");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["title"], HOSTILE, "control chars round-trip");
        assert_eq!(rows[0]["count"], -3, "an integer stays a number");
        assert_eq!(rows[0]["ratio"], 1.5);
        assert_eq!(rows[0]["done"], true);
        assert!(rows[0]["note"].is_null());
        assert!(rows[1]["ratio"].is_null(), "NaN exports as null, not NaN");
    }

    #[test]
    fn json_escapes_backslashes_and_quotes() {
        let mut t = Table::new("t", &["s"]);
        t.push(vec![Cell::text(r#"C:\path "quoted""#)]);
        let json = t.to_json();
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(v["rows"][0]["s"], r#"C:\path "quoted""#);
    }

    #[test]
    fn empty_table_still_emits_a_header_and_an_empty_row_set() {
        let t = Table::new("empty", &["a", "b"]);
        assert_eq!(t.to_csv(), "a,b\r\n");
        let v: serde_json::Value = serde_json::from_str(&t.to_json()).expect("valid json");
        assert_eq!(v["rows"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn trends_table_emits_the_vault_line_then_one_row_per_stream_week() {
        use crate::stats::{StreamTrend, WeekBucket};
        use sunrise_id::EntityKind;

        let stream = EntityRef::new(EntityKind::Stream, [7u8; 16]);
        let trends = Trends {
            week_starts: vec![100, 200],
            overall: vec![
                WeekBucket {
                    week_start_ms: 100,
                    completed: 3,
                    deferred: 1,
                    created: 5,
                },
                WeekBucket {
                    week_start_ms: 200,
                    completed: -1,
                    deferred: 0,
                    created: 0,
                },
            ],
            per_stream: vec![StreamTrend {
                stream,
                weeks: vec![
                    WeekBucket {
                        week_start_ms: 100,
                        completed: 3,
                        deferred: 1,
                        created: 5,
                    },
                    WeekBucket {
                        week_start_ms: 200,
                        completed: -1,
                        deferred: 0,
                        created: 0,
                    },
                ],
            }],
        };
        // A stream name that would corrupt a naive CSV writer.
        let mut names = BTreeMap::new();
        names.insert(stream, "Work, \"Acme\"".to_string());

        let table = trends_table(&trends, &names);
        let parsed = parse_csv(&table.to_csv());
        assert_eq!(parsed.len(), 5, "header + 2 vault rows + 2 stream rows");
        assert_eq!(parsed[1], vec!["100", "", "(all)", "3", "1", "5"]);
        assert_eq!(
            parsed[2][3], "-1",
            "a negative week survives the round trip"
        );
        assert_eq!(parsed[3][2], "Work, \"Acme\"");
        assert_eq!(parsed[3][1], stream.to_str());
    }

    #[test]
    fn activity_table_puts_each_verbs_payload_in_the_detail_column() {
        use crate::activity::ActivityKind;
        use sunrise_id::EntityKind;

        let task = EntityRef::new(EntityKind::Task, [1u8; 16]);
        let dest = EntityRef::new(EntityKind::Stream, [2u8; 16]);
        let session = EntityRef::new(EntityKind::FocusSession, [3u8; 16]);
        let ev = |kind: ActivityKind| ActivityEvent {
            at_ms: 1_000,
            op_id: [9u8; 16],
            device: [0xAB; 16],
            entity: task,
            label: "a\nmultiline title".into(),
            kind,
        };
        let events = vec![
            ev(ActivityKind::TaskCreated),
            ev(ActivityKind::TaskUpdated { fields: 4 }),
            ev(ActivityKind::TaskDeferred { count: 2 }),
            ev(ActivityKind::TaskMoved {
                from: dest,
                to: dest,
            }),
            ev(ActivityKind::FocusEnded {
                session,
                focused_ms: 1_500_000,
                completed_task: true,
            }),
        ];
        let parsed = parse_csv(&activity_table(&events).to_csv());
        assert_eq!(parsed.len(), 6);
        let details: Vec<&str> = parsed[1..].iter().map(|r| r[4].as_str()).collect();
        assert_eq!(
            details,
            vec!["", "4", "2", dest.to_str().as_str(), "1500000"]
        );
        assert_eq!(
            parsed[1][2], "a\nmultiline title",
            "the newline stays inside its field"
        );
        assert_eq!(parsed[1][5], "ab".repeat(16), "device id as lowercase hex");
    }

    #[test]
    fn focus_and_streak_tables_carry_optional_values_as_empty_fields() {
        use crate::focus::{Calibration, StreamFocus};
        use crate::streak::StreakRow;
        use sunrise_id::EntityKind;

        let stream = EntityRef::new(EntityKind::Stream, [7u8; 16]);
        let stats = FocusStats {
            sessions: 3,
            work_sessions: 2,
            running: 0,
            total_focused_ms: 60_000,
            interruptions: 0,
            per_stream: vec![StreamFocus {
                stream,
                sessions: 2,
                focused_ms: 60_000,
                calibration: None,
            }],
            per_energy: Vec::new(),
            overall: Some(Calibration {
                factor: 1.7,
                samples: 3,
                estimated_ms: 1_000,
                actual_ms: 1_700,
            }),
            top_interruptions: Vec::new(),
        };
        let parsed = parse_csv(&focus_table(&stats, &BTreeMap::new()).to_csv());
        assert_eq!(parsed[1], vec!["", "(all)", "2", "60000", "1.7", "3"]);
        assert_eq!(
            parsed[2],
            vec![
                stream.to_str(),
                stream.to_str(),
                "2".to_string(),
                "60000".to_string(),
                String::new(),
                String::new()
            ],
            "an uncalibrated stream exports empty, never 0"
        );

        let rows = vec![StreakRow {
            routine: EntityRef::new(EntityKind::Routine, [4u8; 16]),
            title: "Stretch".into(),
            streak: 12,
            last_completed_at_ms: None,
        }];
        let parsed = parse_csv(&streaks_table(&rows).to_csv());
        assert_eq!(parsed[1][2], "12");
        assert_eq!(parsed[1][3], "");
    }

    #[test]
    fn dataset_names_round_trip() {
        for d in [
            ExportDataset::Trends,
            ExportDataset::Activity,
            ExportDataset::Focus,
            ExportDataset::Streaks,
        ] {
            assert_eq!(ExportDataset::parse(d.as_str()), Some(d));
        }
        assert_eq!(ExportDataset::parse("everything"), None);
    }

    #[test]
    fn format_names_round_trip() {
        for f in [ExportFormat::Csv, ExportFormat::Json] {
            assert_eq!(ExportFormat::parse(f.as_str()), Some(f));
        }
        assert_eq!(ExportFormat::parse("xml"), None);
    }
}
