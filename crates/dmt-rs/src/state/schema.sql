-- Migration state schema for dmt-rs
--
-- NOTE: this file is documentation only — the live schema is created
-- programmatically by `DbStateBackend::init_schema` in `db.rs` (and the
-- MSSQL/MySQL equivalents in the sibling modules). Keep it in sync with
-- the code if either side changes; do NOT include this file at build
-- time via `include_str!`.
--
-- The schema has a single denormalized table `_dmt_rs.table_state` that
-- stores all run-level metadata alongside each per-table row. There is
-- NO separate `migration_runs` table, even though an earlier version of
-- this file (and `docs/tech-specs.md`) described one. See §7 of
-- `docs/benchmark-playbook.md` for the canonical validation query.

-- Per-table state (denormalized with run-level fields)
CREATE TABLE IF NOT EXISTS _dmt_rs.table_state (
    run_id             TEXT        NOT NULL,
    config_hash        TEXT        NOT NULL,
    run_started_at     TIMESTAMPTZ NOT NULL,
    run_completed_at   TIMESTAMPTZ,
    run_status         TEXT        NOT NULL
                       CHECK (run_status IN ('running', 'completed', 'failed', 'cancelled')),
    table_name         TEXT        NOT NULL,
    table_status       TEXT        NOT NULL
                       CHECK (table_status IN ('pending', 'in_progress', 'completed', 'failed')),
    rows_total          BIGINT     NOT NULL DEFAULT 0,
    rows_transferred    BIGINT     NOT NULL DEFAULT 0,
    rows_skipped        BIGINT     NOT NULL DEFAULT 0,
    last_pk             BIGINT,
    last_sync_timestamp TIMESTAMPTZ,  -- For date-based incremental sync
    table_completed_at  TIMESTAMPTZ,
    error               TEXT,
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (run_id, table_name)
);

-- Index for incremental-sync watermark lookups
CREATE INDEX IF NOT EXISTS idx_table_state_incremental_sync
    ON _dmt_rs.table_state(table_name, table_status, last_sync_timestamp)
    WHERE table_status = 'completed' AND last_sync_timestamp IS NOT NULL;

-- Index for latest-run lookups (by config hash, run start time descending)
CREATE INDEX IF NOT EXISTS idx_table_state_latest_run
    ON _dmt_rs.table_state(config_hash, run_started_at DESC);

COMMENT ON TABLE _dmt_rs.table_state IS
    'Per-table migration state for dmt-rs, denormalized with run-level fields. Used for resume and date-based incremental sync.';

COMMENT ON COLUMN _dmt_rs.table_state.last_sync_timestamp IS
    'High-water mark for date-based incremental sync. Set to the run start time after successful completion, so the next run only fetches rows newer than this.';
