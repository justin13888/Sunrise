//! Domain-separated values that travel between two parties, frozen.
//!
//! Everything here is a string or a digest that one side computes and another
//! side has to agree with without either transmitting the rule. A pairing SAS
//! that two devices derive differently reads as an attack to the user; a
//! request signature whose canonical string differs by one line reads as a
//! forged device; a relay batch hash that moved silently un-dedups every retry.
//!
//! Specified by `docs/03-crypto/pairing-and-onboarding.md`, ADR-0022,
//! ADR-0033, `docs/02-domain/routines-and-recurrence.md` and
//! `docs/09-integrations`.

/// One `compute_sas` vector.
///
/// `sas_int = decimal_be(BLAKE3.derive_key("sunrise.pair_sas.v1",
/// handshake_hash, 3)) mod 1_000_000`, per
/// `docs/03-crypto/pairing-and-onboarding.md`.
///
/// The SAS is the *whole* of pairing's man-in-the-middle defence: the user
/// compares six digits on two screens. Neither device sends the rule, so two
/// builds that derive it differently show two different codes and the user is
/// told, correctly, that the pairing is unsafe — for no reason.
#[derive(Debug, Clone, Copy)]
pub struct SasVector {
    /// The 32-byte Noise XX handshake hash.
    pub handshake_hash: [u8; 32],
    /// The six ASCII digits shown to the user.
    pub sas: &'static str,
}

/// Three SAS vectors, both byte edges and one mid-range input.
pub const SAS_VECTORS: [SasVector; 3] = [
    SasVector {
        handshake_hash: [0x00; 32],
        sas: "249762",
    },
    SasVector {
        handshake_hash: [0x07; 32],
        sas: "297149",
    },
    SasVector {
        handshake_hash: [0xff; 32],
        sas: "343465",
    },
];

/// One `account_email_hash` vector.
///
/// `BLAKE3.derive_key("sunrise.account_email_hash.v1", lowercase(trim(email)),
/// 4)`, per `docs/03-crypto/pairing-and-onboarding.md`.
///
/// The bucket key a pairing rate limit counts against. A build that derived it
/// differently would file the same user under a different bucket on a
/// different device, which is a rate limit that does not limit.
#[derive(Debug, Clone, Copy)]
pub struct EmailHashVector {
    /// The address as supplied, before normalisation.
    pub email: &'static str,
    /// The 4-byte bucket key.
    pub hash: [u8; 4],
}

/// Three email-hash vectors, including the empty address and one that
/// exercises the lowercase-and-trim normalisation.
pub const EMAIL_HASH_VECTORS: [EmailHashVector; 3] = [
    EmailHashVector {
        email: "justin@example.com",
        hash: crate::hex("3409e3e0"),
    },
    EmailHashVector {
        email: "  A@B.co  ",
        hash: crate::hex("3b85fd6b"),
    },
    EmailHashVector {
        email: "",
        hash: crate::hex("777f6c37"),
    },
];

/// The `sunrise-device-sig-v2` canonical string and one signature over it.
///
/// ```text
/// canonical = "sunrise-device-sig-v2" \n method \n path_and_query \n date \n
///             hex(BLAKE3(canonical_json(body)))
/// header    = base64url_nopad(Ed25519(D_S_priv, canonical))
/// ```
///
/// per ADR-0022. The version tag is the first line of the signed string, so it
/// is only observable *through* a signature — a verifier that spelled it
/// differently would reject every request and blame the client. The two
/// signatures below are frozen because Ed25519 is deterministic; the canonical
/// string is frozen beside them so a reader can see what moved when one fails.
pub mod device_sig_v2 {
    /// `D_S_priv` — the same seed the envelope vectors sign with.
    pub const SIGNING_SECRET: [u8; 32] = crate::DEVICE_SIGNING_SECRET;
    /// The `Date` header value both vectors sign.
    pub const DATE: &str = "Tue, 14 Nov 2023 22:13:20 GMT";
    /// The canonical string for the bodyless `GET`, spelled out.
    ///
    /// The trailing hash is `BLAKE3("")`, which is what a request with no body
    /// contributes.
    pub const CANONICAL_NO_BODY: &str = concat!(
        "sunrise-device-sig-v2\n",
        "GET\n",
        "/api/v1/meta\n",
        "Tue, 14 Nov 2023 22:13:20 GMT\n",
        "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
    );
    /// `sign(GET /api/v1/meta, no body)`.
    pub const SIG_NO_BODY: &str =
        "e4zwUDEBY9SBVVojJGECjYNse3QDHbFnQj00T9uTL75h6Rtvccm4IdEI0whuculCyC6wizk8aXLuPWSnCxLgDQ";
    /// Path and query of the vector that carries a body.
    pub const PATH_WITH_BODY: &str = "/api/v1/sync/publish?x=1";
    /// The body, as RFC 8785 canonical JSON: keys sorted, which is the
    /// property the signature depends on and the caller does not control.
    pub const CANONICAL_BODY: &str = r#"{"a":1,"b":2}"#;
    /// `sign(POST /api/v1/sync/publish?x=1, {"b":2,"a":1})`.
    pub const SIG_WITH_BODY: &str =
        "JSND6ns9u7QRvetxfV7pMSSUGd6ojfPGCDhlm9w00h9H1sq1c4TqVrqPM_Xe0KCRAFK2fptiN5kJNjUr_Z8eBA";
    /// `device_pub_b64` of the public half of [`SIGNING_SECRET`].
    pub const DEVICE_PUB_B64: &str = "0EqyMnQrtKs6E2i9RhXk5tAiSrcaAWuvhSCjMsl3hzc";
}

/// The relay's batch-dedup key, frozen.
///
/// `BLAKE3("sunrise.relay.batch.v1" || u64_le(count) || for each op:
/// u64_le(len) || op)`, per ADR-0033.
///
/// The relay dedups a retried publish on this hash and on nothing else, so a
/// build whose hash moved would accept a retry as a new batch — appending
/// every op twice — while the client's outbox reported success both times.
pub mod relay_batch {
    /// The first op of the frozen batch.
    pub const OP_ONE: &[u8] = b"op-one";
    /// The second, deliberately a different length so the length prefix is
    /// exercised.
    pub const OP_TWO: &[u8] = b"op-two-longer";
    /// `batch_ops_hash(&[OP_ONE, OP_TWO])`.
    pub const HASH: [u8; 32] =
        crate::hex("d6b68377ceef61b89a0ffe4081bb0fb8bc620b85b76ebcfdc7a1bd0f4b6ad45e");
}

/// One `imported_block_id` vector.
///
/// `BLAKE3("sunrise.import_block.v1" || u64_be(source.len()) || source ||
/// uid)[..16]`.
///
/// Every device that imports the same calendar has to land on the same Block
/// id or a re-import on a second device is a duplicate rather than an update.
/// The rule is never transmitted — the id is, and the two have to agree.
#[derive(Debug, Clone, Copy)]
pub struct ImportBlockVector {
    /// The import source label.
    pub source: &'static str,
    /// The external item's `UID`.
    pub uid: &'static str,
    /// The derived 16-byte Block id.
    pub block_id: [u8; 16],
}

/// Three import vectors: the spec's own example, the same `UID` under a second
/// source, and the empty pair.
pub const IMPORT_BLOCK_VECTORS: [ImportBlockVector; 3] = [
    ImportBlockVector {
        source: "ics",
        uid: "ev1@example.com",
        block_id: crate::hex("cf058891b76d8801566a00abc4199b0d"),
    },
    ImportBlockVector {
        source: "work-calendar",
        uid: "ev1@example.com",
        block_id: crate::hex("3ea28721733acfcebf3c21e23fb23be2"),
    },
    ImportBlockVector {
        source: "",
        uid: "",
        block_id: crate::hex("d65c580be79c4c0c5b9598d0861c6c19"),
    },
];

/// One `occurrence_task_id` vector.
///
/// `BLAKE3("sunrise.routine_task.v1" || routine_id_bytes || key)[..16]`.
///
/// Two replicas materialise the same routine occurrence independently and rely
/// on deriving the same Task id for the CRDT to dedup them. A build that
/// derived it differently produces two Tasks for one occurrence on every
/// device pair, for ever.
#[derive(Debug, Clone, Copy)]
pub struct OccurrenceTaskIdVector {
    /// Raw bytes of the routine's `EntityRef`.
    pub routine_id: [u8; 16],
    /// The occurrence key.
    pub key: &'static str,
    /// The derived 16-byte Task id.
    pub task_id: [u8; 16],
}

/// Two occurrence vectors: an ordinary key and the empty one.
pub const OCCURRENCE_TASK_ID_VECTORS: [OccurrenceTaskIdVector; 2] = [
    OccurrenceTaskIdVector {
        routine_id: [0x5c; 16],
        key: "2026-01-01T09:00",
        task_id: crate::hex("9c5cbdb430eb738d721f5bbe73f24891"),
    },
    OccurrenceTaskIdVector {
        routine_id: [0x5c; 16],
        key: "",
        task_id: crate::hex("f5440a32f336d581a2576058f0e2d87a"),
    },
];
