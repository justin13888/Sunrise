//! Two replicas of one vault and the deliveries between them: the harness a
//! test reaches for when its subject is what one device sees of another's ops.

use super::*;

/// Two replicas of one vault that trust each other, on clocks of their own.
pub(in crate::engine::tests) struct Pair {
    pub(in crate::engine::tests) ea: Engine,
    pub(in crate::engine::tests) eb: Engine,
    pub(in crate::engine::tests) ca: Arc<FakeClock>,
    pub(in crate::engine::tests) cb: Arc<FakeClock>,
    pub(in crate::engine::tests) dba: Db,
    pub(in crate::engine::tests) dbb: Db,
}

/// A [`Pair`] over `root` that has exchanged nothing.
fn pair_under(root: [u8; 32]) -> Pair {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(root, [1u8; 32], ca.clone());
    let eb = engine_seeded(root, [2u8; 32], cb.clone());
    let mut dba = db_root(root);
    let mut dbb = db_root(root);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);
    Pair {
        ea,
        eb,
        ca,
        cb,
        dba,
        dbb,
    }
}

/// A [`Pair`] over [`ROOT`] that has exchanged nothing.
pub(in crate::engine::tests) fn pair() -> Pair {
    pair_under(ROOT)
}

/// A [`Pair`] over `root`, each side having delivered what it wrote to the
/// other, after advertising its features if `advertise`.
pub(in crate::engine::tests) fn synced_pair(root: [u8; 32], advertise: bool) -> Pair {
    let mut p = pair_under(root);
    if advertise {
        assert!(p.ea.advertise_features(&mut p.dba).unwrap());
        assert!(p.eb.advertise_features(&mut p.dbb).unwrap());
    }
    sync(&mut p);
    p
}

/// Every op `from` logged under its own device, delivered to `to` in each
/// stream's order. Re-delivering one `to` already holds is a no-op.
///
/// `strict` panics on a refused op. A test whose subject is the refusal passes
/// `false` and asserts on what the refusal left behind.
pub(in crate::engine::tests) fn deliver(
    from: &Engine,
    from_db: &Db,
    to: &Engine,
    to_db: &mut Db,
    strict: bool,
) {
    let device = from.keychain.device_id();
    let envs: Vec<Vec<u8>> = {
        let mut stmt = from_db
            .conn()
            .prepare("SELECT envelope FROM ops WHERE device_id = ? ORDER BY stream_id, seq")
            .unwrap();
        stmt.query_map(params![&device[..]], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    for env in envs {
        let applied = to.apply_remote_all(to_db, &env);
        if strict {
            applied.unwrap();
        }
    }
}

/// [`deliver`] both ways across `p`, refusing nothing.
pub(in crate::engine::tests) fn sync(p: &mut Pair) {
    deliver(&p.ea, &p.dba, &p.eb, &mut p.dbb, true);
    deliver(&p.eb, &p.dbb, &p.ea, &mut p.dba, true);
}

/// Every envelope `db` holds, in the order a relay would hand them over:
/// by stream, device and seq.
pub(in crate::engine::tests) fn all_envs(db: &Db) -> Vec<Vec<u8>> {
    let mut stmt = db
        .conn()
        .prepare("SELECT envelope FROM ops ORDER BY stream_id, device_id, seq")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// Deliver every envelope of `from` to `to`, in relay order, except the ones
/// `to` wrote itself, which a relay does not echo back.
pub(in crate::engine::tests) fn relay(from: &Db, to: &Engine, to_db: &mut Db) {
    let me = to.keychain.device_id();
    for env in all_envs(from) {
        if decode_envelope(&env).unwrap().device_id != me {
            to.apply_remote_all(to_db, &env).unwrap();
        }
    }
}
