//! The relay's REST client, generated from the API description, and the
//! account/device bootstrap that finally calls it.
//!
//! # Why this crate exists
//!
//! ADR-0021 opens with the finding that motivated the whole port: **nothing in
//! the workspace called the REST API**. Not the CLI, not the macOS app, not
//! `sunrise-core-bindings`. `sunrise-onboarding` declared `AccountCreateRequest`
//! and `AccountInfo` and made no HTTP call with them. The account-and-device
//! bootstrap the crypto design depends on was specified, served, and never
//! invoked — and nothing detected that, because a route with no caller fails no
//! test.
//!
//! This is the caller.
//!
//! # Generated, not written
//!
//! [`api`] is emitted by `spargen` from `schemas/generated/openapi.v1.json` at build time:
//! typed models, one method per operation, typed errors. Nothing here transcribes
//! a request shape by hand, so the drift ADR-0021 catalogued — nine CDDL blocks
//! that had wandered from the handlers — has no place to happen. The
//! description is generated from the handlers and the client is generated from
//! the description.
//!
//! spargen embeds its runtime support into the generated module, so the
//! `spargen` crate itself is a build dependency only and never reaches a
//! consumer's runtime tree.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// The generated client: models, operations and typed errors.
///
/// Regenerated on every build from the committed description, so it is never
/// reviewed as a diff — what is reviewed is the description it comes from.
///
/// The workspace's lints do not apply here, deliberately. They are a style
/// contract between people writing code, and nobody writes this: linting it
/// would mean either editing generated output — which the next build discards —
/// or holding a generator to a house style it has never seen. spargen puts
/// `#![forbid(unsafe_code)]`-equivalent attributes on everything it emits, which
/// is the one guarantee that does have to hold.
///
/// The three `rustdoc::` entries are the same argument one layer along, and they
/// were added when `mise run rust-doc` started denying rustdoc warnings. The
/// generated `api.rs` documents public wrappers in terms of the private
/// `MaybeSend`/`MaybeSync`/`send` helpers spargen emits beside them, and links
/// a `Response` that lives in the runtime support module. Every one of those is
/// spargen's output shape rather than a broken reference someone here wrote, so
/// the honest options are to allow them or to hold a generator to a house style
/// it has never seen — the same fork this attribute already resolved for
/// clippy. If spargen ever qualifies those paths, delete these three lines and
/// the gate will say so.
#[allow(
    missing_docs,
    unreachable_pub,
    missing_debug_implementations,
    clippy::all,
    clippy::pedantic,
    rustdoc::broken_intra_doc_links,
    rustdoc::private_intra_doc_links,
    rustdoc::redundant_explicit_links
)]
pub mod api {
    include!(concat!(env!("OUT_DIR"), "/api.rs"));
}

mod bootstrap;

pub use bootstrap::{bootstrap, BootstrapError, BootstrapOutcome, DeviceIdentity};
