//! `sunrise conflicts` and `sunrise restore`: the writes the vault keeps
//! beside each field's winner, and the commands that settle them
//! (ADR-0065 §4).
//!
//! A conflict is a field two devices wrote without either having seen the
//! other's write. The vault shows one of them, by the field's rule, and keeps
//! the other; `conflicts` lists both, and `conflicts resolve` writes the one
//! to keep, which clears the conflict on every device that syncs it.

use serde_json::{json, Value};
use sunrise_cbor::CborValue;
use sunrise_cli::i18n::strings;
use sunrise_core::{
    Command, ConflictClass, ConflictWrite, EntityConflict, Query, QueryResult, VersionRef,
    WriteOrigin,
};
use sunrise_id::EntityRef;

use super::{flag_value, has, hex16, positionals, CmdResult, Ctx};

/// An id of any kind, or the general "not an id" error.
fn any_id(raw: &str) -> CmdResult<EntityRef> {
    EntityRef::parse_any(raw).map_err(|e| strings::general::not_an_id(raw, &e.to_string()).into())
}

/// `restore <id>...` — bring back deleted entities.
pub(super) async fn restore(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    if rest.is_empty() {
        return Err(strings::restore::needs_id().into());
    }
    for raw in rest {
        let id = any_id(raw)?;
        ctx.submit(Command::Restore(id), strings::undo::restore(raw))
            .await?;
        println!("restored  {raw}");
    }
    Ok(())
}

/// `conflicts [list] [<id>] [--all]` and `conflicts resolve ...`.
pub(super) async fn conflicts(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    match rest.first().map(String::as_str) {
        Some("resolve") => resolve(ctx, &rest[1..]).await,
        Some("list") => list(ctx, &rest[1..]).await,
        Some(verb) if !verb.starts_with("--") && any_id(verb).is_err() => {
            Err(strings::conflicts::unknown_verb(verb).into())
        }
        _ => list(ctx, rest).await,
    }
}

async fn query(
    ctx: &Ctx<'_>,
    entity: Option<EntityRef>,
    include_quiet: bool,
) -> CmdResult<Vec<EntityConflict>> {
    match ctx
        .core
        .query(Query::Conflicts {
            entity,
            include_quiet,
        })
        .await?
    {
        QueryResult::Conflicts(c) => Ok(c),
        _ => Err(strings::general::unexpected_result().into()),
    }
}

async fn list(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let words = positionals(rest, &[], &["--all"]);
    let entity = match words.as_slice() {
        [] => None,
        [raw] => Some(any_id(raw)?),
        [_, extra, ..] => return Err(strings::general::unexpected_argument(extra).into()),
    };
    let found = query(ctx, entity, has(rest, "--all")).await?;
    if ctx.json {
        sunrise_cli::json::print(&Value::Array(found.iter().map(conflict_json).collect()));
        return Ok(());
    }
    if found.is_empty() {
        println!("{}", strings::conflicts::none());
        return Ok(());
    }
    for c in &found {
        let class = serde_json::to_value(c.class).unwrap_or(Value::Null);
        println!(
            "{}  {}  ({})",
            c.entity.to_str(),
            c.field,
            class.as_str().unwrap_or("?")
        );
        if c.class == ConflictClass::DependencyCycle {
            // The winner is the edge the rule sets aside: kept, not shown.
            println!("  set aside  {}", write_line(&c.winner));
            let names: Vec<String> = c.cycle.iter().map(EntityRef::to_str).collect();
            println!("  cycle  {}", names.join(" -> "));
            continue;
        }
        println!("  shown  {}", write_line(&c.winner));
        for l in &c.losers {
            println!("  kept   {}", write_line(l));
        }
        for e in &c.edited_while_deleted {
            println!(
                "  edited while deleted  {}  by {}  ({})",
                short(&e.version),
                &hex16(&e.device)[..8],
                e.fields.join(", ")
            );
        }
    }
    Ok(())
}

/// A version as the table prints it and `--pick` reads it back: the
/// writing device's first eight hex digits and its `seq`.
fn short(v: &VersionRef) -> String {
    format!("{}/{}", &hex16(&v.device)[..8], v.seq)
}

const fn origin(o: WriteOrigin) -> &'static str {
    match o {
        WriteOrigin::User => "user",
        WriteOrigin::Generated => "generated",
    }
}

/// A written value as JSON: CBOR text, numbers, booleans, arrays and maps as
/// themselves, and `null` for a write that left the field unset.
fn value_json(v: Option<&CborValue>) -> Value {
    v.and_then(|v| serde_json::to_value(v).ok())
        .unwrap_or(Value::Null)
}

fn write_line(w: &ConflictWrite) -> String {
    let value = match value_json(w.value.as_ref()) {
        Value::Null => "(unset)".to_owned(),
        v => v.to_string(),
    };
    let generated = if w.origin == WriteOrigin::Generated {
        "  (generated)"
    } else {
        ""
    };
    format!("{value}  {}{generated}", short(&w.version))
}

fn write_json(w: &ConflictWrite) -> Value {
    json!({
        "version": w.version.to_string(),
        "device": hex16(&w.device),
        "origin": origin(w.origin),
        "at_ms": w.hlc_ms,
        "value": value_json(w.value.as_ref()),
    })
}

/// One conflict, as `conflicts --json` prints it
/// (`docs/07-clients/overview.md` §JSON output).
fn conflict_json(c: &EntityConflict) -> Value {
    json!({
        "entity": c.entity.to_str(),
        "field": c.field,
        "class": serde_json::to_value(c.class).unwrap_or(Value::Null),
        "winner": write_json(&c.winner),
        "losers": c.losers.iter().map(write_json).collect::<Vec<_>>(),
        "edited_while_deleted": c.edited_while_deleted.iter().map(|e| json!({
            "version": e.version.to_string(),
            "device": hex16(&e.device),
            "fields": e.fields,
        })).collect::<Vec<_>>(),
        "cycle": c.cycle.iter().map(EntityRef::to_str).collect::<Vec<_>>(),
    })
}

/// The write of `c` that `pick` names: its whole version, or the short form
/// the table prints.
fn picked<'a>(c: &'a EntityConflict, pick: &str) -> CmdResult<&'a ConflictWrite> {
    let mut matches = std::iter::once(&c.winner)
        .chain(&c.losers)
        .filter(|w| w.version.to_string() == pick || short(&w.version) == pick);
    let Some(first) = matches.next() else {
        return Err(strings::conflicts::no_such_version(pick).into());
    };
    if matches.next().is_some() {
        return Err(strings::conflicts::ambiguous_version(pick).into());
    }
    Ok(first)
}

/// `--value`: JSON when it reads as JSON, and plain text otherwise, so a
/// title needs no quoting and a priority is `3`.
fn value_from(raw: &str) -> CmdResult<CborValue> {
    let v = serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.to_owned()));
    serde_json::from_value(v).map_err(|e| strings::conflicts::bad_value(&e.to_string()).into())
}

async fn resolve(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let words = positionals(rest, &["--pick", "--value"], &[]);
    let [raw, field] = words.as_slice() else {
        return Err(strings::conflicts::resolve_usage().into());
    };
    let entity = any_id(raw)?;
    let value = match (flag_value(rest, "--pick"), flag_value(rest, "--value")) {
        (Some(pick), None) => {
            let found = query(ctx, Some(entity), true).await?;
            let Some(c) = found.iter().find(|c| &c.field == field) else {
                return Err(strings::conflicts::no_such_conflict(raw, field).into());
            };
            match &picked(c, &pick)?.value {
                Some(v) => v.clone(),
                None => value_from("null")?,
            }
        }
        (None, Some(raw_value)) => value_from(&raw_value)?,
        _ => return Err(strings::conflicts::resolve_usage().into()),
    };
    ctx.submit(
        Command::ResolveConflict {
            entity,
            field: field.clone(),
            value,
        },
        strings::undo::resolve(field, raw),
    )
    .await?;
    println!("resolved  {raw}  {field}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_is_json_when_it_reads_as_json_and_text_otherwise() {
        let text = |s: &str| serde_json::to_value(value_from(s).unwrap()).unwrap();
        assert_eq!(text("Renew passport"), json!("Renew passport"));
        assert_eq!(text("\"3\""), json!("3"));
        assert_eq!(text("3"), json!(3));
        assert_eq!(text("true"), json!(true));
        assert_eq!(text("null"), Value::Null);
        assert!(
            value_from("1.5").is_err(),
            "a float is not canonical Sunrise CBOR"
        );
    }

    #[test]
    fn a_pick_names_one_write_by_its_whole_or_short_version() {
        let write = |device: u8, seq| ConflictWrite {
            version: VersionRef {
                stream: [1; 16],
                device: [device; 16],
                seq,
            },
            hlc_ms: 0,
            hlc_logical: 0,
            device: [device; 16],
            origin: WriteOrigin::User,
            value: Some(value_from(&format!("v{seq}")).unwrap()),
        };
        let c = EntityConflict {
            entity: EntityRef::new(sunrise_id::EntityKind::Task, [3; 16]),
            field: "title".into(),
            class: sunrise_core::ConflictClass::Tracked,
            winner: write(0xaa, 2),
            losers: vec![write(0xbb, 1), write(0xbb, 3)],
            edited_while_deleted: Vec::new(),
            cycle: Vec::new(),
        };
        assert_eq!(picked(&c, "bbbbbbbb/3").unwrap().version.seq, 3);
        // A dependency cycle prints its class and its loop.
        let (t1, t2) = (
            EntityRef::new(sunrise_id::EntityKind::Task, [1; 16]),
            EntityRef::new(sunrise_id::EntityKind::Task, [2; 16]),
        );
        let cycle = EntityConflict {
            entity: t2,
            field: "blocked_by".into(),
            class: ConflictClass::DependencyCycle,
            winner: write(0xaa, 4),
            losers: Vec::new(),
            edited_while_deleted: Vec::new(),
            cycle: vec![t2, t1, t2],
        };
        let j = conflict_json(&cycle);
        assert_eq!(j["class"], "dependency_cycle");
        assert_eq!(j["cycle"], json!([t2.to_str(), t1.to_str(), t2.to_str()]));
        assert_eq!(conflict_json(&c)["cycle"], json!([]));
        let whole = c.winner.version.to_string();
        assert_eq!(picked(&c, &whole).unwrap().version.seq, 2);
        assert!(picked(&c, "bbbbbbbb/9").is_err());
    }
}
