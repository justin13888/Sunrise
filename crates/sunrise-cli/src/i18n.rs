//! The CLI's user-facing strings: the generated catalog binding and the
//! runtime it calls (ADR-0054).
//!
//! [`strings`] is `packages/sunrise-i18n/generated/strings.rs`, compiled from
//! `i18n/*.toml` and committed; nothing here parses a catalog at run time. The
//! binding calls three things, which are [`rt`]: the catalog locale in use,
//! the CLDR cardinal category of an integer, and an integer in that locale's
//! digits. ICU4X answers the last two from CLDR data baked into the binary.
//!
//! What a line of output *is* — a record `sunrise vaults | wc -l` counts, an id
//! a script reads — is not a message and stays out of the catalog. Only prose
//! a person reads goes through here.

/// The generated messages: `strings::sync::live()`,
/// `strings::devices::unwound(3)`, one function per `cli.*` key.
#[allow(
    // The doc comment on each function is the source message verbatim, and
    // ICU syntax is not markdown.
    clippy::doc_markdown,
    // Two arms rendering the same English (`one {# pending} other {#
    // pending}`) are still two arms: another locale's are not the same.
    clippy::match_same_arms,
    // With one translation a key dispatches `match rt::locale() { "pl" =>
    // …, _ => … }`, which is the same shape at one locale as at ten.
    clippy::single_match_else
)]
pub mod strings {
    use super::rt;

    include!("../../../packages/sunrise-i18n/generated/strings.rs");
}

/// What the generated binding calls.
pub mod rt {
    use std::sync::OnceLock;

    use icu_decimal::input::Decimal;
    use icu_decimal::options::DecimalFormatterOptions;
    use icu_decimal::DecimalFormatter;
    use icu_locale_core::Locale;
    use icu_plurals::{PluralCategory, PluralRules};

    use super::strings::LOCALES;

    /// A CLDR cardinal plural category.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Category {
        /// CLDR `zero`.
        Zero,
        /// CLDR `one`.
        One,
        /// CLDR `two`.
        Two,
        /// CLDR `few`.
        Few,
        /// CLDR `many`.
        Many,
        /// CLDR `other`.
        Other,
    }

    impl From<PluralCategory> for Category {
        fn from(c: PluralCategory) -> Self {
            match c {
                PluralCategory::Zero => Self::Zero,
                PluralCategory::One => Self::One,
                PluralCategory::Two => Self::Two,
                PluralCategory::Few => Self::Few,
                PluralCategory::Many => Self::Many,
                PluralCategory::Other => Self::Other,
            }
        }
    }

    fn parse(locale: &str) -> Locale {
        // Every tag reaching here is one the generator wrote (a catalog file
        // name it validated) or the source locale, so a parse failure is a
        // generator bug; `und` still yields CLDR's root rules rather than a
        // panic in the middle of printing a message.
        locale.parse().unwrap_or(Locale::UNKNOWN)
    }

    /// The CLDR cardinal category of `n` in `locale`.
    #[must_use]
    pub fn plural(locale: &str, n: i64) -> Category {
        match PluralRules::try_new_cardinal(parse(locale).into()) {
            Ok(rules) => rules.category_for(n).into(),
            Err(_) => Category::Other,
        }
    }

    /// `n` in `locale`'s digits and grouping: `1,000` in English.
    #[must_use]
    pub fn number(locale: &str, n: i64) -> String {
        match DecimalFormatter::try_new(parse(locale).into(), DecimalFormatterOptions::default()) {
            Ok(f) => f.format(&Decimal::from(n)).to_string(),
            Err(_) => n.to_string(),
        }
    }

    /// The catalog locale for `requested` tags in preference order: an exact
    /// match, then the tag's language, then the source locale.
    ///
    /// POSIX spellings are accepted as the environment writes them —
    /// `pl_PL.UTF-8`, `ar_EG@latn` — since that is where the CLI's come from.
    #[must_use]
    pub fn negotiate<'a>(requested: impl IntoIterator<Item = &'a str>) -> &'static str {
        let source = LOCALES.first().copied().unwrap_or("en");
        for raw in requested {
            let tag = raw
                .split(['.', '@'])
                .next()
                .unwrap_or_default()
                .replace('_', "-");
            if tag.is_empty() || tag == "C" || tag == "POSIX" {
                continue;
            }
            if let Some(hit) = LOCALES.iter().find(|l| l.eq_ignore_ascii_case(&tag)) {
                return hit;
            }
            let language = tag.split('-').next().unwrap_or_default();
            if let Some(hit) = LOCALES.iter().find(|l| l.eq_ignore_ascii_case(language)) {
                return hit;
            }
        }
        source
    }

    /// The catalog locale this process speaks, negotiated once from the
    /// environment in POSIX precedence: `LC_ALL`, then `LC_MESSAGES`, then
    /// `LANG`. An empty variable is skipped, as POSIX says it is.
    #[must_use]
    pub fn locale() -> &'static str {
        static LOCALE: OnceLock<&'static str> = OnceLock::new();
        LOCALE.get_or_init(|| {
            let vars: Vec<String> = ["LC_ALL", "LC_MESSAGES", "LANG"]
                .iter()
                .filter_map(|v| std::env::var(v).ok())
                .filter(|v| !v.is_empty())
                .collect();
            negotiate(vars.iter().map(String::as_str))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::rt::{self, Category};
    use super::strings;

    /// ICU4X's CLDR data must agree with the categories the catalog's checks
    /// read from `Intl.PluralRules` — the same three locales the CI plural
    /// matrix runs, one sample per category they distinguish.
    #[test]
    fn cldr_categories_across_the_matrix_locales() {
        let cases: &[(&str, i64, Category)] = &[
            ("en", 1, Category::One),
            ("en", 0, Category::Other),
            ("en", 2, Category::Other),
            ("pl", 1, Category::One),
            ("pl", 2, Category::Few),
            ("pl", 5, Category::Many),
            ("pl", 22, Category::Few),
            ("ar", 0, Category::Zero),
            ("ar", 1, Category::One),
            ("ar", 2, Category::Two),
            ("ar", 3, Category::Few),
            ("ar", 11, Category::Many),
            ("ar", 100, Category::Other),
        ];
        for &(locale, n, want) in cases {
            assert_eq!(rt::plural(locale, n), want, "{locale} {n}");
        }
    }

    #[test]
    fn numbers_use_the_locale_grouping() {
        assert_eq!(rt::number("en", 1_234_567), "1,234,567");
        assert_eq!(rt::number("en", -5), "-5");
    }

    #[test]
    fn negotiation_reads_posix_spellings_and_falls_back_to_the_source() {
        assert_eq!(rt::negotiate(["en_US.UTF-8"]), "en");
        assert_eq!(rt::negotiate(["C", "POSIX", ""]), "en");
        // `zz` is an unassigned language subtag, so no catalog will ever hold it.
        assert_eq!(rt::negotiate(["zz_ZZ.UTF-8", "zz@latn"]), "en");
        assert_eq!(rt::negotiate(["zz", "EN-gb"]), "en");
        assert_eq!(rt::negotiate([]), "en");
    }

    /// The generated functions select their arms by the rules above, and
    /// render `#` in the locale's digits.
    #[test]
    fn generated_messages_pluralize() {
        assert_eq!(
            strings::login::signed_in(1),
            "Signed in. Access token valid for 1 second."
        );
        assert_eq!(
            strings::login::signed_in(3600),
            "Signed in. Access token valid for 3,600 seconds."
        );
        assert_eq!(
            strings::sync::pending(2, "Connecting"),
            "sync: 2 pending (Connecting)"
        );
        assert!(strings::devices::unwound(1).contains("one other device, because"));
        assert!(strings::devices::unwound(4).contains("4 other devices, because"));
        assert_eq!(
            strings::focus::started_on("Write"),
            "focus started on Write"
        );
    }
}
