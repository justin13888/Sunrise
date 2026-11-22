//! The Postgres backends a relay running as several nodes shares (ADR-0062).
//!
//! One database holds everything the single-binary relay keeps in its SQLite
//! file — accounts, devices, push tokens, cursors, tombstones, the blob ledger,
//! the pins and the durable relay log — and the three things a single process
//! keeps in memory: sync sessions ([`PgSessions`], §3), rate cells
//! ([`PgLimiter`], §5) and the pairing rendezvous ([`PgRendezvous`], #1262).
//! Every node connects to it, and any node serves any account.
//!
//! - **Metadata.** [`PgStore`] implements [`MetadataStore`]
//!   (`metadata.rs`). Each method is one transaction at `READ COMMITTED`, so a
//!   revocation committed on one node is what every request that starts after
//!   it sees, on any node. Where a method reads before it writes — the
//!   write-once recovery blob, the conditional pins — it locks the account row
//!   first, which is what the SQLite store's one connection gave for free.
//! - **The relay log.** An append takes a transaction-scoped advisory lock on
//!   its channel before it inserts (`relay_log.rs`), so a channel's frame ids
//!   are assigned in the order its appends commit. That is what lets a node
//!   read a channel "after the last id it published" and never skip a frame
//!   that commits late. The append issues `NOTIFY sunrise_relay` in the same
//!   transaction, and [`PgFanout`] (`fanout.rs`) is the listener on every node
//!   that turns the notices into local publishes.
//! - **Leadership.** The maintenance pass runs on the node holding a
//!   session-level advisory lock ([`PgStore::lead_maintenance`]); Postgres
//!   releases it when that node's connection dies.
//!
//! The connection URL and its CA come from files (`PgConnect`, ADR-0062 §6),
//! and the schema from this module's own numbered history (`migrations.rs`).

mod accounts;
mod fanout;
mod lifecycle;
mod limiter;
mod metadata;
mod migrations;
mod pairing;
mod relay_log;
mod sessions;
mod tls;

pub(crate) use tls::client_config_from_ca_file;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod two_node;

use std::path::{Path, PathBuf};
use std::time::Duration;

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use futures_util::StreamExt as _;
use tokio_postgres::config::SslMode;
use tokio_postgres::{AsyncMessage, Client, NoTls, Notification};

pub use fanout::PgFanout;
pub use limiter::PgLimiter;
pub use pairing::PgRendezvous;
pub use sessions::PgSessions;

use super::MetadataError;
use tls::MakeRustls;

/// Connections each node's pool holds at most. One request holds one for one
/// transaction, so this bounds a node's concurrent store work, not its
/// requests; a request that finds none free waits up to [`POOL_WAIT`].
const POOL_SIZE: usize = 16;

/// How long a request waits for a pooled connection before its store call
/// fails as unavailable, which the client retries.
const POOL_WAIT: Duration = Duration::from_secs(5);

/// How long opening one connection may take, where the URL sets no
/// `connect_timeout` of its own.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// `options` with every transaction defaulting to READ COMMITTED, after
/// whatever the URL set, so the later setting wins.
///
/// The read-then-write transactions here (the relay append's dedup read, the
/// account row's `FOR UPDATE`) are written for READ COMMITTED: under a server
/// or role default of REPEATABLE READ or SERIALIZABLE a concurrent duplicate
/// append fails on the batch's key instead of reading the first one's row.
/// A startup option outranks both of those defaults. The transactions that
/// want one snapshot ask for REPEATABLE READ themselves.
fn with_read_committed(options: Option<&str>) -> String {
    let ours = r"-c default_transaction_isolation=read\ committed";
    match options {
        Some(theirs) if !theirs.trim().is_empty() => format!("{theirs} {ours}"),
        _ => ours.to_owned(),
    }
}

/// The session-level advisory lock the maintenance leader holds. Distinct
/// from the migration lock's key, and from the channel and account locks,
/// which use the two-key form and so a different key space.
const MAINTENANCE_LOCK: i64 = 0x5352_5345_4d41_494e;

/// Why the Postgres backend could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The URL file could not be read, or does not hold a URL.
    #[error("[storage] postgres_url_file {}: {cause}", path.display())]
    UrlFile {
        /// The file.
        path: PathBuf,
        /// What was wrong with it.
        cause: String,
    },
    /// The URL file can be read by its group or by others.
    #[error(
        "[storage] postgres_url_file {} has mode {mode:o}: it holds the database password, so \
         only its owner may read it — chmod 600",
        path.display()
    )]
    UrlPermissions {
        /// The file.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// The URL asks for TLS and no CA file says whom to trust.
    #[error(
        "the Postgres URL asks for sslmode=require: name the CA its server certificate chains \
         to in [storage] postgres_ca_file"
    )]
    TlsWithoutCa,
    /// The CA file could not be used.
    #[error("{0}")]
    Tls(String),
    /// The database could not be reached, or refused a statement.
    #[error("postgres: {0}")]
    Db(String),
    /// The database's schema is one a newer release wrote.
    #[error(
        "the Postgres database is at schema version {found}, and this binary supports up to \
         {supported}: run the release that wrote it, or restore a backup taken before that \
         upgrade"
    )]
    SchemaTooNew {
        /// The stored version.
        found: i64,
        /// The newest this binary migrates to.
        supported: i32,
    },
}

impl OpenError {
    fn db(e: impl std::fmt::Display) -> Self {
        Self::Db(e.to_string())
    }
}

/// A failed statement or connection, as every [`MetadataStore`] caller sees
/// it: retryable, and nothing committed.
pub(super) fn unavailable(e: impl std::fmt::Display) -> MetadataError {
    MetadataError::Unavailable(e.to_string())
}

/// How to reach the database: the parsed URL and its TLS. Holds the password,
/// so its `Debug` shows neither.
#[derive(Clone)]
pub struct PgConnect {
    config: tokio_postgres::Config,
    tls: Option<MakeRustls>,
    /// The channel appends notify on and the bus listens to:
    /// `sunrise_relay`, or a test's own so concurrent tests on one database
    /// do not hear each other.
    notify: String,
}

impl std::fmt::Debug for PgConnect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgConnect")
            .field("tls", &self.tls.is_some())
            .finish_non_exhaustive()
    }
}

impl PgConnect {
    /// Read the URL from `url_file`, which only its owner may read, and the
    /// trust roots from `ca_file`.
    ///
    /// # Errors
    /// Each [`OpenError`] about the files or the URL in them.
    pub fn from_files(url_file: &Path, ca_file: Option<&Path>) -> Result<Self, OpenError> {
        let unreadable = |cause: String| OpenError::UrlFile {
            path: url_file.to_owned(),
            cause,
        };
        let meta = std::fs::metadata(url_file).map_err(|e| unreadable(e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(OpenError::UrlPermissions {
                    path: url_file.to_owned(),
                    mode,
                });
            }
        }
        #[cfg(not(unix))]
        let _ = meta;
        let url = std::fs::read_to_string(url_file).map_err(|e| unreadable(e.to_string()))?;
        Self::from_url(url.trim(), ca_file).map_err(|e| match e {
            // The parser's message can quote the URL, password and all.
            OpenError::Db(_) => unreadable("not a Postgres connection URL".to_owned()),
            other => other,
        })
    }

    /// Parse `url`, a `postgres://` URL or `key=value` string.
    ///
    /// With `ca_file` every connection is TLS, verified against it. Without
    /// one connections are plaintext, and a URL that asks for
    /// `sslmode=require` is refused rather than silently downgraded.
    ///
    /// # Errors
    /// The URL does not parse, asks for TLS with no CA, or the CA file is
    /// unusable.
    pub fn from_url(url: &str, ca_file: Option<&Path>) -> Result<Self, OpenError> {
        let mut config: tokio_postgres::Config = url.parse().map_err(OpenError::db)?;
        if config.get_connect_timeout().is_none() {
            config.connect_timeout(CONNECT_TIMEOUT);
        }
        config.options(with_read_committed(config.get_options()));
        let tls = match ca_file {
            Some(ca) => {
                config.ssl_mode(SslMode::Require);
                Some(MakeRustls::from_ca_file(ca).map_err(OpenError::Tls)?)
            }
            None if config.get_ssl_mode() == SslMode::Require => {
                return Err(OpenError::TlsWithoutCa);
            }
            None => {
                config.ssl_mode(SslMode::Disable);
                None
            }
        };
        Ok(Self {
            config,
            tls,
            notify: relay_log::NOTIFY_CHANNEL.to_owned(),
        })
    }

    /// This connection confined to `schema`, notifying on a channel of its
    /// own: the tests' way of sharing one database without sharing state.
    #[cfg(test)]
    pub(crate) fn isolated(mut self, schema: &str) -> Self {
        let options = self.config.get_options().unwrap_or_default();
        self.config
            .options(format!("{options} -c search_path={schema}").trim_start());
        self.notify = format!("{}_{schema}", relay_log::NOTIFY_CHANNEL);
        self
    }

    /// The notification channel relay appends announce themselves on.
    pub(super) fn notify(&self) -> &str {
        &self.notify
    }

    /// A pool of connections to the database.
    fn pool(&self) -> Result<Pool, OpenError> {
        let mc = ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        };
        let manager = match &self.tls {
            Some(tls) => Manager::from_config(self.config.clone(), tls.clone(), mc),
            None => Manager::from_config(self.config.clone(), NoTls, mc),
        };
        Pool::builder(manager)
            .max_size(POOL_SIZE)
            .runtime(Runtime::Tokio1)
            .wait_timeout(Some(POOL_WAIT))
            .create_timeout(Some(CONNECT_TIMEOUT))
            .build()
            .map_err(OpenError::db)
    }

    /// One connection of its own, outside the pool, whose notifications go to
    /// `notices`; `notices` closes when the connection ends.
    pub(crate) async fn listen(
        &self,
        notices: tokio::sync::mpsc::UnboundedSender<Notification>,
    ) -> Result<Client, OpenError> {
        match &self.tls {
            Some(tls) => {
                let (client, conn) = self
                    .config
                    .connect(tls.clone())
                    .await
                    .map_err(OpenError::db)?;
                drive(conn, notices);
                Ok(client)
            }
            None => {
                let (client, conn) = self.config.connect(NoTls).await.map_err(OpenError::db)?;
                drive(conn, notices);
                Ok(client)
            }
        }
    }
}

/// Run `conn` until it ends, forwarding each notification to `notices`.
fn drive<S, T>(
    mut conn: tokio_postgres::Connection<S, T>,
    notices: tokio::sync::mpsc::UnboundedSender<Notification>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut messages = futures_util::stream::poll_fn(move |cx| conn.poll_message(cx));
        while let Some(message) = messages.next().await {
            match message {
                Ok(AsyncMessage::Notification(n)) => {
                    if notices.send(n).is_err() {
                        return;
                    }
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
    });
}

/// The Postgres [`MetadataStore`](super::MetadataStore): a pool of
/// connections to one database every relay node shares.
#[derive(Clone)]
pub struct PgStore {
    pool: Pool,
    connect: PgConnect,
}

impl std::fmt::Debug for PgStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgStore").finish_non_exhaustive()
    }
}

impl PgStore {
    /// Connect, and bring the schema to the newest version this binary knows.
    ///
    /// # Errors
    /// The database could not be reached, or is at a version a newer release
    /// wrote, which is refused before anything is written.
    pub async fn open(connect: PgConnect) -> Result<Self, OpenError> {
        let pool = connect.pool()?;
        {
            let mut client = pool.get().await.map_err(OpenError::db)?;
            let client: &mut Client = &mut client;
            migrations::migrate(client).await?;
        }
        Ok(Self { pool, connect })
    }

    /// How this store reaches its database, for the listener and the
    /// leadership lock, which each hold a connection of their own.
    #[must_use]
    pub fn connect(&self) -> &PgConnect {
        &self.connect
    }

    /// A pooled connection.
    pub(crate) async fn client(&self) -> Result<deadpool_postgres::Object, MetadataError> {
        self.pool.get().await.map_err(unavailable)
    }

    /// Take the maintenance lead, or `None` when another node holds it.
    ///
    /// The lock is session-level, on a connection taken out of the pool for
    /// it, and the returned guard owns that connection: dropping the guard
    /// closes it, and Postgres releases the lock with it — as it does when a
    /// leader's process dies, so a crashed node cannot keep the role.
    ///
    /// # Errors
    /// The database did not answer.
    pub async fn lead_maintenance(&self) -> Result<Option<Box<dyn Send>>, MetadataError> {
        let client = deadpool_postgres::Object::take(self.client().await?);
        let held: bool = client
            .query_one("SELECT pg_try_advisory_lock($1)", &[&MAINTENANCE_LOCK])
            .await
            .map_err(unavailable)?
            .get(0);
        Ok(held.then(|| Box::new(client) as Box<dyn Send>))
    }
}

/// `u64` to the `BIGINT` it is stored as, saturating.
pub(super) fn signed(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// A `BIGINT` back to `u64`, a negative one as 0.
pub(super) fn unsigned(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// A 16-byte key read back from `BYTEA`.
pub(super) fn key16(v: &[u8]) -> Option<[u8; 16]> {
    v.try_into().ok()
}
