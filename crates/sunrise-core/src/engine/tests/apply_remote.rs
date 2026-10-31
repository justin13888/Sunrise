//! The receive half of sync: envelope refusals and convergence.

use super::*;

#[test]
fn apply_remote_round_trip_materializes_identically() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca);
    let eb = engine_seeded(ROOT, [2u8; 32], cb);
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);

    let res = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "shared task".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    let env = env_bytes(&dba, &res.op_id);

    let ev = eb.apply_remote(&mut dbb, &env).unwrap();
    assert!(matches!(ev, Some(DomainEvent::Created(id)) if id == res.entity));

    // Field-by-field identical materialization on the receiver.
    let ta = read_task_t(&ea, &dba, res.entity);
    let tb = read_task_t(&eb, &dbb, res.entity);
    assert_eq!(ta, tb);
}

#[test]
fn apply_remote_is_idempotent() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);

    let res = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "once".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    let env = env_bytes(&dba, &res.op_id);

    assert!(eb.apply_remote(&mut dbb, &env).unwrap().is_some());
    let task_after_first = read_task_t(&eb, &dbb, res.entity);
    let ops_after_first = op_count(&dbb);

    // Second application is a no-op: Ok(None), row unchanged, no new ops row.
    assert!(eb.apply_remote(&mut dbb, &env).unwrap().is_none());
    assert_eq!(read_task_t(&eb, &dbb, res.entity), task_after_first);
    assert_eq!(op_count(&dbb), ops_after_first);
    assert_eq!(ops_after_first, 1, "exactly one remote op recorded");
}

#[test]
fn unknown_device_rejected() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    // NOTE: B does NOT trust A.

    let res = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "from stranger".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    let env = env_bytes(&dba, &res.op_id);

    assert!(matches!(
        eb.apply_remote(&mut dbb, &env),
        Err(EngineError::UnknownDevice)
    ));
    // No ops row, no materialization.
    assert_eq!(op_count(&dbb), 0);
    assert!(matches!(
        eb.query(&dbb, Query::EntityById(res.entity)),
        Err(EngineError::NotFound(_))
    ));
}

#[test]
fn tampered_envelope_rejected() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);

    let res = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "authentic".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    let env = env_bytes(&dba, &res.op_id);

    // Flip a byte in the trailing signature region.
    let mut sig_tampered = env.clone();
    let last = sig_tampered.len() - 1;
    sig_tampered[last] ^= 0x01;
    assert!(matches!(
        eb.apply_remote(&mut dbb, &sig_tampered),
        Err(EngineError::RemoteOpInvalid(_))
    ));

    // Flip a byte in the ciphertext region (roughly the middle of the CBOR).
    let mut ct_tampered = env.clone();
    let mid = ct_tampered.len() / 2;
    ct_tampered[mid] ^= 0x01;
    assert!(matches!(
        eb.apply_remote(&mut dbb, &ct_tampered),
        Err(EngineError::RemoteOpInvalid(_))
    ));

    // Nothing was applied by either rejection.
    assert_eq!(op_count(&dbb), 0);
}

#[test]
fn wrong_stream_key_rejected() {
    // A and B are on DIFFERENT vault roots, so B derives a different Stream
    // key and cannot decrypt A's payload — even though the cert (device-key
    // signed, root-independent) verifies and A is trusted.
    let root_a = [0xa1; 32];
    let root_b = [0xb2; 32];
    let ea = engine_seeded(root_a, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(root_b, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(root_a);
    let mut dbb = db_root(root_b);
    trust(&eb, &mut dbb, &ea);

    let res = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "secret".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    let env = env_bytes(&dba, &res.op_id);

    assert!(matches!(
        eb.apply_remote(&mut dbb, &env),
        Err(EngineError::RemoteOpInvalid(_))
    ));
    assert_eq!(op_count(&dbb), 0);
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig {
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/apply_remote.txt",
            ),
        )),
        ..proptest::prelude::ProptestConfig::with_cases(32)
    })]

    /// Two mutually-trusting engines run random interleaved command streams
    /// on their own DBs; exchanging ALL envelopes both ways (with reordering
    /// and duplication) converges both DBs to an identical task projection.
    #[test]
    fn convergence_under_random_interleaving(
        steps in proptest::collection::vec(
            (proptest::bool::ANY, 0u8..4u8, 0u8..8u8),
            1usize..14usize,
        ),
    ) {
        begin_case();
        // Shared timeline so ops carry a common ts axis (ties possible).
        let clock = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], clock.clone());
        let eb = engine_seeded(ROOT, [2u8; 32], clock.clone());
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);
        trust(&ea, &mut dba, &eb);

        let mut ids_a: Vec<EntityRef> = Vec::new();
        let mut ids_b: Vec<EntityRef> = Vec::new();
        let mut tick = T0;

        for (i, (actor_a, kind, arg)) in steps.into_iter().enumerate() {
            tick += 1;
            set_clock(&clock, tick);
            let (e, d, ids) = if actor_a {
                (&ea, &mut dba, &mut ids_a)
            } else {
                (&eb, &mut dbb, &mut ids_b)
            };
            // With no existing task, any non-create degrades to a create.
            let effective = if ids.is_empty() { 0 } else { kind };
            match effective {
                0 => {
                    let r = e
                        .apply(
                            d,
                            Command::CreateTask(TaskDraft {
                                title: format!("t{}-{}", usize::from(actor_a), i),
                                ..Default::default()
                            }),
                        )
                        .unwrap();
                    ids.push(r.entity);
                }
                1 => {
                    let target = ids[usize::from(arg) % ids.len()];
                    let _ = e.apply(
                        d,
                        Command::UpdateTask {
                            id: target,
                            patch: TaskPatch {
                                title: Some(format!("u{}-{}", usize::from(actor_a), i)),
                                ..Default::default()
                            },
                        },
                    );
                }
                2 => {
                    let target = ids[usize::from(arg) % ids.len()];
                    let _ = e.apply(d, Command::CompleteTask(target));
                }
                _ => {
                    let target = ids[usize::from(arg) % ids.len()];
                    let _ = e.apply(
                        d,
                        Command::DeferTask {
                            id: target,
                            to_ms: tick + 86_400_000,
                        },
                    );
                }
            }
        }

        // Exchange ALL envelopes both ways, forward then reversed (the
        // reversed pass re-delivers every op as a duplicate and in a
        // different order).
        let envs_a = all_envelopes(&dba);
        let envs_b = all_envelopes(&dbb);
        for pass in 0..2 {
            let a_iter: Vec<&Vec<u8>> = if pass == 0 {
                envs_a.iter().collect()
            } else {
                envs_a.iter().rev().collect()
            };
            let b_iter: Vec<&Vec<u8>> = if pass == 0 {
                envs_b.iter().collect()
            } else {
                envs_b.iter().rev().collect()
            };
            for env in a_iter {
                eb.apply_remote(&mut dbb, env).unwrap();
            }
            for env in b_iter {
                ea.apply_remote(&mut dba, env).unwrap();
            }
        }

        proptest::prop_assert_eq!(tasks_projection(&dba), tasks_projection(&dbb));
        ea.assert_rebuild_identity(&dba);
        eb.assert_rebuild_identity(&dbb);
    }
}

/// A proptest case replayed from `proptest-regressions` builds engines that
/// mint the ids they minted when it failed: [`begin_case`] restarts the
/// seeded sequence however many engines earlier cases built.
#[test]
fn begin_case_restarts_the_seeded_rng_sequence() {
    let draw = |rng: &SeededRng| {
        let mut b = [0u8; 16];
        rng.fill_bytes(&mut b);
        b
    };
    begin_case();
    let first = draw(&seeded_rng());
    let second = draw(&seeded_rng());
    assert_ne!(
        first, second,
        "two engines of one case draw distinct streams"
    );
    begin_case();
    assert_eq!(draw(&seeded_rng()), first);
    assert_eq!(draw(&seeded_rng()), second);
}
