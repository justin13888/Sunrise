//! Chaos harness: a fault-injecting transport wrapper plus a reusable
//! in-process transport, for exercising sync convergence under network
//! adversity.
//!
//! Two building blocks:
//!
//! - [`loopback_pair`] — a connected pair of async, FIFO in-process transports
//!   ([`LoopbackEnd`]).
//! - [`Toxic`] — wraps any [`Transport`](sunrise_sync::transport::Transport)
//!   and injects drop / corrupt / delay / partition faults from a seeded RNG,
//!   with runtime control via a shared [`FaultHandle`].
//!
//! The convergence *scenarios* that drive real cores through a `Toxic` link and
//! assert byte-identical state after a heal land in a later slice as
//! `cargo test -p sunrise-e2e --test chaos`. This module is the harness they
//! build on.
//!
//! ## Reproducibility
//!
//! `Toxic` seeds its RNG from `SUNRISE_FUZZ_SEED` (see
//! [`seed_from_env`]) or an explicit seed via [`Toxic::with_seed`], so any run
//! is replayable from its seed.

pub mod loopback;
pub mod toxic;

pub use loopback::{loopback_pair, LoopbackEnd};
pub use toxic::{seed_from_env, FaultHandle, Toxic, ToxicConfig, DEFAULT_FUZZ_SEED};
