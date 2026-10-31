use super::table::csv_field;
use super::*;
use crate::activity::ActivityEvent;
use crate::focus::FocusStats;
use crate::stats::Trends;
use std::collections::BTreeMap;
use sunrise_id::EntityRef;

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
            from: Some(dest),
            to: Some(dest),
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
        unknown: crate::Unknowns::new(),
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
