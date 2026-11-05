//! The relay-revocation drain, against a transport that records what it is
//! asked to revoke.

use super::*;

/// Records what it was asked to revoke, and answers as a relay would.
struct Recorder {
    seen: Arc<std::sync::Mutex<Vec<[u8; 16]>>>,
    answer: Result<RevokeOutcome, ()>,
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
    async fn revoke_device(
        &mut self,
        device_id: [u8; 16],
    ) -> Result<RevokeOutcome, TransportError> {
        self.seen.lock().unwrap().push(device_id);
        self.answer
            .map_err(|()| TransportError::Unavailable("relay down".into()))
    }
}

/// Revoking a device queues the relay's half, and a session hands it to
/// the transport.
///
/// This is the write bound. A revocation that never reaches the relay
/// leaves it accepting the revoked device's uploads and streaming it
/// everyone else's ops, whatever the vault believes — and the relay cannot
/// learn it from the op stream, because `device_revoke` is sealed under a
/// key the relay does not hold.
#[tokio::test]
async fn a_revocation_is_handed_to_the_transport_and_then_forgotten() {
    let dir = tempfile::tempdir().unwrap();
    let core = open_arc(dir.path()).await;
    let target = [0x9c; 16];

    // A revocation of a device this vault knows about. The command refuses
    // an unknown device, so the row is planted the way pairing would.
    core.queue_relay_revocation_for_test(target).unwrap();
    assert_eq!(core.pending_relay_revocations().unwrap(), vec![target]);

    // A relay that cannot be reached keeps the intent: the device this is
    // about is typically lost, and that is exactly when the network is also
    // the thing that just went away. Giving up would leave it accepted
    // forever, with nothing left that remembers otherwise.
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut unreachable = Recorder {
        seen: seen.clone(),
        answer: Err(()),
    };
    crate::relay_intents::drain(&core, &mut unreachable).await;
    assert_eq!(*seen.lock().unwrap(), vec![target]);
    assert_eq!(
        core.pending_relay_revocations().unwrap(),
        vec![target],
        "a refused revocation must stay queued"
    );
    assert_eq!(
        core.relay_revocation_attempts_for_test(target).unwrap(),
        1,
        "and the refusal must be counted, so a relay that keeps saying no is visible"
    );

    // And an accepting one clears it, so it is not re-sent every session.
    let mut accepting = Recorder {
        seen: seen.clone(),
        answer: Ok(RevokeOutcome::Revoked),
    };
    crate::relay_intents::drain(&core, &mut accepting).await;
    assert!(
        core.pending_relay_revocations().unwrap().is_empty(),
        "an accepted revocation must not be re-sent forever"
    );
}

/// A relay that has never heard of the device clears the intent, because
/// retrying cannot make a row appear — and it is *not* reported as a
/// revocation.
///
/// This is the case PR #86 got wrong: it read the `404` as success and
/// logged that the relay had been told to stop accepting the device, about
/// a request naming an id the relay had never held. The distinction is a
/// separate `RevokeOutcome` arm rather than a comment, so the two cannot be
/// collapsed again by accident.
#[tokio::test]
async fn a_relay_that_never_knew_the_device_ends_the_intent_without_claiming_a_revocation() {
    let dir = tempfile::tempdir().unwrap();
    let core = open_arc(dir.path()).await;
    let target = [0x3e; 16];
    core.queue_relay_revocation_for_test(target).unwrap();

    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut unknown = Recorder {
        seen: seen.clone(),
        answer: Ok(RevokeOutcome::Unknown),
    };
    crate::relay_intents::drain(&core, &mut unknown).await;

    assert_eq!(*seen.lock().unwrap(), vec![target]);
    assert!(
        core.pending_relay_revocations().unwrap().is_empty(),
        "a 404 is terminal: no retry can make a row appear"
    );
}

/// A transport with no account API is not counted as an attempt.
///
/// The loopback transports every convergence test runs on cannot revoke
/// anything. Counting them would inflate `attempts` on a row that had never
/// actually been offered to a relay, which is the one number an operator
/// would read to decide whether a relay is refusing them.
#[tokio::test]
async fn an_unsupported_transport_does_not_burn_an_attempt() {
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
    let target = [0x7d; 16];
    core.queue_relay_revocation_for_test(target).unwrap();

    crate::relay_intents::drain(&core, &mut NoAccountApi).await;

    let attempts = core.relay_revocation_attempts_for_test(target).unwrap();
    assert_eq!(
        attempts, 0,
        "a transport with no account API is not a refusal"
    );
    assert_eq!(
        core.pending_relay_revocations().unwrap(),
        vec![target],
        "and the intent survives for a transport that can carry it"
    );
}

/// An intent whose register row a re-fold unwound is not sent, and is
/// sent after all once the register revokes the device again (#257).
///
/// The unwind is the fold rebuilding `device_revocations` without the
/// row; it never touches `relay_revocation_intents`. Sending the intent
/// anyway would have the relay refuse a device every replica shows as
/// current. Deleting it instead would lose it for the later fold that
/// restores the revocation, and the relay would then be behind the
/// register — the #160 direction, which is the unsafe one.
#[tokio::test]
async fn an_intent_the_register_no_longer_agrees_with_is_held_not_sent() {
    let dir = tempfile::tempdir().unwrap();
    let core = open_arc(dir.path()).await;
    let target = [0x5a; 16];
    core.queue_relay_revocation_for_test(target).unwrap();
    core.unwind_register_row_for_test(target).unwrap();

    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut accepting = Recorder {
        seen: seen.clone(),
        answer: Ok(RevokeOutcome::Revoked),
    };
    crate::relay_intents::drain(&core, &mut accepting).await;
    assert!(
        seen.lock().unwrap().is_empty(),
        "a device the register calls current must not be cut at the relay"
    );
    assert!(
        !core.relay_revocation_pending(&target).unwrap(),
        "and no surface may call it queued, since it will not be sent"
    );
    assert!(
        core.relay_intent_row_exists_for_test(target).unwrap(),
        "the intent is held, not dropped"
    );

    // The register revokes it again, as a later fold can.
    core.queue_relay_revocation_for_test(target).unwrap();
    assert!(core.relay_revocation_pending(&target).unwrap());
    crate::relay_intents::drain(&core, &mut accepting).await;
    assert_eq!(
        *seen.lock().unwrap(),
        vec![target],
        "once the register agrees again the relay is owed the revocation"
    );
    assert!(!core.relay_intent_row_exists_for_test(target).unwrap());
}

/// A device unwound while the drain is awaiting an earlier send is not
/// sent (#257).
///
/// The drain reads the owed set once, then awaits the network per device.
/// This transport unwinds the second device's register row while the
/// first send is in flight, as a local command on another task could.
#[tokio::test]
async fn a_device_unwound_mid_drain_is_not_sent() {
    struct UnwindsOnFirstSend {
        core: Arc<Core>,
        unwind: [u8; 16],
        seen: Vec<[u8; 16]>,
    }
    #[async_trait]
    impl Transport for UnwindsOnFirstSend {
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
            if self.seen.is_empty() {
                self.core.unwind_register_row_for_test(self.unwind).unwrap();
            }
            self.seen.push(device_id);
            Ok(RevokeOutcome::Revoked)
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let core = open_arc(dir.path()).await;
    // Same `created_at_ms`, so the id breaks the tie: `first` is sent first.
    let first = [0x11; 16];
    let second = [0x22; 16];
    core.queue_relay_revocation_for_test(first).unwrap();
    core.queue_relay_revocation_for_test(second).unwrap();
    assert_eq!(
        core.pending_relay_revocations().unwrap(),
        vec![first, second]
    );

    let mut transport = UnwindsOnFirstSend {
        core: core.clone(),
        unwind: second,
        seen: Vec::new(),
    };
    crate::relay_intents::drain(&core, &mut transport).await;
    assert_eq!(
        transport.seen,
        vec![first],
        "a device the register stopped calling revoked mid-drain must not be cut"
    );
    assert!(
        core.relay_intent_row_exists_for_test(second).unwrap(),
        "and its intent is held, not dropped"
    );
}

/// A storage failure is an error, not "nothing queued".
///
/// Reporting `false` here would tell someone the relay had been told about
/// a device when this client could not even read whether it had.
#[tokio::test]
async fn a_storage_failure_is_not_read_as_nothing_pending() {
    let dir = tempfile::tempdir().unwrap();
    let core = open_arc(dir.path()).await;
    let target = [0x6b; 16];
    core.queue_relay_revocation_for_test(target).unwrap();
    core.break_register_for_test().unwrap();

    assert!(
        core.relay_revocation_pending(&target).is_err(),
        "an unreadable register must surface as an error"
    );
    assert!(core.pending_relay_revocations().is_err());
}
