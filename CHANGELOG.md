# Changelog

All notable changes to this project will be documented in this file.

## [1.46.0] - 2026-04-19

### Features
- **AI-powered error diagnosis** (#123, #124, #125) — Mirrors Go dmt's `ai_errordiag.go`. When AI is configured, failed DDL operations (CREATE TABLE / PRIMARY KEY / INDEX / FK / CHECK across both `prepare_target` and `finalize`) and transfer writer errors are sent to the configured LLM with schema context; the structured response (cause / 2-3 suggestions / confidence / 6-category taxonomy) is cached by SHA-256 error hash and emitted via a pluggable handler. Activation: no flag or new config field — reuses the existing `ai:` section in `~/.dmt-rs/dmt-rs-config.yaml`.
  - Walks the full `error.source()` chain so the LLM sees the real detail (e.g., `"permission denied for schema public"`) instead of just the top-level `Display` message (`"db error"`).
  - Per-key `tokio::sync::Mutex` in-flight dedup: N parallel spawned tasks hitting the same error fire exactly one provider request, not N.
  - TUI mode registers a diagnosis handler that renders each diagnosis as structured transcript entries (header + cause + numbered suggestions); CLI mode falls back to a boxed `tracing::warn!` rendering. Diagnosis in TUI is non-blocking — transfer-failure diagnoses spawn in the background so they don't delay cancellation or sibling shutdown.

## [1.45.1] - 2026-04-19

### Build
- **Release binaries now ship with `mysql`, `tui`, and `ai` features enabled.** Prior releases used default features only (`default = []`), so downloaded binaries failed at runtime for any MySQL target with "MySQL target requires the 'mysql' feature." The `kerberos` feature is still off in releases to avoid GSSAPI cross-compile complexity on linux-arm64 — build from source if you need it.

### Documentation
- Consolidated 6 benchmark docs into `docs/benchmarks.md` (current numbers, performance model, reproduction) and `docs/benchmarks-archive.md` (historical experiments with "superseded by" pointers). The former "120 K MySQL target ceiling" finding is corrected — v1.45 measures 369-452 K rows/s.

## [1.45.0] - 2026-04-19

### Performance
- **Inline PRIMARY KEY for Postgres targets** (#122) — PK emitted as `CONSTRAINT ... PRIMARY KEY` inside `CREATE TABLE` instead of a trailing `ALTER TABLE ADD CONSTRAINT` at finalize. On pg→mysql SO2010 (19.3M rows), PK finalize phase drops to ~0.1s total across 9 tables.
- **Inline PRIMARY KEY for MySQL targets** (#121) — same treatment for MySQL/InnoDB, where heap is PK-clustered so inline PK avoids a clustered-index rebuild at finalize.

### Refactor
- **Remove `mysql_bulk_session_tuning` config knob** (#120) — always-on; the knob offered no useful tradeoff in benchmarks.

### Documentation
- M3 Max 36 GB / 10 GiB MSSQL RAM experiment playbook and confirmed results.
- 16 GB Docker VM finding: ~0 throughput win, ~4× tighter variance than 9 GiB.
- MySQL buffer-pool sizing experiment — 2 GB is the sweet spot.
- LOAD DATA A/B re-validation on tuned container (still loses).

## [1.44.0] - 2026-04-17

### Features
- **Share state across `target_mode`** (#108) — `config_hash` now excludes `target_mode`, so a `drop_recreate` run seeds watermarks that a subsequent `upsert` run inherits. Post-drop upsert with no source changes completes in ~6s instead of paying the full first-upsert tax.
- **Dialect-specific AI prompt augmentation** (#106) — AI type mapping prompts now include dialect-specific context (type quirks, reserved words) for better cross-DB type resolution.
- Seeded `date_updated_columns` defaults in the 18 bundled test configs for out-of-the-box incremental sync.

### Bug Fixes
- **Cross-engine `datetime2` → `datetime`** (#109) — MSSQL targets now use `datetime` rather than `datetime2` when the source is a different engine, working around a bulk-insert driver issue with `datetime2`.
- **MSSQL state init order** (#107) — state schema is now initialized before any writes; previously could race during cold-start.
- **MySQL source TLS** — fixed TLS handshake failures when MySQL is the source.
- **AI config on resume** (#105) — `dmt-rs resume` now loads the AI global config; previously only `run` did.
- **Incremental sync watermark / resume** — multiple fixes to date-watermark handling during resume, including identity-mapper and datetime bulk-insert edge cases.

### Tests / Chore
- Standardized all 18 test configs (#109) — consistent formatting, shared `TestPass2024` password, hash-parity between drop/upsert variant pairs.

### Documentation
- M5 Pro cgroup-cap benchmark findings — Config E (41.1s `mssql→mssql` on 12 GiB VM with per-container 5 GiB cgroup cap) added to the cross-hardware table.
- Incremental upsert-after-drop playbook section (§11).

## [1.43.0] - 2026-04-14

### Features
- **AI-Powered Type Mapping** (#103) - LLM-backed type mapping for unknown/exotic database types
  - Supports Anthropic, OpenAI, Ollama, and LM Studio providers
  - Persistent JSON cache at `~/.dmt-rs/type-cache.json` — each type resolved once
  - Feature-gated behind `--features ai` for zero impact on default binary
  - Global config at `~/.dmt-rs/dmt-rs-config.yaml` with `--global-config` CLI flag
  - Secure file permissions: directory `700`, files `600`, warns if too open
  - SQL injection protection: AI responses validated against character allowlist

### Bug Fixes
- **Orchestrator Writer Failure Propagation** (#102) - Fixed #97: writer failures now cancel sibling partitions immediately via shared per-table `CancellationToken`. Previously, a writer failure in one partition would cascade into pool exhaustion and data loss.
- **MSSQL Upsert Oversized Strings** (#101) - Check `CompressedText` in oversized string detection for MSSQL upsert staging path
- **MSSQL Upsert Pool Timeout** (#100) - Cap `parallel_writers` to 1 for MSSQL upsert to prevent bb8 pool timeout (MERGE WITH TABLOCK serializes writers at DB level)

### Documentation
- Full 8-cell Go vs Rust benchmark comparison on identical infrastructure
- Updated benchmark doc with fair same-session numbers

## [1.41.0] - 2026-01-24

### Features
- **MySQL Target Support** - Full MySQL/MariaDB target support using mysql_async driver
  - Batched INSERT statements for reliable bulk loading
  - Optional LOAD DATA LOCAL INFILE for large text tables (`mysql_load_data: always`)
  - SSL/TLS support with multiple modes (disable, prefer, require, verify-ca, verify-full)
  - Migration state storage in MySQL target database
  - Complete MSSQL → MySQL type mapping

### Performance
- **MySQL Performance Tuning** - Extensive benchmarking of MySQL bulk load strategies
  - Batched INSERT: 33,387 rows/sec (optimal for parallel workloads)
  - LOAD DATA: 34,231 rows/sec with 1 worker (2% faster for single-threaded)
  - Added `mysql_load_data` config option (never/always)
  - See [`docs/benchmarks-archive.md`](docs/benchmarks-archive.md) §4 for the tuning history

### Bug Fixes
- **drop_recreate State Handling** - Fixed bug where drop_recreate mode would skip tables marked as completed in state from previous runs. Now always processes all tables in drop_recreate mode.

### Tests
- Added unit tests for drop_recreate vs upsert state initialization behavior
- Added tests for table filtering logic in different target modes

## [0.8.10] - 2025-12-29

### Performance
- **Batched INSERT Fallback** - Oversized string fallback now uses multi-row INSERT statements instead of single-row
  - Respects both MSSQL limits: 2100 parameters AND 1000 rows per VALUES clause
  - Batch size: `min(2100/columns, 1000)`
  - Expected 10-20x improvement for tables with many oversized strings (e.g., SO2013 Posts.Body)
  - Examples: 10-column table → 210 rows/batch; 2-column table → 1000 rows/batch (capped)

### Bug Fixes
- Proper error handling for zero-column edge case in INSERT fallback

### Tests
- Added unit test for batch size calculation with 1000-row cap verification

## [0.8.9] - 2025-12-29

### Performance
- **MSSQL Upsert 100x Faster** - Complete rewrite using staging table approach
  - Bulk insert to staging table → single MERGE to target
  - First pass: ~500 rows/sec → **95,873 rows/sec**
  - Second pass (no changes): **157,057 rows/sec**

### Bug Fixes
- **Zero Deadlocks** - Added `WITH (TABLOCK)` hint to MERGE statements to prevent S→X lock conversion deadlocks
- **Reliable Deadlock Detection** - Uses tiberius built-in `is_deadlock()` instead of string matching
- **SQL Injection Prevention** - Parameterized queries with QUOTENAME for staging table existence check

### Improvements
- Staging tables use target schema with `_staging_[table]_[writerid]` naming (no separate staging schema)
- Linear backoff retries: 5 attempts with 200ms base delay (200ms, 400ms, 600ms, 800ms, 1000ms)
- Added `PartialEq` derive to `SqlValue` and `SqlNullType` for test assertions

### Tests
- Row partitioning logic (bulk-insertable vs oversized strings)
- NULL-safe change detection pattern verification
- Multiple non-PK column handling in MERGE
- Deadlock detection with non-server errors

## [0.8.8] - 2025-12-29

### Bug Fixes
- **Large String Fallback** - Strings exceeding TDS bulk insert limit (65535 UTF-16 bytes) now fall back to parameterized INSERT instead of failing (#51)

### Improvements
- Added `row_has_oversized_strings()` helper to detect rows needing INSERT fallback
- Added unit tests for oversized string detection with various edge cases

## [0.8.7] - 2025-12-29

### Bug Fixes
- **PostgreSQL to MSSQL type mapping fixes** - Fixed ambiguous type detection that caused incorrect column types
  - `text` now correctly maps to `nvarchar(max)` instead of deprecated MSSQL `text` type
  - `varchar` now correctly maps to `nvarchar` for Unicode support
  - `char` now correctly maps to `nchar` for Unicode support
  - `date` now maps to `datetime2` to work around Tiberius bulk insert DATE serialization issues

### Improvements
- Improved comments documenting intentionally excluded types from `is_mssql_type()` check
- Updated type mapping comments to explain rationale for date->datetime2 conversion

## [0.8.6] - 2025-12-29

### Performance
- **TDS Bulk Insert for MSSQL Target** - 6x faster data loading (~180,000 rows/sec) using native TDS bulk insert protocol instead of INSERT statements (#48)

### Bug Fixes
- **JSONB handling in PG to MSSQL migration** - Fixed JSONB values becoming NULL when migrating from PostgreSQL to MSSQL (#47)

### Security
- Fixed SQL injection vulnerability in `create_schema()` using parameterized queries

### Improvements
- Added bounds checking for date conversions to prevent overflow
- Added warning logs for NaN/Infinity and out-of-range date conversions
- Added unit tests for bulk insert type conversions
- IDENTITY columns converted to regular INT/BIGINT for data warehouse use (enables bulk insert)

### Breaking Changes
- MSSQL target tables no longer have IDENTITY columns (intentional for data warehouse use case)

## [0.8.5] - 2025-12-29

### Changes
- Simplified codebase architecture
- Documentation updates for performance and benchmarks

## [0.8.4] - 2025-12-29

### Changes
- Fast upsert mode enabled by default
- Performance optimizations for upsert operations

## [0.8.3] - 2025-12-29

### Bug Fixes
- Fixed nvarchar(max) hash detection issues

## [0.8.2] - 2025-12-28

### Performance
- Verification performance improvements

## [0.8.1] - 2025-12-28

### Features
- Added `hash_text_columns` performance option for large text column handling

## [0.8.0] - 2025-12-28

### Features
- **Bidirectional Migration Support** - Migrate from MSSQL to PostgreSQL and PostgreSQL to MSSQL
- PostgreSQL source pool implementation
- MSSQL target pool implementation
- Type mapping for both directions

## [0.7.1] - 2025-12-28

### Bug Fixes
- Fixed wizard default values

## [0.7.0] - 2025-12-28

### Features
- Batch hash verification for data integrity checks

## [0.3.0] - 2025-12-27

### Features
- Interactive TUI mode for guided migration setup
