//! A writer from a build newer than any in the run.
//!
//! The property needs ops carrying kinds, fields and enum values that only a
//! newer build has. `HEAD` has very few of those relative to the baseline
//! today, and has none relative to itself, so the harness supplies them: it
//! takes a real device of the account (paired by `HEAD`, holding a real cert
//! and the Stream's key), and seals ops that device's build could not have
//! written, the way the next build will write them. Each is built from a
//! `HEAD` `Task` — so it is exactly the shape `HEAD` itself writes — and then
//! given the one thing a newer build would add.
//!
//! The ops never touch the relay. The harness hands each one to every replica
//! with `apply_remote`, in an order the scenario chooses, which is what lets a
//! property put a newer op on either side of a concurrent older-build write.

use std::collections::BTreeMap;

use ciborium::value::Value;
use rand_chacha::rand_core::{RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;
use sunrise_cbor::version::{DOC_SCHEMA_V, ENVELOPE_FORMAT_V};
use sunrise_cbor::{CborValue, Hlc};
use sunrise_crypto::keys::{DeviceSigningKeyPair, StreamKey};
use sunrise_crypto::op_envelope::{seal_envelope, OpEnvelope};
use sunrise_crypto::suite::{AeadAlgId, SigAlgId};
use sunrise_domain::{SunriseTime, Task, TaskState};

/// The unknown top-level field a newer build sets.
pub const FUTURE_FIELD: &str = "x326_note";
/// The task state a newer build writes.
pub const FUTURE_STATE: &str = "x326_waiting";
/// The `SunriseTime` kind a newer build writes into `due_at`.
pub const FUTURE_TIME_KIND: &str = "x326_lunar";
/// The op kind a newer build writes.
pub const FUTURE_OP_KIND: &str = "X326Annotate";

/// The four shapes of newer-build write the harness generates. Each names the
/// rule in `docs/02-domain/schema-versioning.md` §Compatibility rules it
/// exercises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FutureWrite {
    /// A top-level field the reader does not model (rule 1, top level).
    Field,
    /// An enum value the reader does not know (rule 2).
    EnumValue,
    /// A nested value of a kind the reader does not know: a `SunriseTime`
    /// kind inside `due_at` (rule 1 below the top level).
    NestedKind,
    /// An op kind the reader does not know (parking).
    OpKind,
}

impl FutureWrite {
    /// Every shape, for strategies and for documentation tables.
    pub const ALL: [Self; 4] = [Self::Field, Self::EnumValue, Self::NestedKind, Self::OpKind];
}

/// Seals newer-build ops as one certified device of the account.
#[derive(Debug)]
pub(crate) struct FutureWriter {
    device_id: [u8; 16],
    signing_seed: [u8; 32],
    stream_id: [u8; 16],
    epoch: u32,
    key: [u8; 32],
    next_seq: u64,
    rng: ChaCha20Rng,
}

impl FutureWriter {
    /// `device_id` and `signing_seed` are the paired device's; `epoch` and
    /// `key` its current key for `stream_id`.
    ///
    /// The device must never write to `stream_id` itself: this writer owns
    /// that `(stream, device)` sequence from 1. A paired device that issues no
    /// commands writes only control ops, which live on the meta stream.
    pub(crate) fn new(
        device_id: [u8; 16],
        signing_seed: [u8; 32],
        stream_id: [u8; 16],
        epoch: u32,
        key: [u8; 32],
    ) -> Self {
        Self {
            device_id,
            signing_seed,
            stream_id,
            epoch,
            key,
            next_seq: 1,
            rng: ChaCha20Rng::seed_from_u64(u64::from_le_bytes(
                device_id[..8].try_into().expect("8 bytes"),
            )),
        }
    }

    /// Seal `write` applied to `current`, the task as `HEAD` holds it now, at
    /// `now_ms`.
    pub(crate) fn seal(
        &mut self,
        write: FutureWrite,
        current: &Task,
        now_ms: u64,
        n: u32,
    ) -> Vec<u8> {
        let inner = match write {
            FutureWrite::Field => {
                let mut t = current.clone();
                t.unknown.insert(
                    FUTURE_FIELD.to_owned(),
                    CborValue(Value::Text(format!("note {n}"))),
                );
                update(&t)
            }
            FutureWrite::EnumValue => {
                let mut t = current.clone();
                t.state = TaskState::from_raw(FUTURE_STATE);
                update(&t)
            }
            FutureWrite::NestedKind => {
                let mut t = current.clone();
                let mut raw = BTreeMap::new();
                raw.insert(
                    "phase".to_owned(),
                    CborValue(Value::Text(format!("waxing {n}"))),
                );
                t.due_at = Some(SunriseTime::Unknown {
                    kind: FUTURE_TIME_KIND.to_owned(),
                    raw,
                });
                update(&t)
            }
            FutureWrite::OpKind => {
                let body = Value::Map(vec![
                    (
                        Value::Text("task".to_owned()),
                        Value::Text(current.id.to_str()),
                    ),
                    (
                        Value::Text("note".to_owned()),
                        Value::Text(format!("annotation {n}")),
                    ),
                ]);
                encode(&Value::Map(vec![(
                    Value::Text(FUTURE_OP_KIND.to_owned()),
                    body,
                )]))
            }
        };
        let mut nonce = [0u8; 24];
        self.rng.fill_bytes(&mut nonce);
        let seq = self.next_seq;
        self.next_seq += 1;
        let env = OpEnvelope {
            v: u32::from(ENVELOPE_FORMAT_V),
            stream_id: self.stream_id,
            device_id: self.device_id,
            seq,
            // One past the wall clock: a newer write the scenario issues after
            // a settle must order after everything that settle saw.
            hlc: Hlc {
                physical_ms: now_ms + 1,
                logical: 0,
            },
            aead_alg: AeadAlgId::XChaCha20Poly1305,
            sig_alg: SigAlgId::Ed25519,
            epoch: self.epoch,
            nonce,
            payload: inner,
            sig: [0u8; 64],
            doc_schema_v: u32::from(DOC_SCHEMA_V),
            unknown: BTreeMap::new(),
        };
        seal_envelope(
            env,
            Some(&StreamKey::from_bytes(self.key)),
            &DeviceSigningKeyPair::from_secret_bytes(&self.signing_seed),
        )
        .expect("a well-formed envelope seals")
    }
}

/// A `task.update` op carrying `t`, in the externally tagged form every
/// build's inner-op codec reads.
fn update(t: &Task) -> Vec<u8> {
    let body = Value::serialized(t).expect("a Task serializes");
    encode(&Value::Map(vec![(
        Value::Text("TaskUpdate".to_owned()),
        body,
    )]))
}

fn encode(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(v, &mut out).expect("CBOR encodes");
    out
}
