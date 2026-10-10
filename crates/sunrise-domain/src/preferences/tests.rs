use super::*;
use std::collections::BTreeSet;

fn cbor(v: Value) -> CborValue {
    CborValue(v)
}

fn spec(key: &str) -> &'static PrefSpec {
    pref_spec(key).expect("known key")
}

#[test]
fn the_two_decided_defaults_are_pinned() {
    // ADR-0050 §2 decides exactly these two.
    let none = BTreeMap::new();
    let all = resolve_all(&none, &none, DeviceClass::Desktop);
    let get = |k: &str| all.iter().find(|r| r.key == k).expect(k).clone();
    assert_eq!(
        get("week_start").value,
        Some(PrefValue::Weekday(Weekday::Su))
    );
    assert_eq!(
        get("notifications.timezone_changed.enabled").value,
        Some(PrefValue::Bool(false))
    );
    assert!(all.iter().all(|r| r.source == PrefSource::Default));
}

#[test]
fn every_default_fits_its_own_type() {
    for s in PREFERENCE_KEYS {
        for class in [DeviceClass::Desktop, DeviceClass::Handheld] {
            if let Some(v) = s.default.value(class) {
                assert!(s.ty.fits(&v), "{}'s default does not fit", s.key);
                assert_eq!(s.ty.decode(&v.encode()), Some(v), "{} round trip", s.key);
            } else {
                assert_eq!(s.default, PrefDefault::Absent, "{}", s.key);
            }
        }
    }
}

#[test]
fn keys_are_unique_well_formed_and_bootstrap_keys_are_device_text() {
    let mut seen = BTreeSet::new();
    for s in PREFERENCE_KEYS {
        assert!(is_pref_key(s.key), "{}", s.key);
        assert!(seen.insert(s.key), "{} twice", s.key);
        if s.bootstrap {
            assert_eq!(s.scope, PrefScope::Device, "{}", s.key);
            assert!(
                matches!(s.ty, PrefType::Url | PrefType::Text),
                "{} is stored as text in the bootstrap file",
                s.key
            );
        }
    }
}

/// The backticked tokens of one markdown table cell, in order.
fn ticked(cell: &str) -> Vec<&str> {
    cell.split('`').skip(1).step_by(2).collect()
}

fn scope_named(s: &str) -> Option<PrefScope> {
    [
        PrefScope::Vault,
        PrefScope::VaultOverridable,
        PrefScope::Device,
    ]
    .into_iter()
    .find(|scope| scope.as_str() == s)
}

/// Every key the two normative pages list, with its scope.
fn documented_keys() -> BTreeMap<String, PrefScope> {
    let docs = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/");
    let read = |p: &str| std::fs::read_to_string(format!("{docs}{p}")).expect(p);
    let rows = |text: &str| -> Vec<Vec<String>> {
        text.lines()
            .filter(|l| l.starts_with("| `"))
            .map(|l| l.split('|').skip(1).map(|c| c.trim().to_owned()).collect())
            .collect()
    };
    let mut out = BTreeMap::new();
    for cells in rows(&read("02-domain/preferences.md")) {
        let (Some(key), Some(scope)) = (
            ticked(&cells[0]).first().copied(),
            cells
                .get(3)
                .and_then(|c| ticked(c).first().copied())
                .and_then(scope_named),
        ) else {
            continue;
        };
        if is_pref_key(key) && cells.len() >= 5 {
            out.insert(key.to_owned(), scope);
        }
    }
    let notifications = read("08-features/notifications.md");
    let catalog = notifications
        .split("## Preference keys")
        .nth(1)
        .and_then(|s| s.split("\n## ").next())
        .expect("notifications.md §Preference keys");
    for cells in rows(catalog) {
        let name = ticked(&cells[0])[0];
        if name.starts_with("notifications.") {
            let scope = scope_named(ticked(&cells[3])[0]).expect(name);
            out.insert(name.to_owned(), scope);
            continue;
        }
        let mut fields = vec![".enabled"];
        fields.extend(ticked(&cells[2]).into_iter().filter(|t| t.starts_with('.')));
        for field in fields {
            let parts: Vec<&str> = cells[3].split(';').collect();
            let scope = if parts.len() == 1 {
                ticked(parts[0])[0]
            } else {
                let part = parts
                    .iter()
                    .find(|p| ticked(p).first() == Some(&field))
                    .unwrap_or_else(|| panic!("no scope for {name}{field}"));
                ticked(part)[1]
            };
            out.insert(
                format!("notifications.{name}{field}"),
                scope_named(scope).expect(scope),
            );
        }
    }
    out
}

/// `preferences.md` and `notifications.md` §Preference keys are the list of
/// record; this table is their Rust form and must name exactly the same keys
/// with the same scopes. `day_schedule` is documented ahead of its key (#338).
#[test]
fn the_key_table_is_the_documented_one() {
    let mut documented = documented_keys();
    assert!(documented.remove("day_schedule").is_some());
    let table: BTreeMap<String, PrefScope> = PREFERENCE_KEYS
        .iter()
        .map(|s| (s.key.to_owned(), s.scope))
        .collect();
    assert_eq!(table, documented);
}

#[test]
fn key_grammar_follows_the_cddl() {
    for ok in [
        "week_start",
        "review.cadence",
        "day_schedule.weekday.MO",
        "day_schedule.date.2026-12-25",
        "day_schedule.month_day.-1",
    ] {
        assert!(is_pref_key(ok), "{ok}");
    }
    for bad in ["", "Week", "1x", "a..b", "a.", ".a", "a.b c", "_a"] {
        assert!(!is_pref_key(bad), "{bad}");
    }
}

/// The table-driven test across all three scopes the issue asks for.
#[test]
fn the_resolver_reads_overlay_then_vault_then_default_as_the_scope_allows() {
    use PrefSource::{Default as D, Overlay as O, Vault as V};
    let t = |s: &str| Some(cbor(Value::Text(s.into())));
    let b = |v: bool| Some(cbor(Value::Bool(v)));
    let n = |v: u64| Some(cbor(Value::Integer(v.into())));
    let day = |d: Weekday| Some(PrefValue::Weekday(d));
    let txt = |s: &str| Some(PrefValue::Text(s.into()));
    let bool_ = |v: bool| Some(PrefValue::Bool(v));
    // (key, overlay, vault, expected value, expected source)
    let cases = [
        // vault: the overlay is never read.
        ("week_start", None, None, day(Weekday::Su), D),
        ("week_start", None, t("MO"), day(Weekday::Mo), V),
        ("week_start", t("TU"), None, day(Weekday::Su), D),
        ("week_start", t("TU"), t("MO"), day(Weekday::Mo), V),
        ("week_start", None, t("XX"), day(Weekday::Su), D),
        // vault_overridable: overlay, then vault, then default.
        ("time_format", None, None, txt("locale"), D),
        ("time_format", None, t("h24"), txt("h24"), V),
        ("time_format", t("h12"), t("h24"), txt("h12"), O),
        ("time_format", t("h12"), None, txt("h12"), O),
        ("time_format", t("h36"), t("h24"), txt("h24"), V),
        ("time_format", t("h36"), t("h48"), txt("locale"), D),
        // device: the vault is never read.
        ("keyboard.vim_mode", None, None, bool_(false), D),
        ("keyboard.vim_mode", b(true), None, bool_(true), O),
        ("keyboard.vim_mode", None, b(true), bool_(false), D),
        ("keyboard.vim_mode", t("yes"), None, bool_(false), D),
        // An absent default stays absent.
        ("home_timezone", None, None, None, D),
        (
            "home_timezone",
            None,
            t("Europe/Paris"),
            txt("Europe/Paris"),
            V,
        ),
        ("home_timezone", None, t("Mars/Olympus"), None, D),
        // Out of range from a peer: preserved by the store, read as the default.
        (
            "stale_after_days",
            None,
            n(400),
            Some(PrefValue::Uint(14)),
            D,
        ),
        ("stale_after_days", None, n(0), Some(PrefValue::Uint(0)), V),
    ];
    for (key, overlay, vault, value, source) in cases {
        let r = resolve(
            spec(key),
            overlay.as_ref(),
            vault.as_ref(),
            DeviceClass::Desktop,
        );
        assert_eq!(
            (r.value.clone(), r.source),
            (value, source),
            "{key} {overlay:?} {vault:?}"
        );
    }
}

#[test]
fn a_class_dependent_default_follows_the_device() {
    let s = spec("attachments.cache_limit_bytes");
    assert_eq!(
        resolve(s, None, None, DeviceClass::Desktop).value,
        Some(PrefValue::Uint(1_000_000_000))
    );
    assert_eq!(
        resolve(s, None, None, DeviceClass::Handheld).value,
        Some(PrefValue::Uint(200_000_000))
    );
    assert_eq!(DeviceClass::of_platform("ios"), DeviceClass::Handheld);
    assert_eq!(DeviceClass::of_platform("macos"), DeviceClass::Desktop);
}

#[test]
fn structured_values_round_trip_through_their_one_cbor_form() {
    let cadence = PrefValue::Cadence {
        day: Weekday::Mo,
        at: civil::time(9, 30, 0, 0),
    };
    assert_eq!(PrefType::Cadence.decode(&cadence.encode()), Some(cadence));
    let window = PrefValue::TimeWindow {
        start: civil::time(22, 0, 0, 0),
        end: civil::time(7, 0, 0, 0),
    };
    assert_eq!(PrefType::TimeWindow.decode(&window.encode()), Some(window));
    let empty = PrefValue::TimeWindow {
        start: civil::time(7, 0, 0, 0),
        end: civil::time(7, 0, 0, 0),
    };
    assert!(
        !PrefType::TimeWindow.fits(&empty),
        "start MUST NOT equal end"
    );
}

#[test]
fn writes_are_checked_against_scope_type_and_range() {
    let field = |r: Result<&PrefSpec, ValidationError>| match r {
        Err(ValidationError::Field { constraint, .. }) => Some(constraint),
        _ => None,
    };
    let v = |n: u64| PrefValue::Uint(n);
    let mo = PrefValue::Weekday(Weekday::Mo);
    assert_eq!(
        check_write("week_start", Some(&mo), PrefTarget::Device),
        Err(ValidationError::PreferenceScope)
    );
    let on = PrefValue::Bool(true);
    assert_eq!(
        check_write("keyboard.vim_mode", Some(&on), PrefTarget::Vault),
        Err(ValidationError::PreferenceScope)
    );
    assert!(check_write("time_format", None, PrefTarget::Device).is_ok());
    assert!(check_write("time_format", None, PrefTarget::Vault).is_ok());
    assert!(check_write("stale_after_days", Some(&v(365)), PrefTarget::Vault).is_ok());
    let too_long = check_write("stale_after_days", Some(&v(366)), PrefTarget::Vault);
    assert_eq!(field(too_long), Some("type"));
    let unknown = check_write("no.such.key", None, PrefTarget::Vault);
    assert_eq!(field(unknown), Some("unknown"));
    let bootstrap = check_write("sync.relay_url", None, PrefTarget::Device);
    assert_eq!(field(bootstrap), Some("bootstrap"));
    let task = PrefValue::Text("tsk_00000000000000000000000000".into());
    let wrong_kind = check_write("capture_default_stream", Some(&task), PrefTarget::Vault);
    assert_eq!(field(wrong_kind), Some("type"));
}

#[test]
fn urls_and_language_tags_are_checked() {
    let fits = |ty: PrefType, s: &str| ty.fits(&PrefValue::Text(s.into()));
    for ok in [
        "https://relay.example",
        "wss://r.example/sync",
        "http://127.0.0.1:8080",
    ] {
        assert!(fits(PrefType::Url, ok), "{ok}");
    }
    for bad in [
        "",
        "relay.example",
        "https://",
        "https:///x",
        "ftp://x",
        "https://a b",
    ] {
        assert!(!fits(PrefType::Url, bad), "{bad}");
    }
    for ok in ["en", "en-GB", "zh-Hant-TW"] {
        assert!(fits(PrefType::LanguageTag, ok), "{ok}");
    }
    for bad in ["", "e", "en_GB", "en--GB"] {
        assert!(!fits(PrefType::LanguageTag, bad), "{bad}");
    }
}
