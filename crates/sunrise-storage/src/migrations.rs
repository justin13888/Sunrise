//! Schema migrations.
//!
//! Per `docs/04-storage/migrations.md`. Migration scripts are static,
//! embedded at build time, and applied in id order.
//!
//! Adding a migration: bump `STORAGE_V` in `sunrise-cbor::version`, append a
//! new entry to [`MIGRATIONS`], add a new file `migrations/000N_*.sql`.

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

/// All known migrations, in apply order.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        id: 1,
        name: "init",
        sql: include_str!("../migrations/0001_init.sql"),
    },
    Migration {
        id: 2,
        name: "stream_names",
        sql: include_str!("../migrations/0002_stream_names.sql"),
    },
    Migration {
        id: 3,
        name: "scheduling_constraints",
        sql: include_str!("../migrations/0003_scheduling_constraints.sql"),
    },
];

/// Current storage version (= last migration id).
#[must_use]
pub fn current_storage_v() -> u32 {
    MIGRATIONS.iter().map(|m| m.id).max().unwrap_or(0)
}
