-- 0033: record which features a vault's data depends on, and which features
-- each device supports (issue #324, ADR-0045 §7–§8).
--
-- Why
-- ---
--
-- Nothing told an older client that a vault now used a feature it lacked, so
-- it kept editing entities whose newer fields or op kinds it could not
-- represent, and under full-state ops those edits overwrote data. Two control
-- op families now carry the state, and these two tables are their folds.
--
-- `vault_required_features` is the fold of every applied `VaultRequires` op:
-- a grow-only set, so concurrent enables converge and no row is ever deleted.
-- A build that finds an id here it does not support refuses local writes on
-- that feature's scope (structural for `core.*`, one entity otherwise) and
-- keeps syncing and reading.
--
-- `device_features` is the fold of every applied `DeviceFeatures` op: the
-- latest per device, by HLC, with the encoded feature list as text. A device
-- with no row supports nothing, which is what every build older than this
-- table looks like, so no feature is enabled over it without the user being
-- told.
--
-- Columns
-- -------
--
-- `features` holds the ids joined by newlines. A feature id is
-- `[a-z][a-z0-9_]*(\.[a-z0-9_]+)+`, so it never contains one; the list is
-- sorted and de-duplicated before it is written, so equal sets are equal text,
-- which is also the tie-break between two ops with the same stamp.
CREATE TABLE vault_required_features (
    feature         TEXT PRIMARY KEY,
    recorded_at_ms  INTEGER NOT NULL
);

CREATE TABLE device_features (
    device_id       BLOB PRIMARY KEY,
    features        TEXT NOT NULL,
    hlc_ms          INTEGER NOT NULL,
    hlc_logical     INTEGER NOT NULL,
    recorded_at_ms  INTEGER NOT NULL
);
