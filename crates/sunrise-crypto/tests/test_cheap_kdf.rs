//! The `test-cheap-kdf` feature: where it may reach, and what it does there.
//!
//! The feature puts recovery blobs on a cheap Argon2id set for the tests of
//! other crates (`src/recovery.rs`, `docs/10-cross-cutting/testing.md`). A
//! release build cannot use the set, because it is compiled only where
//! `debug_assertions` is on; the first test here holds the other half, that no
//! shipped package's own dependency graph turns the feature on at all.

use std::path::Path;
use std::process::Command;

/// Every package a release ships: the `sunrise-server` and `sunrise` binaries
/// (`release.yml`'s binaries job, the server image), the `UniFFI` seam the Apple
/// and Android apps link, and the web client's WASM core.
const SHIPPED: [&str; 4] = [
    "sunrise-server",
    "sunrise-cli",
    "sunrise-core-bindings",
    "sunrise-core-wasm",
];

/// The features `sunrise-crypto` resolves to when only the shipped packages
/// are built, through normal dependencies alone: what a release build of any
/// of them would compile. Dev-dependencies, where the test crates switch the
/// feature on, are left out exactly as a release build leaves them out.
#[test]
fn no_shipped_package_enables_the_cheap_kdf() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut tree = Command::new(cargo);
    tree.arg("tree")
        .arg("--locked")
        .arg("--manifest-path")
        .arg(&workspace);
    for package in SHIPPED {
        tree.args(["-p", package]);
    }
    tree.args(["-e", "normal,features", "-i", "sunrise-crypto"])
        .args(["--prefix", "none", "--depth", "1"]);
    let out = tree.output().expect("run cargo tree");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "cargo tree failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.starts_with("sunrise-crypto v"),
        "sunrise-crypto is not in the shipped packages' graph, so this check reads nothing:\n{stdout}"
    );
    assert!(
        !stdout.contains("feature \"test-cheap-kdf\""),
        "a shipped package's normal dependencies enable sunrise-crypto's `test-cheap-kdf`; \
         it belongs in `[dev-dependencies]` only:\n{stdout}"
    );
}

#[cfg(all(feature = "test-cheap-kdf", debug_assertions))]
mod cheap {
    use rand_chacha::ChaCha20Rng;
    use rand_core::SeedableRng;
    use sunrise_crypto::recovery::test_cheap_kdf::at_production_strength;
    use sunrise_crypto::recovery::{
        retired_check_matches, seal_recovery_blob, unseal_recovery_blob, RecoveryError,
        RecoveryPayload, RETIRED_CHECK_LEN,
    };

    fn payload() -> RecoveryPayload {
        RecoveryPayload {
            id_s_priv: [1u8; 32],
            id_d_priv: [2u8; 32],
            id_s_pub: [3u8; 32],
            id_d_pub: [4u8; 32],
            identity_id: [5u8; 16],
            created_at_ms: 1_700_000_000_000,
            account_id_s_pub: None,
            meta_genesis_keys: Vec::new(),
        }
    }

    /// With the feature on, a blob round-trips under the cheap set, and the
    /// set is not the spec's: the same blob read at production strength is
    /// named as the other set's, and so is the check kept of it.
    #[test]
    fn a_cheap_blob_round_trips_and_is_not_a_production_one() {
        let mut rng = ChaCha20Rng::seed_from_u64(1);
        let seed = [9u8; 32];
        let p = payload();
        let blob = seal_recovery_blob(&seed, &p, &mut rng).unwrap();
        assert_eq!(
            unseal_recovery_blob(&blob, &seed, &p.identity_id).unwrap(),
            p
        );
        assert!(retired_check_matches(&blob[..RETIRED_CHECK_LEN], &seed).unwrap());

        at_production_strength(|| {
            assert!(matches!(
                unseal_recovery_blob(&blob, &seed, &p.identity_id),
                Err(RecoveryError::KdfParamsMismatch)
            ));
            assert!(matches!(
                retired_check_matches(&blob[..RETIRED_CHECK_LEN], &seed),
                Err(RecoveryError::KdfParamsMismatch)
            ));
        });
        // The scope ends with the closure: this thread is back on the cheap set.
        assert_eq!(
            unseal_recovery_blob(&blob, &seed, &p.identity_id).unwrap(),
            p
        );
    }

    /// The other direction: a production blob read by a build on the cheap
    /// set fails as a mismatch, not as a wrong code, and a wrong code is
    /// still a wrong code.
    #[test]
    fn a_production_blob_read_cheaply_is_a_mismatch() {
        let mut rng = ChaCha20Rng::seed_from_u64(2);
        let seed = [9u8; 32];
        let p = payload();
        let blob = at_production_strength(|| seal_recovery_blob(&seed, &p, &mut rng).unwrap());
        assert!(matches!(
            unseal_recovery_blob(&blob, &seed, &p.identity_id),
            Err(RecoveryError::KdfParamsMismatch)
        ));
        assert!(matches!(
            unseal_recovery_blob(&blob, &[0u8; 32], &p.identity_id),
            Err(RecoveryError::AuthFailed)
        ));
    }
}
