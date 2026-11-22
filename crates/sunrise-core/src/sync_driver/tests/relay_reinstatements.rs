//! The relay-reinstatement drain (ADR-0056 §5), against a transport that
//! records what it is asked to reinstate.

use super::*;
use crate::relay_intents::RelayReinstatement;
use sunrise_sync::ReinstateOutcome;

/// Records what it was asked to reinstate, and answers as a relay would.
struct Recorder {
    seen: Arc<std::sync::Mutex<Vec<[u8; 16]>>>,
    answer: Result<ReinstateOutcome, ()>,
}

impl Recorder {
    fn answering(answer: Result<ReinstateOutcome, ()>) -> Self {
        Self {
            seen: Arc::new(std::sync::Mutex::new(Vec::new())),
            answer,
        }
    }

    fn seen(&self) -> Vec<[u8; 16]> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl Transport for Recorder {
    async fn send_frame(&mut self, _frame: Vec<u8>) -> Result<(), TransportError> {
        Ok(())
    }
    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        Ok(None)
    }
    async fn close(&mut self) -> Result<(), TransportError> {
        Ok(())
    }
    async fn reinstate_device(
        &mut self,
        device_id: [u8; 16],
    ) -> Result<ReinstateOutcome, TransportError> {
        self.seen.lock().unwrap().push(device_id);
        self.answer
            .map_err(|()| TransportError::Unavailable("relay down".into()))
    }
}

/// A queued reinstatement is pending until the relay accepts it, a relay
/// that cannot be reached keeps it and counts the attempt, and an accepting
/// one clears it so it is not sent again.
#[tokio::test]
async fn a_reinstatement_is_retried_until_the_relay_accepts_it() {
    let dir = tempfile::tempdir().unwrap();
    let core = open_arc(dir.path()).await;
    let target = [0x5a; 16];
    core.queue_relay_reinstatement_for_test(target).unwrap();
    assert_eq!(
        core.relay_reinstatement_pending(&target).unwrap(),
        RelayReinstatement::Pending
    );

    let mut unreachable = Recorder::answering(Err(()));
    crate::relay_intents::drain_reinstatements(&core, &mut unreachable).await;
    assert_eq!(unreachable.seen(), vec![target]);
    assert_eq!(
        core.relay_reinstatement_pending(&target).unwrap(),
        RelayReinstatement::Pending,
        "a failed send keeps the row"
    );
    assert_eq!(
        core.relay_reinstatement_attempts_for_test(target).unwrap(),
        Some(1),
        "and counts the attempt"
    );

    let mut accepting = Recorder::answering(Ok(ReinstateOutcome::Reinstated));
    crate::relay_intents::drain_reinstatements(&core, &mut accepting).await;
    assert_eq!(accepting.seen(), vec![target]);
    assert_eq!(
        core.relay_reinstatement_pending(&target).unwrap(),
        RelayReinstatement::Clear
    );
    assert_eq!(
        core.relay_reinstatement_attempts_for_test(target).unwrap(),
        None
    );

    let mut again = Recorder::answering(Ok(ReinstateOutcome::Reinstated));
    crate::relay_intents::drain_reinstatements(&core, &mut again).await;
    assert!(again.seen().is_empty(), "an accepted row is not sent again");
}

/// `NotRevoker` is terminal and not success: the row stays as its trace,
/// the seam reports the refusal, and the drain never sends it again.
#[tokio::test]
async fn a_relay_that_recorded_another_revoker_leaves_a_trace_and_is_not_asked_again() {
    let dir = tempfile::tempdir().unwrap();
    let core = open_arc(dir.path()).await;
    let target = [0x6b; 16];
    core.queue_relay_reinstatement_for_test(target).unwrap();

    let mut refusing = Recorder::answering(Ok(ReinstateOutcome::NotRevoker));
    crate::relay_intents::drain_reinstatements(&core, &mut refusing).await;
    assert_eq!(refusing.seen(), vec![target]);
    assert_eq!(
        core.relay_reinstatement_pending(&target).unwrap(),
        RelayReinstatement::Refused,
        "the refusal outlives the drain, so a client can say to pair it again"
    );

    let mut accepting = Recorder::answering(Ok(ReinstateOutcome::Reinstated));
    crate::relay_intents::drain_reinstatements(&core, &mut accepting).await;
    assert!(
        accepting.seen().is_empty(),
        "a refused row is never re-sent"
    );
    assert_eq!(
        core.relay_reinstatement_pending(&target).unwrap(),
        RelayReinstatement::Refused
    );

    // A fresh withdrawal writes the row anew: an earlier refusal says nothing
    // about a revocation made under this device's current key.
    core.queue_relay_reinstatement_for_test(target).unwrap();
    assert_eq!(
        core.relay_reinstatement_pending(&target).unwrap(),
        RelayReinstatement::Pending
    );
    assert_eq!(
        core.relay_reinstatement_attempts_for_test(target).unwrap(),
        Some(0)
    );
}

/// `Unknown` is terminal: no retry can make a row appear, so the row is
/// cleared, and it is not reported as a refusal either.
#[tokio::test]
async fn a_relay_with_no_such_device_ends_the_intent() {
    let dir = tempfile::tempdir().unwrap();
    let core = open_arc(dir.path()).await;
    let target = [0x7c; 16];
    core.queue_relay_reinstatement_for_test(target).unwrap();

    let mut unknown = Recorder::answering(Ok(ReinstateOutcome::Unknown));
    crate::relay_intents::drain_reinstatements(&core, &mut unknown).await;
    assert_eq!(unknown.seen(), vec![target]);
    assert_eq!(
        core.relay_reinstatement_pending(&target).unwrap(),
        RelayReinstatement::Clear
    );
    assert_eq!(
        core.relay_reinstatement_attempts_for_test(target).unwrap(),
        None
    );
}

/// A row is owed only while the register does not call the device revoked:
/// held, not sent, and not reported, while it does; sent once it does not.
#[tokio::test]
async fn a_reinstatement_waits_while_the_register_revokes_the_device() {
    let dir = tempfile::tempdir().unwrap();
    let core = open_arc(dir.path()).await;
    let target = [0x8d; 16];
    // Plants the register row beside a revocation intent.
    core.queue_relay_revocation_for_test(target).unwrap();
    core.queue_relay_reinstatement_for_test(target).unwrap();
    assert_eq!(
        core.relay_reinstatement_pending(&target).unwrap(),
        RelayReinstatement::Clear,
        "a device the register revokes is not back, so nothing is pending"
    );

    let mut accepting = Recorder::answering(Ok(ReinstateOutcome::Reinstated));
    crate::relay_intents::drain_reinstatements(&core, &mut accepting).await;
    assert!(
        accepting.seen().is_empty(),
        "held while the register revokes it"
    );
    assert_eq!(
        core.relay_reinstatement_attempts_for_test(target).unwrap(),
        Some(0),
        "held, not deleted"
    );

    core.unwind_register_row_for_test(target).unwrap();
    crate::relay_intents::drain_reinstatements(&core, &mut accepting).await;
    assert_eq!(accepting.seen(), vec![target]);
    assert_eq!(
        core.relay_reinstatement_pending(&target).unwrap(),
        RelayReinstatement::Clear
    );
}

/// A transport with no account API is not an attempt.
#[tokio::test]
async fn an_unsupported_transport_does_not_burn_a_reinstatement_attempt() {
    struct NoAccountApi;
    #[async_trait]
    impl Transport for NoAccountApi {
        async fn send_frame(&mut self, _f: Vec<u8>) -> Result<(), TransportError> {
            Ok(())
        }
        async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
            Ok(None)
        }
        async fn close(&mut self) -> Result<(), TransportError> {
            Ok(())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let core = open_arc(dir.path()).await;
    let target = [0x9e; 16];
    core.queue_relay_reinstatement_for_test(target).unwrap();
    crate::relay_intents::drain_reinstatements(&core, &mut NoAccountApi).await;
    assert_eq!(
        core.relay_reinstatement_attempts_for_test(target).unwrap(),
        Some(0)
    );
    assert_eq!(
        core.relay_reinstatement_pending(&target).unwrap(),
        RelayReinstatement::Pending
    );
}

/// A withdrawal that cancels a revocation while the drain is sending it
/// believes the relay was not told, and queues no reinstatement. The relay
/// then accepts the revocation, so the drain, finding the row gone, queues
/// the reinstatement itself.
#[tokio::test]
async fn a_revocation_cancelled_mid_send_queues_its_reinstatement() {
    /// Cancels the row and lets the device go, as a withdrawal would, while
    /// the revocation is in flight; then the relay accepts it.
    struct WithdrawnInFlight {
        core: Arc<Core>,
    }
    #[async_trait]
    impl Transport for WithdrawnInFlight {
        async fn send_frame(&mut self, _f: Vec<u8>) -> Result<(), TransportError> {
            Ok(())
        }
        async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
            Ok(None)
        }
        async fn close(&mut self) -> Result<(), TransportError> {
            Ok(())
        }
        async fn revoke_device(
            &mut self,
            device_id: [u8; 16],
        ) -> Result<RevokeOutcome, TransportError> {
            assert!(self.core.clear_relay_revocation(device_id).unwrap());
            self.core.unwind_register_row_for_test(device_id).unwrap();
            Ok(RevokeOutcome::Revoked)
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let core = open_arc(dir.path()).await;
    let target = [0xaf; 16];
    core.queue_relay_revocation_for_test(target).unwrap();
    let mut t = WithdrawnInFlight { core: core.clone() };
    crate::relay_intents::drain(&core, &mut t).await;
    assert_eq!(
        core.relay_reinstatement_pending(&target).unwrap(),
        RelayReinstatement::Pending,
        "the relay was told after all, so it must be told back"
    );
}

/// An ordinary accepted revocation queues no reinstatement.
#[tokio::test]
async fn an_accepted_revocation_queues_no_reinstatement() {
    struct Accepting;
    #[async_trait]
    impl Transport for Accepting {
        async fn send_frame(&mut self, _f: Vec<u8>) -> Result<(), TransportError> {
            Ok(())
        }
        async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
            Ok(None)
        }
        async fn close(&mut self) -> Result<(), TransportError> {
            Ok(())
        }
        async fn revoke_device(
            &mut self,
            _device_id: [u8; 16],
        ) -> Result<RevokeOutcome, TransportError> {
            Ok(RevokeOutcome::Revoked)
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let core = open_arc(dir.path()).await;
    let target = [0xb0; 16];
    core.queue_relay_revocation_for_test(target).unwrap();
    crate::relay_intents::drain(&core, &mut Accepting).await;
    assert_eq!(
        core.relay_reinstatement_attempts_for_test(target).unwrap(),
        None
    );
}
