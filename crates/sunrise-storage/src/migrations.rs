//! Schema migrations.
//!
//! Per `docs/04-storage/migrations.md`. Migration scripts are static, embedded
//! at build time, and applied in id order.
//!
//! There is currently exactly ONE migration: the `0013_baseline.sql` schema
//! reset (ADR-0018). The list, the runner, and the ordering rule are all still
//! here and still exercised — a single entry is a state of the list, not a
//! simplification of the mechanism — because the next schema change appends to
//! it exactly as before.
//!
//! Adding a migration after 1.0: bump `STORAGE_V` in `sunrise-cbor::version`,
//! append a new entry to [`MIGRATIONS`], add a new file `migrations/00NN_*.sql`.
//! Never edit an existing one.

/// Static migration record.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    /// Numeric id; matches the `STORAGE_V` after applying.
    pub id: u32,
    /// Human-readable name.
    pub name: &'static str,
    /// SQL DDL.
    pub sql: &'static str,
}

/// The lowest `storage_v` this build can open.
///
/// A vault at `0 < storage_v < BASELINE_STORAGE_V` predates the baseline reset
/// and is refused rather than upgraded: the migrations that would have carried
/// it forward no longer exist, and no released build ever produced such a
/// vault. See [`crate::db::DbError::StorageVPreBaseline`].
pub const BASELINE_STORAGE_V: u32 = 13;

/// All known migrations, in apply order.
pub const MIGRATIONS: &[Migration] = &[Migration {
    id: BASELINE_STORAGE_V,
    name: "baseline",
    sql: include_str!("../migrations/0013_baseline.sql"),
}];

/// Current storage version (= last migration id).
#[must_use]
pub fn current_storage_v() -> u32 {
    MIGRATIONS.iter().map(|m| m.id).max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_ids_are_unique_and_ascending() {
        let mut prev = 0;
        for m in MIGRATIONS {
            assert!(m.id > prev, "migration ids must strictly ascend");
            prev = m.id;
        }
    }

    #[test]
    fn current_storage_v_matches_the_version_constant() {
        assert_eq!(
            current_storage_v(),
            u32::from(sunrise_cbor::version::STORAGE_V),
            "STORAGE_V must equal the last migration id"
        );
    }

    #[test]
    fn baseline_is_the_only_migration() {
        // Guards the reset itself: if a migration is appended, STORAGE_V moves
        // with it and the baseline floor stays where it is.
        assert_eq!(MIGRATIONS.len(), 1);
        assert_eq!(MIGRATIONS[0].id, BASELINE_STORAGE_V);
    }
}
