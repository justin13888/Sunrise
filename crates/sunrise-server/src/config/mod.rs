//! Server config: the value the server runs on, and where it comes from.
//!
//! Two halves that share nothing but [`ServerConfig`]. `model` is the value
//! type, its defaults and the safety rules it is checked against — pure, which
//! is why those rules are testable without a filesystem. `file` builds one from
//! the outside world: the TOML tables, the candidate paths, the `-c`/`--config`
//! arguments and `$SUNRISE_CONFIG`. It is the only code in the crate that reads
//! `std::env` or the config filesystem, and nothing in `model` knows either
//! exists.

mod file;
mod model;

pub use file::{
    load, resolve_candidate, AuthTable, Candidate, FileConfig, LoadError, ServerTable,
    StorageTable, ENV_CONFIG, IMPLICIT_CONFIG_PATHS,
};
pub use model::{ConfigError, ServerConfig};

pub(crate) use model::binds_loopback;
