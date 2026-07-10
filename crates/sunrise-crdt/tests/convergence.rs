//! CRDT convergence property test per docs/10-cross-cutting/testing.md §2.
//!
//! N=3 simulated replicas; each replica generates a random sequence of
//! ops; we cross-import in a random order; assert all replicas converge to
//! identical state.

use proptest::prelude::*;
use sunrise_crdt::StreamDoc;

#[derive(Debug, Clone)]
enum Op {
    SetMetaScalar { key: String, value: String },
    AppendText(String),
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        ("[a-z]{1,8}", "[\\PC]{0,32}",).prop_map(|(k, v)| Op::SetMetaScalar { key: k, value: v }),
        "[\\PC]{0,32}".prop_map(Op::AppendText),
    ]
}

fn apply(d: &StreamDoc, op: &Op) {
    match op {
        Op::SetMetaScalar { key, value } => {
            d.set_meta_scalar(key, value.as_str()).unwrap();
        }
        Op::AppendText(s) => {
            d.append_body_text(s).unwrap();
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        ..ProptestConfig::default()
    })]

    #[test]
    fn three_replica_convergence(
        ops_a in proptest::collection::vec(op_strategy(), 0..16),
        ops_b in proptest::collection::vec(op_strategy(), 0..16),
        ops_c in proptest::collection::vec(op_strategy(), 0..16),
    ) {
        let a = StreamDoc::new(1);
        let b = StreamDoc::new(2);
        let c = StreamDoc::new(3);

        for op in &ops_a { apply(&a, op); }
        for op in &ops_b { apply(&b, op); }
        for op in &ops_c { apply(&c, op); }

        // Every replica imports updates from every other.
        let snap_a = a.export_snapshot().unwrap();
        let snap_b = b.export_snapshot().unwrap();
        let snap_c = c.export_snapshot().unwrap();

        // Round 1: spread snapshots.
        a.import_updates(&snap_b).unwrap();
        a.import_updates(&snap_c).unwrap();
        b.import_updates(&snap_a).unwrap();
        b.import_updates(&snap_c).unwrap();
        c.import_updates(&snap_a).unwrap();
        c.import_updates(&snap_b).unwrap();

        // Round 2: pull any updates each replica has but the others don't.
        let from_a_to_b = a.export_updates_since(&b.version_vector()).unwrap();
        b.import_updates(&from_a_to_b).unwrap();
        let from_a_to_c = a.export_updates_since(&c.version_vector()).unwrap();
        c.import_updates(&from_a_to_c).unwrap();
        let from_b_to_a = b.export_updates_since(&a.version_vector()).unwrap();
        a.import_updates(&from_b_to_a).unwrap();
        let from_b_to_c = b.export_updates_since(&c.version_vector()).unwrap();
        c.import_updates(&from_b_to_c).unwrap();
        let from_c_to_a = c.export_updates_since(&a.version_vector()).unwrap();
        a.import_updates(&from_c_to_a).unwrap();
        let from_c_to_b = c.export_updates_since(&b.version_vector()).unwrap();
        b.import_updates(&from_c_to_b).unwrap();

        // All three replicas now agree on the body text and the version vector.
        let body_a = a.body_text();
        let body_b = b.body_text();
        let body_c = c.body_text();
        prop_assert_eq!(&body_a, &body_b);
        prop_assert_eq!(&body_b, &body_c);
    }
}
