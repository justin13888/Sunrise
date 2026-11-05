use super::*;

#[test]
fn probabilities_clamp() {
    let h = FaultHandle::new(2.0, -1.0);
    assert!((h.drop_prob() - 1.0).abs() < f64::EPSILON);
    assert!(h.corrupt_prob().abs() < f64::EPSILON);
    h.set_drop_prob(0.5);
    assert!((h.drop_prob() - 0.5).abs() < f64::EPSILON);
}

fn a_frame(kind: MsgKind) -> Vec<u8> {
    sunrise_wire_protocol::encode_frame(kind, sunrise_wire_protocol::FrameFlags::EMPTY, &[1, 2, 3])
        .expect("encode")
}

#[tokio::test]
async fn every_drop_is_counted_in_both_directions() {
    let (a, mut b) = crate::chaos::loopback_pair();
    let config = ToxicConfig {
        drop_prob: 1.0,
        ..ToxicConfig::passthrough()
    };
    let (mut toxic, faults) = Toxic::with_seed(a, config, 1);
    for _ in 0..3 {
        toxic
            .send_frame(vec![7])
            .await
            .expect("a dropped send succeeds");
    }
    assert_eq!(faults.dropped(), 3);

    b.send_frame(vec![8]).await.expect("peer send");
    b.close().await.expect("peer close");
    assert_eq!(toxic.recv_frame().await.expect("recv"), None);
    assert_eq!(faults.dropped(), 4, "the inbound frame was swallowed");
    assert_eq!(faults.corrupted(), 0);
}

#[tokio::test]
async fn every_corruption_is_counted_and_a_corrupted_subscribe_is_not() {
    let (a, mut b) = crate::chaos::loopback_pair();
    let config = ToxicConfig {
        corrupt_prob: 1.0,
        ..ToxicConfig::passthrough()
    };
    let (mut toxic, faults) = Toxic::with_seed(a, config, 2);
    toxic
        .send_frame(a_frame(MsgKind::Subscribe))
        .await
        .expect("send");
    assert_eq!(faults.corrupted(), 1);
    assert_eq!(
        faults.subscribes_sent(),
        0,
        "a Subscribe the relay cannot read is no resync round"
    );

    b.send_frame(vec![9, 9]).await.expect("peer send");
    toxic.recv_frame().await.expect("recv").expect("a frame");
    assert_eq!(faults.corrupted(), 2);
    assert_eq!(faults.dropped(), 0);
}

#[tokio::test]
async fn only_forwarded_subscribes_are_counted() {
    let (a, _b) = crate::chaos::loopback_pair();
    let (mut toxic, faults) = Toxic::with_seed(a, ToxicConfig::passthrough(), 3);
    toxic
        .send_frame(a_frame(MsgKind::Subscribe))
        .await
        .expect("send");
    toxic
        .send_frame(a_frame(MsgKind::OpBatch))
        .await
        .expect("send");
    toxic.send_frame(vec![0; 4]).await.expect("send");
    assert_eq!(faults.subscribes_sent(), 1);

    faults.partition(true);
    assert!(toxic.send_frame(a_frame(MsgKind::Subscribe)).await.is_err());
    assert_eq!(faults.subscribes_sent(), 1, "a refused one never left");
    assert_eq!(faults.refused_sends(), 1);
}

#[tokio::test]
async fn the_account_api_reaches_the_inner_transport_unless_partitioned() {
    let (a, _b) = crate::chaos::loopback_pair();
    let (mut toxic, faults) = Toxic::with_seed(a, ToxicConfig::passthrough(), 4);
    // The loopback has no account API, so reaching it answers Unsupported.
    assert!(matches!(
        toxic.blob_fetch(&[0; 16]).await,
        Err(TransportError::Unsupported)
    ));
    faults.partition(true);
    assert!(matches!(
        toxic.blob_fetch(&[0; 16]).await,
        Err(TransportError::Unavailable(_))
    ));
    assert_eq!(faults.refused_sends(), 0, "only sends are counted");
}

#[tokio::test]
async fn wait_until_returns_once_the_condition_holds() {
    let h = FaultHandle::new(0.0, 0.0);
    h.inner.dropped.fetch_add(1, Ordering::Relaxed);
    h.wait_until("a drop", Duration::from_millis(50), |h| h.dropped() > 0)
        .await;
}

#[tokio::test]
#[should_panic(expected = "waiting for a drop")]
async fn wait_until_panics_at_its_timeout() {
    let h = FaultHandle::new(0.0, 0.0);
    h.wait_until("a drop", Duration::from_millis(30), |h| h.dropped() > 0)
        .await;
}
