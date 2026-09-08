//! Schema migrations.
//!
//! Per `docs/04-storage/migrations.md`. Migration scripts are static, embedded
//! at build time, and applied in id order.
//!
//! The list starts at the `0013_baseline.sql` schema reset (ADR-0018) and
//! grows by appending, which is what that ADR reinstated: `0014` is the first
//! migration appended after it, and 0013 was not touched to make room for it.
//!
//! Adding a migration: bump `STORAGE_V` in `sunrise-cbor::version`, append a
//! new entry to [`MIGRATIONS`], add a new file `migrations/00NN_*.sql`. Never
//! edit an existing one.

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
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        id: BASELINE_STORAGE_V,
        name: "baseline",
        sql: include_str!("../migrations/0013_baseline.sql"),
    },
    Migration {
        id: 14,
        name: "stream_sort_order",
        sql: include_str!("../migrations/0014_stream_sort_order.sql"),
    },
    Migration {
        id: 15,
        name: "entity_extra_columns",
        sql: include_str!("../migrations/0015_entity_extra_columns.sql"),
    },
    Migration {
        id: 16,
        name: "stream_description_and_default_context",
        sql: include_str!("../migrations/0016_stream_description_and_default_context.sql"),
    },
    Migration {
        id: 17,
        name: "key_hierarchy",
        sql: include_str!("../migrations/0017_key_hierarchy.sql"),
    },
    Migration {
        id: 18,
        name: "key_envelope_recipients",
        sql: include_str!("../migrations/0018_key_envelope_recipients.sql"),
    },
    Migration {
        id: 19,
        name: "identity_minted_by",
        sql: include_str!("../migrations/0019_identity_minted_by.sql"),
    },
    Migration {
        id: 20,
        name: "relay_revocation_intents",
        sql: include_str!("../migrations/0020_relay_revocation_intents.sql"),
    },
    Migration {
        id: 21,
        name: "ops_by_ts",
        sql: include_str!("../migrations/0021_ops_by_ts.sql"),
    },
];

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
    fn the_list_still_starts_at_the_baseline() {
        // Guards the reset itself. Migrations append after 0013; the floor
        // stays where ADR-0018 put it, so a pre-baseline vault is still
        // refused rather than half-upgraded by whatever was appended since.
        assert_eq!(MIGRATIONS[0].id, BASELINE_STORAGE_V);
        assert!(MIGRATIONS[1..].iter().all(|m| m.id > BASELINE_STORAGE_V));
    }

    #[test]
    fn every_migration_file_is_distinct() {
        // A copy-pasted `include_str!` would apply one file twice and skip
        // another, which the ascending-id check above cannot see.
        for (i, a) in MIGRATIONS.iter().enumerate() {
            for b in &MIGRATIONS[i + 1..] {
                assert_ne!(a.sql, b.sql, "{} and {} share their SQL", a.name, b.name);
                assert_ne!(a.name, b.name);
            }
        }
    }
}
