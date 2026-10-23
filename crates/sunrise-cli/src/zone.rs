//! Which zone the CLI reads times in, and where that choice came from.
//!
//! The core reads the reader's zone from its injected `Clock`
//! (`docs/11-adr/0066-times-survive-travel.md` §5), and a client that knows
//! better reports it through [`sunrise_core::Core::on_time_zone_changed`].
//! The CLI knows better in three ways, in this order of precedence:
//!
//! 1. `--tz <IANA>` on the command line, for one command;
//! 2. `SUNRISE_TZ`, for a shell or a cron job;
//! 3. `sunrise tz set <IANA>`, kept in the vault directory, for this device.
//!
//! With none of them the OS's zone stands, as `SystemClock` reads it. Every one
//! of the three reaches the core through `on_time_zone_changed`, never as a
//! zone a command passes around itself, so the queries, the capture parser and
//! the client layer all read in the same zone. A name the tzdb does not know is
//! [`ZoneError::Unknown`], naming where it came from — never UTC (§3).

use std::path::{Path, PathBuf};

use crate::i18n::strings;

/// The environment variable that names the reader's zone.
pub const ENV_TZ: &str = "SUNRISE_TZ";

/// The file `sunrise tz set` keeps its zone in, inside the vault directory.
///
/// Plain text: a zone name is not content, and the OS's own zone is no more
/// secret than this.
pub const ZONE_FILE: &str = "cli-timezone";

/// Where the zone in force came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneSource {
    /// `--tz`.
    Flag,
    /// `SUNRISE_TZ`.
    Env,
    /// `sunrise tz set`.
    Set,
    /// The OS, as the core's clock reads it.
    System,
}

impl ZoneSource {
    /// The stable name `sunrise tz` prints and `--json` carries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Flag => "flag",
            Self::Env => "env",
            Self::Set => "set",
            Self::System => "system",
        }
    }
}

/// A zone a person named, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneChoice {
    /// The IANA name, as given.
    pub name: String,
    /// Where it was given.
    pub source: ZoneSource,
}

/// A zone that cannot be read in.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ZoneError {
    /// The tzdb does not know the name.
    #[error("{}", strings::tz::unknown(.name, .from.as_str()))]
    Unknown {
        /// The name as given.
        name: String,
        /// Where it was given.
        from: ZoneSource,
    },
}

/// Check `name` against the tzdb, through the client layer's one reader.
///
/// # Errors
/// [`ZoneError::Unknown`].
pub fn check(name: &str, source: ZoneSource) -> Result<(), ZoneError> {
    sunrise_client_core::civil::reader_zone(name)
        .map(|_| ())
        .map_err(|_| ZoneError::Unknown {
            name: name.to_owned(),
            from: source,
        })
}

/// The zone a person named, by precedence, or `None` to keep the OS's.
///
/// An empty value counts as unset, so `SUNRISE_TZ= sunrise today` is the OS
/// zone rather than an error.
///
/// # Errors
/// [`ZoneError::Unknown`] for the first non-empty name the tzdb does not
/// know. A bad `--tz` is reported even when `SUNRISE_TZ` is good: the flag is
/// what was asked for.
pub fn choose(
    flag: Option<&str>,
    env: Option<&str>,
    stored: Option<&str>,
) -> Result<Option<ZoneChoice>, ZoneError> {
    let given = [
        (flag, ZoneSource::Flag),
        (env, ZoneSource::Env),
        (stored, ZoneSource::Set),
    ]
    .into_iter()
    .find_map(|(v, source)| {
        v.map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| (v.to_owned(), source))
    });
    let Some((name, source)) = given else {
        return Ok(None);
    };
    check(&name, source)?;
    Ok(Some(ZoneChoice { name, source }))
}

/// Where `sunrise tz set` keeps the zone of the vault at `vault_dir`.
#[must_use]
pub fn path(vault_dir: &Path) -> PathBuf {
    vault_dir.join(ZONE_FILE)
}

/// The zone `sunrise tz set` kept, if any.
#[must_use]
pub fn stored(vault_dir: &Path) -> Option<String> {
    std::fs::read_to_string(path(vault_dir))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Keep `zone`, or forget the kept one with `None`.
///
/// # Errors
/// The file could not be written or removed.
pub fn store(vault_dir: &Path, zone: Option<&str>) -> std::io::Result<()> {
    match zone {
        Some(z) => std::fs::write(path(vault_dir), format!("{z}\n")),
        None => match std::fs::remove_file(path(vault_dir)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_beats_the_environment_which_beats_the_stored_zone() {
        let pick = |f, e, s| choose(f, e, s).unwrap().map(|c| (c.name, c.source));
        assert_eq!(
            pick(Some("Asia/Kolkata"), Some("Europe/Berlin"), Some("UTC")),
            Some(("Asia/Kolkata".into(), ZoneSource::Flag))
        );
        assert_eq!(
            pick(None, Some("Europe/Berlin"), Some("UTC")),
            Some(("Europe/Berlin".into(), ZoneSource::Env))
        );
        assert_eq!(
            pick(None, Some("  "), Some("UTC")),
            Some(("UTC".into(), ZoneSource::Set))
        );
        assert_eq!(pick(None, None, None), None);
    }

    #[test]
    fn an_unknown_zone_is_a_typed_error_naming_its_source_never_utc() {
        assert_eq!(
            choose(Some("Mars/Olympus"), Some("UTC"), None),
            Err(ZoneError::Unknown {
                name: "Mars/Olympus".into(),
                from: ZoneSource::Flag
            })
        );
        let err = choose(None, Some("Not/AZone"), None).unwrap_err();
        assert!(err.to_string().contains("Not/AZone"), "{err}");
        assert!(err.to_string().contains("env"), "{err}");
    }

    #[test]
    fn a_stored_zone_is_kept_and_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(stored(dir.path()), None);
        store(dir.path(), Some("America/New_York")).unwrap();
        assert_eq!(stored(dir.path()).as_deref(), Some("America/New_York"));
        store(dir.path(), None).unwrap();
        store(dir.path(), None).unwrap();
        assert_eq!(stored(dir.path()), None);
    }
}
