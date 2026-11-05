//! The flags every subcommand takes: `--json` and `--tz <IANA>`.
//!
//! They are recognised anywhere on the line, before or after the subcommand
//! (`sunrise --json today` and `sunrise today --json` are the same), so a
//! script can append one without knowing a subcommand's own grammar. A `--`
//! ends that: everything after it is the subcommand's, verbatim, which is how
//! to capture a task titled "--json".

use crate::i18n::strings;

/// The global flags, taken off the line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Global {
    /// `--json`: print the query's result as one JSON document.
    pub json: bool,
    /// `--tz <IANA>` or `--tz=<IANA>`: the reader's zone for this command.
    pub tz: Option<String>,
}

/// Split `args` into the global flags and everything else, in order.
///
/// # Errors
/// A `--tz` with no zone after it.
pub fn split(args: &[String]) -> Result<(Global, Vec<String>), String> {
    let mut global = Global::default();
    let mut rest = Vec::with_capacity(args.len());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--" => {
                rest.extend(it.by_ref().cloned());
                break;
            }
            "--json" => global.json = true,
            "--tz" => {
                let zone = it.next().ok_or_else(strings::tz::flag_needs_zone)?;
                global.tz = Some(zone.clone());
            }
            other => match other.strip_prefix("--tz=") {
                Some(zone) => global.tz = Some(zone.to_owned()),
                None => rest.push(a.clone()),
            },
        }
    }
    Ok((global, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_global_flags_come_off_the_line_wherever_they_are() {
        let (g, rest) = split(&v(&["--json", "today", "--tz", "Asia/Kolkata"])).unwrap();
        assert!(g.json);
        assert_eq!(g.tz.as_deref(), Some("Asia/Kolkata"));
        assert_eq!(rest, v(&["today"]));

        let (g, rest) = split(&v(&["show", "tsk_x", "--tz=UTC"])).unwrap();
        assert_eq!(g.tz.as_deref(), Some("UTC"));
        assert!(!g.json);
        assert_eq!(rest, v(&["show", "tsk_x"]));
    }

    #[test]
    fn a_double_dash_hands_the_rest_over_verbatim() {
        let (g, rest) = split(&v(&["capture", "--", "--json", "is", "a", "title"])).unwrap();
        assert!(!g.json);
        assert_eq!(rest, v(&["capture", "--json", "is", "a", "title"]));
    }

    #[test]
    fn a_tz_flag_with_no_zone_is_refused() {
        assert!(split(&v(&["today", "--tz"])).is_err());
    }
}
