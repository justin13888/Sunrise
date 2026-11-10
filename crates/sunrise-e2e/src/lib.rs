//! End-to-end gating crate.
//!
//! Boots cross-crate scenarios that span [`sunrise_core`] + [`sunrise_server`].
//! Run by `cargo test -p sunrise-e2e` and `mise run e2e-cli`.
//!
//! This crate's library surface is a **test harness**: helpers to boot the real
//! relay in-process, open [`Core`](sunrise_core::Core) instances wired to it
//! over the real SSE + `POST` sync transport, exchange device trust, and assert
//! convergence with event-driven waits (no bare sleeps for correctness — only a
//! short poll interval inside a timeout).
//!
//! The helpers live in four modules, each re-exported here so a scenario
//! imports them from the crate root: relay boot and transport factories
//! (`relay`), opening cores (`open`), canonical projections for table-dump
//! equality (`canonical`), and the convergence waits (`wait`).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::missing_panics_doc)]

pub mod chaos;
pub mod cli;
pub mod cross_version;
pub mod latency;

mod canonical;
mod open;
mod relay;
mod wait;

pub use canonical::{
    assert_task_converged, canonical_streams, canonical_task_by_id, canonical_tasks,
    CanonicalStream, CanonicalTask,
};
pub use open::{
    open_core_offline, open_core_with_factory, open_paired_core, open_paired_core_offline,
    open_paired_core_with_factory, open_recovered_core, open_synced_core, pair_with,
};
pub(crate) use open::{open_core_paired, try_open_core_unlocked};
pub use relay::{
    refusing_upload_ws_factory, signed_ws_factory, spawn_relay, spawn_relay_unlimited,
    spawn_relay_with, toxic_ws_factory, ws_factory, UploadRefusal,
};
pub use wait::{
    wait_field_ops, wait_live, wait_pending_zero, wait_streams_converge, wait_task_converges,
    wait_tasks_converge,
};
