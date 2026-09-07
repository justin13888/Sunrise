//! Unit-level checks for the chaos transport primitives: [`loopback_pair`] and
//! the [`Toxic`] fault-injecting wrapper. Convergence *scenarios* (real cores
//! over a toxic link) land later as `cargo test -p sunrise-e2e --test chaos`.

use std::time::Duration;

use proptest::prelude::*;
use sunrise_e2e::chaos::{loopback_pair, FaultHandle, Toxic, ToxicConfig};
use sunrise_sync::transport::{Transport, TransportError};

/// Drive `count` numbered single-byte frames through a `Toxic`-wrapped sender,
/// close it, then drain the peer. Returns the frames actually delivered.
async fn run_drop_scenario(seed: u64, drop_prob: f64, count: u8) -> Vec<Vec<u8>> {
    let (a, mut b) = loopback_pair();
    let config = ToxicConfig {
        drop_prob,
        ..ToxicConfig::passthrough()
    };
    let (mut toxic, _faults) = Toxic::with_seed(a, config, seed);
    for i in 0..count {
        toxic.send_frame(vec![i]).await.expect("send ok");
    }
    toxic.close().await.expect("close ok");
    let mut delivered = Vec::new();
    while let Some(f) = b.recv_frame().await.expect("recv ok") {
        delivered.push(f);
    }
    delivered
}

/// Same seed + drop probability must deliver exactly the same set of frames on
/// two independent runs (seeded determinism).
#[tokio::test]
async fn drop_is_deterministic_across_runs() {
    let run1 = run_drop_scenario(0xC0FF_EE00, 0.5, 100).await;
    let run2 = run_drop_scenario(0xC0FF_EE00, 0.5, 100).await;
    assert_eq!(run1, run2, "identical seed must reproduce delivery set");
    // Sanity: 0.5 drop over 100 frames neither delivers all nor none.
    assert!(
        !run1.is_empty() && run1.len() < 100,
        "expected a partial delivery, got {} frames",
        run1.len()
    );
}

/// Corruption changes exactly one byte and preserves length.
#[tokio::test]
async fn corruption_flips_one_byte_same_length() {
    let (a, mut b) = loopback_pair();
    let config = ToxicConfig {
        corrupt_prob: 1.0,
        ..ToxicConfig::passthrough()
    };
    let (mut toxic, _faults) = Toxic::with_seed(a, config, 7);
    let original: Vec<u8> = (0..32u8).collect();
    toxic.send_frame(original.clone()).await.expect("send ok");
    let got = b
        .recv_frame()
        .await
        .expect("recv ok")
        .expect("frame delivered");
    assert_eq!(got.len(), original.len(), "length must be preserved");
    let diffs = original.iter().zip(&got).filter(|(x, y)| x != y).count();
    assert_eq!(diffs, 1, "exactly one byte must differ");
}

/// While partitioned, sends fail; after healing, traffic flows again.
#[tokio::test]
async fn partition_blocks_then_heals() {
    let (a, mut b) = loopback_pair();
    let (mut toxic, faults): (_, FaultHandle) = Toxic::with_seed(a, ToxicConfig::passthrough(), 1);

    faults.partition(true);
    let err = toxic.send_frame(vec![1, 2, 3]).await;
    assert!(
        matches!(err, Err(TransportError::Unavailable(_))),
        "send must fail while partitioned, got {err:?}"
    );

    faults.partition(false);
    toxic
        .send_frame(vec![1, 2, 3])
        .await
        .expect("send after heal");
    let got = b.recv_frame().await.expect("recv ok");
    assert_eq!(got, Some(vec![1, 2, 3]), "traffic flows after heal");
}

/// A partitioned link also fails inbound receives even with a frame waiting.
#[tokio::test]
async fn partition_blocks_recv() {
    let (a, mut b) = loopback_pair();
    let (mut toxic, faults) = Toxic::with_seed(a, ToxicConfig::passthrough(), 1);
    // Peer pushes a frame toward the toxic end.
    b.send_frame(vec![9]).await.expect("peer send");
    faults.partition(true);
    let got = toxic.recv_frame().await;
    assert!(
        matches!(got, Err(TransportError::Unavailable(_))),
        "recv must fail while partitioned, got {got:?}"
    );
}

/// Delays are applied but ordering is preserved (FIFO per direction).
#[tokio::test]
async fn delay_preserves_fifo_order() {
    let (a, mut b) = loopback_pair();
    let config = ToxicConfig {
        delay: Some((Duration::from_millis(1), Duration::from_millis(4))),
        ..ToxicConfig::passthrough()
    };
    let (mut toxic, _faults) = Toxic::with_seed(a, config, 42);
    for i in 0..10u8 {
        toxic.send_frame(vec![i]).await.expect("send ok");
    }
    toxic.close().await.expect("close ok");
    let mut order = Vec::new();
    while let Some(f) = b.recv_frame().await.expect("recv ok") {
        order.push(f[0]);
    }
    let expected: Vec<u8> = (0..10).collect();
    assert_eq!(order, expected, "frames must arrive in send order");
}

proptest! {
    // `Direct`, not the `SourceParallel` default: nothing above a `tests/` file
    // holds a `lib.rs` or `main.rs`, so the default warns and drops the
    // counterexample beside this source instead. See
    // docs/10-cross-cutting/testing.md section 2.
    #![proptest_config(ProptestConfig {
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/chaos_transport.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]

    /// With no faults, `Toxic` is byte-transparent for arbitrary frame
    /// sequences: whatever is sent arrives, in order, unchanged.
    #[test]
    fn passthrough_is_byte_transparent(
        frames in proptest::collection::vec(
            proptest::collection::vec(any::<u8>(), 0..48),
            0..24,
        )
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let sent = frames.clone();
        let got = rt.block_on(async move {
            let (a, mut b) = loopback_pair();
            let (mut toxic, _faults) =
                Toxic::with_seed(a, ToxicConfig::passthrough(), 123);
            for f in &frames {
                toxic.send_frame(f.clone()).await.expect("send ok");
            }
            toxic.close().await.expect("close ok");
            let mut got = Vec::new();
            while let Some(f) = b.recv_frame().await.expect("recv ok") {
                got.push(f);
            }
            got
        });
        prop_assert_eq!(got, sent);
    }
}
