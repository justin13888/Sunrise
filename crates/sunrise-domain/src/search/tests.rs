use super::*;
use crate::rrule::Weekday;
use jiff::civil::date;

fn term(s: &str) -> Expr {
    Expr::Term(s.to_owned())
}

fn phrase(s: &str) -> Expr {
    Expr::Phrase(s.to_owned())
}

fn filter(f: Filter) -> Expr {
    Expr::Filter(f)
}

fn clauses(s: &str) -> Vec<Expr> {
    parse(s).clauses
}

fn kinds(s: &str) -> Vec<NoticeKind> {
    parse_with_notices(s)
        .notices
        .into_iter()
        .map(|n| n.kind)
        .collect()
}

/// Every query below reads back from its canonical text as itself.
fn round_trips(s: &str) -> String {
    let q = parse(s);
    let canonical = q.to_string();
    assert_eq!(parse(&canonical), q, "{s:?} → {canonical:?}");
    canonical
}

#[test]
fn words_are_anded_and_whitespace_is_ignored() {
    assert_eq!(
        clauses("  deploy   prod "),
        vec![term("deploy"), term("prod")]
    );
    assert!(parse("").is_empty());
    assert!(parse(" \t\n ").is_empty());
}

#[test]
fn a_phrase_is_quoted_and_trimmed() {
    assert_eq!(
        clauses(r#""quarterly report" x"#),
        vec![phrase("quarterly report"), term("x")]
    );
    assert_eq!(clauses(r#"" padded ""#), vec![phrase("padded")]);
    assert_eq!(kinds(r#""""#), vec![NoticeKind::EmptyPhrase]);
}

#[test]
fn an_unclosed_quote_runs_to_the_end() {
    let p = parse_with_notices(r#"a "open phrase"#);
    assert_eq!(p.query.clauses, vec![term("a"), phrase("open phrase")]);
    assert_eq!(p.notices[0].kind, NoticeKind::UnclosedQuote);
    assert_eq!(round_trips(r#"a "open phrase"#), r#"a "open phrase""#);
}

#[test]
fn or_binds_tighter_than_and() {
    assert_eq!(
        clauses("a b OR c"),
        vec![term("a"), Expr::Or(vec![term("b"), term("c")])]
    );
    assert_eq!(
        clauses("a OR b OR c"),
        vec![Expr::Or(vec![term("a"), term("b"), term("c")])]
    );
}

#[test]
fn or_is_an_operator_only_in_capitals_and_between_two_sides() {
    assert_eq!(clauses("a or b"), vec![term("a"), term("or"), term("b")]);
    assert_eq!(clauses("a OR"), vec![term("a"), phrase("OR")]);
    assert_eq!(clauses("OR a"), vec![phrase("OR"), term("a")]);
    assert!(kinds("a OR").contains(&NoticeKind::LiteralOr));
    assert_eq!(round_trips("a OR"), r#"a "OR""#);
    round_trips("a OR OR b");
    round_trips("(a OR) b");
}

#[test]
fn minus_negates_the_atom_after_it() {
    assert_eq!(
        clauses(r#"-draft -"exact phrase" -@waiting"#),
        vec![
            Expr::Not(Box::new(term("draft"))),
            Expr::Not(Box::new(phrase("exact phrase"))),
            Expr::Not(Box::new(filter(Filter::Context(vec!["waiting".into()])))),
        ]
    );
    // A minus with nothing after it is a word, and two cancel.
    assert_eq!(clauses("- x"), vec![term("-"), term("x")]);
    assert_eq!(clauses("--x"), vec![term("x")]);
    assert_eq!(clauses("a-b"), vec![term("a-b")]);
}

#[test]
fn groups_nest_and_an_empty_one_vanishes() {
    assert_eq!(
        clauses("(a OR b) c"),
        vec![
            Expr::Group(vec![Expr::Or(vec![term("a"), term("b")])]),
            term("c")
        ]
    );
    assert_eq!(clauses("() x"), vec![term("x")]);
    assert_eq!(kinds("(a"), vec![NoticeKind::UnclosedGroup]);
    assert_eq!(round_trips("(a"), "(a)");
    let stray = parse_with_notices("a) b");
    assert_eq!(stray.query.clauses, vec![term("a"), term("b")]);
    assert_eq!(stray.notices[0].kind, NoticeKind::StrayParen);
}

#[test]
fn shorthands_are_filters() {
    assert_eq!(
        clauses("#work @errands !1"),
        vec![
            filter(Filter::Stream(vec!["work".into()])),
            filter(Filter::Context(vec!["errands".into()])),
            filter(Filter::Priority(Cmp::Eq, vec![1])),
        ]
    );
    assert_eq!(
        clauses(r#"#"Side project""#),
        vec![filter(Filter::Stream(vec!["Side project".into()]))]
    );
    // Not shorthands: a bare sigil, a priority off the scale.
    assert_eq!(
        clauses("# !9 !12 @"),
        vec![term("#"), term("!9"), term("!12"), term("@")]
    );
}

#[test]
fn keyed_filters_read_their_values() {
    assert_eq!(
        clauses(r#"stream:"Side project" context:home,errands state:open,done kind:note,task"#),
        vec![
            filter(Filter::Stream(vec!["Side project".into()])),
            filter(Filter::Context(vec!["home".into(), "errands".into()])),
            filter(Filter::State(vec![StateValue::Open, StateValue::Done])),
            filter(Filter::Kind(vec![SearchKind::Note, SearchKind::Task])),
        ]
    );
    assert_eq!(
        clauses("is:late has:attachment priority<=2 place:office"),
        vec![
            filter(Filter::Is(vec![IsValue::Late])),
            filter(Filter::Has(vec![HasValue::Attachment])),
            filter(Filter::Priority(Cmp::Le, vec![2])),
            filter(Filter::Place(vec!["office".into()])),
        ]
    );
    // Keys and keyword values are case-insensitive; names keep their case.
    assert_eq!(
        clauses("STATE:Done Stream:Work"),
        vec![
            filter(Filter::State(vec![StateValue::Done])),
            filter(Filter::Stream(vec!["Work".into()])),
        ]
    );
}

#[test]
fn date_filters_read_every_value_form() {
    let cases = [
        (
            "planned:today",
            DateField::Planned,
            Cmp::Eq,
            DateSpan::One(DateValue::Today),
        ),
        (
            "due<friday",
            DateField::Due,
            Cmp::Lt,
            DateSpan::One(DateValue::Weekday(DayOfWeek::Fri)),
        ),
        (
            "hard<+3d",
            DateField::Hard,
            Cmp::Lt,
            DateSpan::One(DateValue::Offset {
                n: 3,
                unit: OffsetUnit::Days,
            }),
        ),
        (
            "done:last-week",
            DateField::Done,
            Cmp::Eq,
            DateSpan::One(DateValue::LastWeek),
        ),
        (
            "created>=2026-10",
            DateField::Created,
            Cmp::Ge,
            DateSpan::One(DateValue::Month {
                year: 2026,
                month: 10,
            }),
        ),
        (
            "target:2026-10-01..2026-10-07",
            DateField::Target,
            Cmp::Eq,
            DateSpan::Range(
                DateValue::Day(date(2026, 10, 1)),
                DateValue::Day(date(2026, 10, 7)),
            ),
        ),
    ];
    for (s, field, cmp, span) in cases {
        assert_eq!(
            clauses(s),
            vec![filter(Filter::Date(field, cmp, span))],
            "{s}"
        );
        round_trips(s);
    }
}

#[test]
fn an_operator_it_cannot_read_is_text_with_a_notice() {
    for (s, kind) in [
        ("colour:red", NoticeKind::UnknownKey),
        ("state:sleeping", NoticeKind::BadValue),
        ("priority:9", NoticeKind::BadValue),
        ("priority<1,2", NoticeKind::BadValue),
        ("stream<work", NoticeKind::BadValue),
        ("due:someday", NoticeKind::BadValue),
        ("due<today..fri", NoticeKind::BadValue),
        ("stream:", NoticeKind::BadValue),
        ("stream:a,", NoticeKind::BadValue),
        ("kind:spaceship", NoticeKind::BadValue),
    ] {
        let p = parse_with_notices(s);
        assert_eq!(p.query.clauses, vec![term(s)], "{s}");
        assert_eq!(p.notices.len(), 1, "{s}");
        assert_eq!(p.notices[0].kind, kind, "{s}");
        round_trips(s);
    }
    // A bad value with an open quote closes it, so it reads back the same.
    assert_eq!(
        clauses(r#"(state:"nope"#),
        vec![Expr::Group(vec![term(r#"state:"nope""#)])]
    );
    round_trips(r#"(state:"nope"#);
}

#[test]
fn the_canonical_text_uses_shorthands_and_lowercase_keywords() {
    assert_eq!(
        round_trips("Stream:work  Context:home   priority:1"),
        "#work @home !1"
    );
    assert_eq!(
        round_trips(r#"stream:"a,b" context:"x y""#),
        r#"#a,b @"x y""#
    );
    assert_eq!(round_trips(r#"stream:"a b",c"#), r#"stream:"a b",c"#);
    assert_eq!(round_trips("due<=MONDAY"), "due<=mon");
    assert_eq!(round_trips("planned:-0d"), "planned:+0d");
    assert_eq!(round_trips("state:\"done\""), "state:done");
    assert_eq!(round_trips("a(b)c"), "a (b) c");
}

#[test]
fn past_the_clause_limit_the_rest_is_one_phrase() {
    let words: Vec<String> = (0..40).map(|i| format!("w{i}")).collect();
    let p = parse_with_notices(&words.join(" "));
    assert_eq!(p.query.clauses.len(), MAX_CLAUSES + 1);
    let rest: Vec<&str> = words[MAX_CLAUSES..].iter().map(String::as_str).collect();
    assert_eq!(p.query.clauses[MAX_CLAUSES], phrase(&rest.join(" ")));
    assert!(p
        .notices
        .iter()
        .any(|n| n.kind == NoticeKind::TooManyClauses));
    round_trips(&words.join(" "));
}

#[test]
fn past_the_depth_limit_the_rest_is_one_phrase() {
    let deep = format!("{}x{}", "(".repeat(10), ")".repeat(10));
    let p = parse_with_notices(&deep);
    assert!(p.notices.iter().any(|n| n.kind == NoticeKind::TooDeep));
    round_trips(&deep);
}

#[test]
fn past_the_text_budget_the_rest_is_left_out() {
    let long = format!("{} tail", "x".repeat(MAX_TEXT + 1));
    let p = parse_with_notices(&long);
    assert!(p.query.is_empty());
    assert_eq!(p.notices[0].kind, NoticeKind::TooLong);
    let fits = format!("{} tail", "y".repeat(MAX_TEXT - 2));
    let p = parse(&fits);
    assert_eq!(p.clauses.len(), 1, "the second word is past the budget");
}

#[test]
fn positive_text_skips_negated_words_and_filters() {
    let q = parse(r#"a -b "c d" (e OR -f) #s"#);
    assert_eq!(q.positive_text(), vec!["a", "c d", "e"]);
}

#[test]
fn the_last_term_is_the_word_being_typed() {
    assert_eq!(parse("#work pass").last_term(), Some("pass"));
    assert_eq!(parse("(a OR pass").last_term(), Some("pass"));
    assert_eq!(parse(r#"pass "x""#).last_term(), None);
    assert_eq!(parse("-pass").last_term(), None);
}

#[test]
fn dates_resolve_against_the_readers_day_and_week() {
    // A Wednesday.
    let today = date(2026, 10, 21);
    let mo = Weekday::Mo;
    let days = |s: &str| {
        DateValue::parse(s)
            .unwrap()
            .days(today, &mo)
            .map(|d| (d.start, d.end))
            .unwrap()
    };
    assert_eq!(days("today"), (today, date(2026, 10, 22)));
    assert_eq!(
        days("wed"),
        (today, date(2026, 10, 22)),
        "today is included"
    );
    assert_eq!(days("tue"), (date(2026, 10, 27), date(2026, 10, 28)));
    assert_eq!(days("this-week"), (date(2026, 10, 19), date(2026, 10, 26)));
    assert_eq!(days("last-week"), (date(2026, 10, 12), date(2026, 10, 19)));
    assert_eq!(days("next-month"), (date(2026, 11, 1), date(2026, 12, 1)));
    assert_eq!(days("-2w"), (date(2026, 10, 7), date(2026, 10, 8)));
    assert_eq!(days("+1m"), (date(2026, 11, 21), date(2026, 11, 22)));
    assert_eq!(days("2026-02"), (date(2026, 2, 1), date(2026, 3, 1)));
    let su = DateValue::ThisWeek.days(today, &Weekday::Su).unwrap();
    assert_eq!(
        su.start,
        date(2026, 10, 18),
        "the week starts on the preference"
    );
    let range = DateSpan::Range(DateValue::Today, DateValue::Tomorrow)
        .days(today, &mo)
        .unwrap();
    assert_eq!((range.start, range.end), (today, date(2026, 10, 23)));
    assert!(DateSpan::Range(DateValue::Tomorrow, DateValue::Yesterday)
        .days(today, &mo)
        .is_none());
    assert!(DateValue::parse("2026-13").is_none());
    assert!(DateValue::parse("2026-02-30").is_none());
    assert!(DateValue::parse("+1000d").is_none());
}

#[test]
fn folding_drops_marks_case_and_width() {
    assert_eq!(fold("Café"), "cafe");
    assert_eq!(fold("ÅNGSTRÖM"), "angstrom");
    assert_eq!(fold("ＡＢＣ"), "abc");
    assert_eq!(fold("東京出張"), "東京出張");
}

#[test]
fn highlights_are_scalar_ranges_into_the_original() {
    let h = highlight("Renew the Passport at the café", &["pass", "cafe"]);
    assert_eq!(h.ranges, vec![(10, 14), (26, 30)]);
    let cjk = highlight("来週の東京出張の準備", &["東京"]);
    assert_eq!(cjk.ranges, vec![(3, 5)]);
    // A decomposed accent folds to one base letter; its range covers both.
    let decomposed = highlight("Cafe\u{301} ok", &["café"]);
    assert_eq!(decomposed.ranges, vec![(0, 5)]);
    let merged = highlight("passport", &["pass", "sport"]);
    assert_eq!(merged.ranges, vec![(0, 8)]);
}

#[test]
fn a_snippet_is_a_window_around_the_first_match() {
    let body = format!("{}needle{}", "a ".repeat(200), " b".repeat(200));
    let s = snippet(&body, &["needle"]).unwrap();
    assert!(s.text.chars().count() <= SNIPPET_LEN);
    let (start, end) = s.ranges[0];
    let shown: String = s
        .text
        .chars()
        .skip(start as usize)
        .take((end - start) as usize)
        .collect();
    assert_eq!(shown, "needle");
    assert!(snippet("nothing here", &["needle"]).is_none());
    let short = snippet("line one\nneedle", &["needle"]).unwrap();
    assert_eq!(short.text, "line one needle");
    assert_eq!(short.ranges, vec![(9, 15)]);
}

#[test]
fn cjk_is_recognized() {
    assert!("東京".chars().all(is_cjk));
    assert!("ひらがな".chars().all(is_cjk));
    assert!("한국어".chars().all(is_cjk));
    assert!(!"tokyo".chars().any(is_cjk));
}
