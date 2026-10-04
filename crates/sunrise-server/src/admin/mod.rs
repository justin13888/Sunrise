//! The operator's surface: `sunrise-server admin <cmd>`, and the maintenance
//! pass both it and the serving binary run.
//!
//! # Direct on the data dir, not an admin socket
//!
//! `admin` opens `[storage] data_dir` itself, through the same config
//! resolution the server uses, rather than talking to a running server.
//! `docs/06-server/self-hosting.md` asks for operator surfaces that are
//! loopback-only or authenticated; a command that needs a shell on the host and
//! read access to the data dir is both, with no listener, credential or
//! protocol to get wrong. SQLite's locking is what makes it safe beside a
//! running relay: the database is in WAL mode, each write takes the write lock
//! for one short transaction, and the busy timeout waits out the server's.
//!
//! The cost is that an erasure run here cannot reach the running server's
//! in-memory relay ring or its live sessions. Neither holds anything a client
//! of the erased account can reach again — its bearer no longer resolves to an
//! account — and both are dropped at the server's next restart.

pub mod cli;
pub mod maintenance;

#[cfg(test)]
mod tests;
