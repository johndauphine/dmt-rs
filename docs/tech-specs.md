# Technical Specifications

This document is the authoritative reference for what `dmt-rs` supports, what it requires, and what guarantees it makes. It complements [`philosophy.md`](philosophy.md) (the "why") and [`design.md`](design.md) (the "how"). The audience is anyone who needs to verify this tool will fit a specific environment before adopting it.

For end-user operational guidance see [`DATA_ENGINEER_GUIDE.md`](DATA_ENGINEER_GUIDE.md).
For raw configuration examples see `../config.example.yaml` and `../examples/`.

## Workspace and crate layout

| Crate | Purpose |
|---|---|
| `dmt-rs` | Core library — all migration logic, public API for embedders |
| `dmt-rs-cli` | CLI binary — clap argument parsing, wizard, optional TUI |

Both crates are in a Rust 2021 workspace at the repo root. MSRV is **1.75**. The CLI is published as a binary; the library is `publish = false` by default but is structured so it could be published independently.

## Build matrix

| Target | Architecture | Build |
|---|---|---|
| Linux | x86_64 | `cargo build --release` |
| Linux | aarch64 | `cargo build --release` |
| macOS | x86_64 | `cargo build --release` |
| macOS | aarch64 (Apple Silicon) | `cargo build --release` |
| Windows | x86_64 | `cargo build --release` |

The output is a single statically-linked binary with no runtime dependencies (no JVM, no Python, no ODBC drivers, no `libpq`). TLS roots are bundled. The default build supports MSSQL and PostgreSQL.

## Feature flags

| Feature | Crate | Default | What it does |
|---|---|---|---|
| `mysql` | `dmt-rs` | off | MySQL/MariaDB source and target via `mysql_async`. Adds `LOAD DATA LOCAL INFILE` for large text tables. |
| `kerberos` | both | off | MSSQL Kerberos auth via GSSAPI. Linux requires `libgssapi-krb5-2`; macOS uses `GSS.framework`; Windows uses SSPI. |
| `tui` | `dmt-rs-cli` | off | Terminal UI for interactive runs (ratatui). The headless CLI must remain fully functional without this. |

Common build commands:

```bash
cargo build --release                           # MSSQL + PostgreSQL only
cargo build --release --features mysql          # + MySQL/MariaDB
cargo build --release --features tui            # + TUI
cargo build --release --features kerberos       # + Kerberos auth
cargo build --release --all-features            # everything
```

## Supported databases

### Source databases

| Engine | Versions | Driver | Notes |
|---|---|---|---|
| Microsoft SQL Server | 2008+ (TDS 7.3+) | tiberius (forked) | Azure SQL also supported. The fork adds 32 KB packet size config (42% throughput improvement vs. default 4 KB). PR: [prisma/tiberius#400](https://github.com/prisma/tiberius/pull/400). |
| PostgreSQL | 12+ | tokio-postgres | Source reads can use `COPY TO BINARY` for ~4-5x faster extraction (`use_copy_binary` config). |
| MySQL / MariaDB | 5.7+ / 10.3+ | mysql_async | Requires `mysql` feature. Source reads use streaming SELECTs. |

### Target databases

| Engine | Versions | Driver | Notes |
|---|---|---|---|
| PostgreSQL | 12+ | tokio-postgres + deadpool | Uses binary `COPY` protocol for inserts. Pooled via `deadpool-postgres`. |
| MSSQL | 2016+ | tiberius | Pooled via `bb8`. |
| MySQL / MariaDB | 5.7+ / 10.3+ | mysql_async | Requires `mysql` feature. Uses batched INSERT or `LOAD DATA LOCAL INFILE` (controlled by `mysql_load_data` config). |

### Authentication

| Method | MSSQL | PostgreSQL | MySQL | Notes |
|---|---|---|---|---|
| Username / password | yes | yes | yes | Default. Fastest. |
| Kerberos / GSSAPI | yes (with `kerberos` feature) | no | no | tokio-postgres lacks GSSAPI support. |
| Azure AD token | no | no | no | Not currently supported. |
| Client certificate | no | no | no | Not currently supported. |

## TLS / encryption

| Engine | Library | Defaults |
|---|---|---|
| MSSQL | rustls (via tiberius's `rustls` feature) | `encrypt: true`, `trust_server_cert: false` |
| PostgreSQL | rustls (via `tokio-postgres-rustls`) | `ssl_mode: require` |
| MySQL | rustls (via `mysql_async`'s `rustls-tls`) | `ssl_mode: disable` (configurable) |

The `main()` of the CLI installs the **rustls ring** crypto provider before any TLS work. This is required because `tokio-postgres-rustls` and `mysql_async` both pull `rustls` with different default features. Removing this initialization breaks all TLS.

PostgreSQL `ssl_mode` accepts: `disable`, `prefer`, `require`, `verify-ca`, `verify-full`.

## Type mapping

Type mapping is canonical: each source type is mapped to a canonical type, and each canonical type has per-target encoders. See `crates/dmt-rs/src/dialect/canonical.rs` and `dialect/typemap.rs` for implementation.

### MSSQL → PostgreSQL

| MSSQL | PostgreSQL | Notes |
|---|---|---|
| `bit` | `boolean` | |
| `tinyint` | `smallint` | MSSQL `tinyint` is unsigned 0–255; widened to `smallint` to fit. |
| `smallint` | `smallint` | |
| `int` | `integer` | |
| `bigint` | `bigint` | |
| `decimal(p,s)` / `numeric(p,s)` | `numeric(p,s)` | Precision and scale preserved. |
| `money` / `smallmoney` | `numeric(19,4)` | |
| `float` | `double precision` | |
| `real` | `real` | |
| `char(n)` | `char(n)` | |
| `varchar(n)` | `varchar(n)` | |
| `varchar(max)` | `text` | |
| `nchar(n)` | `char(n)` | |
| `nvarchar(n)` | `varchar(n)` | |
| `nvarchar(max)` | `text` | |
| `text` / `ntext` | `text` | Deprecated MSSQL types. |
| `binary(n)` / `varbinary(n)` | `bytea` | |
| `varbinary(max)` / `image` | `bytea` | |
| `date` | `date` | |
| `time` | `time` | |
| `datetime` | `timestamp` | Naive (no timezone). |
| `datetime2` | `timestamp` | Naive. Higher precision than `datetime`. |
| `smalldatetime` | `timestamp` | |
| `datetimeoffset` | `timestamptz` | Timezone-aware. |
| `uniqueidentifier` | `uuid` | |
| `xml` | `xml` | |
| `geography` / `geometry` / `hierarchyid` | `text` | UDT columns are read as their textual representation rather than failing the migration. See [`upsert-update-detection-bug.md`](upsert-update-detection-bug.md) and PR #94 for related correctness work. |

Nullable numeric columns are correctly handled as `Option<T>` end-to-end (PR #94).

### MSSQL → MySQL

| MSSQL | MySQL |
|---|---|
| `bit` | `TINYINT(1)` |
| `tinyint` | `TINYINT UNSIGNED` |
| `smallint` | `SMALLINT` |
| `int` | `INT` |
| `bigint` | `BIGINT` |
| `decimal(p,s)` | `DECIMAL(p,s)` |
| `float` | `DOUBLE` |
| `real` | `FLOAT` |
| `varchar(n)` / `nvarchar(n)` | `VARCHAR(n)` |
| `varchar(max)` / `nvarchar(max)` | `LONGTEXT` |
| `text` / `ntext` | `LONGTEXT` |
| `binary` / `varbinary` | `VARBINARY` / `LONGBLOB` |
| `datetime` / `datetime2` | `DATETIME(6)` |
| `datetimeoffset` | `DATETIME(6)` (UTC normalized) |
| `date` | `DATE` |
| `time` | `TIME(6)` |
| `uniqueidentifier` | `CHAR(36)` |

PostgreSQL → MSSQL and PostgreSQL → MySQL mappings exist for the bidirectional case; see the type mapper modules for the full table.

## Target modes

| Mode | Behavior | Use case | Constraints |
|---|---|---|---|
| `drop_recreate` | Drops the target table, recreates schema, bulk-loads via binary COPY. Fastest. | Full refresh, initial loads, dev/test. | Destructive — wipes existing target data. |
| `upsert` | Streams to a staging table, then `INSERT ... ON CONFLICT DO UPDATE` (or equivalent on MSSQL/MySQL). Idempotent. | Incremental sync, production with watermarks. | **Requires a primary key** on every table. Returns `MigrateError::NoPrimaryKey` (exit 4) for tables without one. |

Upsert never deletes rows from the target. If a row was deleted from the source, it stays in the target. This is intentional; see [`philosophy.md`](philosophy.md).

### Date-based incremental sync (upsert only)

When `date_updated_columns` is set in the migration config (e.g. `["LastActivityDate", "ModifiedDate", "UpdatedAt", "CreationDate"]`), each table's first matching column is used as a watermark. Subsequent runs only fetch rows where `<watermark> > last_sync_timestamp`.

Watermark validation (`transfer/mod.rs:DateFilter::new`):
- Rejects timestamps more than 1 hour in the future (clock skew or tampering)
- Rejects timestamps more than ~100 years old (corruption or tampering)
- Formats as ISO 8601 (`YYYY-MM-DD HH:MM:SS.fff`) which contains no SQL metacharacters

Tables with no matching date column fall back to a full sync.

## Configuration reference

Configuration is YAML (or JSON via `-c config.json`). The full schema is defined in `crates/dmt-rs/src/config/types.rs`.

### Top-level structure

```yaml
source:    { ... }    # see below
target:    { ... }    # see below
migration: { ... }    # see below — all fields optional, all auto-tuned by default
```

### `source` section

| Field | Type | Default | Notes |
|---|---|---|---|
| `type` | string | `"mssql"` | One of `mssql`, `postgres`, `mysql`. |
| `host` | string | required | |
| `port` | u16 | engine default | 1433 / 5432 / 3306. |
| `database` | string | required | |
| `user` | string | `""` | Optional for Kerberos. |
| `password` | string | `""` | Never serialized in `Debug` output or config dumps. |
| `schema` | string | `"dbo"` (mssql), `"public"` (pg) | |
| `encrypt` | bool | `true` | MSSQL only. |
| `trust_server_cert` | bool | `false` | MSSQL only. |
| `ssl_mode` | string | `"require"` | PostgreSQL only. |
| `auth` | enum | `native` | One of `native`, `kerberos`. `kerberos` requires the `kerberos` feature. |

### `target` section

Same fields as `source`, with type-specific defaults (`postgres`, `5432`, `public`).

### `migration` section

All fields are optional. Unset numeric fields are auto-tuned from system RAM and CPU.

| Field | Type | Default | Notes |
|---|---|---|---|
| `target_mode` | enum | `drop_recreate` | One of `drop_recreate`, `upsert`. |
| `workers` | usize | auto | Concurrent table workers. |
| `chunk_size` | usize | auto | Rows per read chunk. Auto-tuned from RAM and avg row size. |
| `parallel_readers` | usize | auto | Concurrent readers per large table (PK range splitting). |
| `parallel_writers` (alias: `write_ahead_writers`) | usize | auto | Concurrent writers per worker. |
| `read_ahead_buffers` | usize | auto | Read-ahead chunk channel depth. |
| `max_partitions` | usize | auto | Max PK partitions per large table. |
| `large_table_threshold` | i64 | auto | Min rows to trigger partitioning. |
| `min_rows_per_partition` | i64 | auto | Floor on partition size. |
| `max_mssql_connections` | usize | auto | Max pooled MSSQL connections. |
| `max_pg_connections` | usize | auto | Max pooled PostgreSQL connections. |
| `memory_budget_percent` | u8 | `70` | Percentage of system RAM that buffers may consume. Auto-tuner constrains chunk sizes and read-ahead depth to fit. |
| `include_tables` | `Vec<string>` | `[]` | Glob patterns. Empty = include all. |
| `exclude_tables` | `Vec<string>` | `[]` | Glob patterns. Applied after `include_tables`. |
| `create_indexes` | bool | `false` | Create non-PK indexes after data load. |
| `create_foreign_keys` | bool | `false` | Create FKs after data load. |
| `create_check_constraints` | bool | `false` | Create CHECK constraints after data load. |
| `finalizer_concurrency` | usize | auto | Parallelism for index/FK/constraint creation. |
| `use_binary_copy` | bool | `true` | PostgreSQL targets only. Disabling forces text COPY (slower). |
| `use_unlogged_tables` | bool | `false` | PostgreSQL targets only. UNLOGGED tables are faster but not crash-safe. |
| `copy_buffer_rows` | usize | auto | Rows per COPY buffer flush. |
| `upsert_batch_size` | usize | auto | Rows per upsert batch statement. |
| `upsert_parallel_tasks` | usize | auto | Concurrent upsert workers. |
| `date_updated_columns` | `Vec<string>` | `[]` | Watermark column priority list. Upsert mode only. |
| `mysql_load_data` | enum | `never` | One of `never`, `always`. `always` uses `LOAD DATA LOCAL INFILE` (requires `local_infile=ON` on the server). |
| `compress_text` | bool | `false` | LZ4-compress text columns in the read-ahead buffer. Trades CPU for memory pressure. |

The defaults are documented in `MigrationConfig::default()` (`config/types.rs`).

## CLI reference

```
dmt-rs [GLOBAL OPTIONS] <COMMAND>
```

| Global option | Description |
|---|---|
| `-c, --config <PATH>` | Path to YAML/JSON config file (required for most commands) |
| `--output-json` | Emit machine-readable JSON to stdout (for Airflow XCom etc.) |
| `--log-level <LEVEL>` | Override log level (`trace`, `debug`, `info`, `warn`, `error`) |

### Commands

| Command | Description |
|---|---|
| `run` | Start a migration. Idempotent — auto-resumes from database state if it exists. |
| `run --dry-run` | Validate config and show plan without transferring data. |
| `run --source-schema <NAME>` | Override source schema for this run. |
| `run --target-schema <NAME>` | Override target schema for this run. |
| `run --workers <N>` | Override worker count for this run. |
| `resume` | Explicit crash recovery. Errors if no state exists in the target DB (use `run` for the idempotent path). |
| `validate` | Compare row counts between source and target tables. |
| `health-check` | Test source and target connectivity, return exit code only. |
| `init` | Interactive configuration wizard. |
| `init --advanced` | Wizard with all performance tuning options. |
| `init -o <PATH>` | Output path for the generated config file. |
| `init --force` | Overwrite an existing config file without prompting. |
| `tui` | Launch the interactive terminal UI (requires `tui` feature). |

`run` and `resume` differ in one important way: `run` is idempotent (creates state on first invocation, resumes on subsequent invocations); `resume` errors if no state exists. Use `run` in routine workflows and `resume` only in explicit crash-recovery scenarios.

## Exit codes

Defined in `crates/dmt-rs/src/error.rs`. These are designed for Airflow / Kubernetes / shell-script consumption — each maps to a single error category.

| Code | Constant | Category | Recoverable? |
|---|---|---|---|
| `0` | `EXIT_SUCCESS` | Success | — |
| `1` | `EXIT_CONFIG_ERROR` | Config / YAML / JSON parsing or validation errors | No |
| `2` | `EXIT_CONNECTION_ERROR` | Source DB / target DB / pool errors | Yes |
| `3` | `EXIT_TRANSFER_ERROR` | Transfer or schema-extraction failure | No |
| `4` | `EXIT_VALIDATION_ERROR` | Row count mismatch or missing primary key | No |
| `5` | `EXIT_CANCELLED` | SIGINT / SIGTERM received | Yes |
| `6` | `EXIT_STATE_ERROR` | State backend error or `ConfigChanged` on resume | No |
| `7` | `EXIT_IO_ERROR` | Filesystem I/O failure | Yes |

`MigrateError::is_recoverable()` is the contract for retry policies. Recoverable errors are good candidates for Airflow retries; non-recoverable errors should fail the task immediately.

## State storage

Migration state is stored *in the target database* under a `_dmt_rs` schema. The schema is created programmatically by `DbStateBackend::init_schema` (and the MSSQL/MySQL equivalents) in `crates/dmt-rs/src/state/`; see `crates/dmt-rs/src/state/schema.sql` for a documentation copy of the same layout.

### Tables

The schema is a **single denormalized table** `_dmt_rs.table_state`. All run-level fields (`run_id`, `run_started_at`, `run_completed_at`, `run_status`, `config_hash`) are stored on every per-table row, indexed on `(run_id, table_name)`. There is no separate `migration_runs` table.

```sql
_dmt_rs.table_state
  run_id              text         NOT NULL
  config_hash         text         NOT NULL    -- HMAC-SHA256 of the config (detects drift)
  run_started_at      timestamptz  NOT NULL
  run_completed_at    timestamptz
  run_status          text         NOT NULL    -- 'running' | 'completed' | 'failed' | 'cancelled'
  table_name          text         NOT NULL
  table_status        text         NOT NULL    -- 'pending' | 'in_progress' | 'completed' | 'failed'
  rows_total          bigint
  rows_transferred    bigint
  rows_skipped        bigint
  last_pk             bigint                   -- For resume
  last_sync_timestamp timestamptz              -- For date-based incremental sync
  table_completed_at  timestamptz
  error               text                     -- NULL on success
  updated_at          timestamptz  NOT NULL
  PRIMARY KEY (run_id, table_name)
```

Indexes: one partial index on `(table_name, table_status, last_sync_timestamp) WHERE status='completed'` for fast watermark lookups on incremental sync, and one on `(config_hash, run_started_at DESC)` for latest-run lookups.

### Behavior

- State backends exist for PostgreSQL (`DbStateBackend`), MSSQL (`MssqlStateBackend`), and MySQL (`MysqlStateBackend`, `mysql` feature).
- Multi-instance coordination is via row locking on `_dmt_rs.table_state`.
- Config drift is detected via HMAC-SHA256: if the config changes between runs, `resume` fails with `MigrateError::ConfigChanged`. Use a fresh `run` to start over.
- The schema requires no setup or manual migration — it's idempotently created on first connect.

## Resource requirements

### Memory

Memory consumption is dominated by the read-ahead buffers. Worst case is approximately:

```
chunk_size × parallel_readers × read_ahead × avg_row_size × workers
```

The auto-tuner constrains this to fit within `memory_budget_percent` of system RAM (default 70%). On a 16 GB host the default budget is ~11 GB; on a 4 GB host it shrinks chunks and parallelism accordingly.

Minimum to run: ~512 MB RAM (with auto-tuning kicking in to use small chunks).

### CPU

The transfer engine is CPU-light when both databases are local — most time is spent waiting on network I/O. CPU matters for:
- LZ4 compression (when `compress_text: true`)
- TLS encryption / decryption
- Type encoding / decoding (especially `nvarchar(max)` decoding from UTF-16LE)

The auto-tuner sets `parallel_readers`, `parallel_writers`, and `workers` proportional to CPU core count. A single-core machine can run the tool but throughput is bottlenecked.

### Network

Throughput is bandwidth-limited in most realistic deployments. For reference:

| Bandwidth | Approximate ceiling |
|---|---|
| 1 Gbps | ~80 MB/s sustained |
| 10 Gbps | ~800 MB/s sustained |
| Localhost | Limited by CPU and disk |

The 32 KB packet size optimization (vs. tiberius's default 4 KB) is most impactful at higher bandwidths and on lossy networks where round-trip count matters.

### Disk

The tool itself does no disk I/O on the migration host (no temp files, no spooling). Disk requirements are entirely on the source and target databases.

## Security

- **Passwords are never serialized.** `SourceConfig` and `TargetConfig` implement custom `Debug` that prints `[REDACTED]` for the password field. The `serde` `skip_serializing` attribute prevents accidental dumps via `serde_yaml::to_string` or similar.
- **Identifiers are validated and quoted.** All schema/table/column names flow through `core/identifier.rs`. Even though most identifiers come from `information_schema` queries (already trustworthy), the validation layer is defense in depth.
- **Watermarks are bounds-checked.** `DateFilter::new` rejects timestamps that are obviously wrong, and the SQL-safe formatter avoids any metacharacters.
- **Logs do not contain row data.** Logging is at the table/row-count/timing level. Errors include enough context to debug but never include row contents.
- **TLS is on by default** for MSSQL (`encrypt: true`) and PostgreSQL (`ssl_mode: require`). Disabling either is a deliberate config choice.
- **Static binary** means no runtime library substitution attacks (no `LD_PRELOAD` of `libpq` etc.).

## Limitations

These are deliberate non-features. See [`philosophy.md`](philosophy.md) for the rationale.

- No CDC / log-based replication. Incremental sync is date-watermark batched.
- No DDL versioning or schema migration tracking.
- No row-level transformations / UDFs / pipeline DSL.
- No deletes propagated to the target in `upsert` mode.
- No transactional consistency across the entire migration — long migrations of busy sources end up with rows from different points in time.
- No PostgreSQL Kerberos auth (tokio-postgres lacks GSSAPI).
- No Azure AD / managed identity auth.
- Tables must have a primary key for `upsert` mode (`MigrateError::NoPrimaryKey`, exit 4).
- MSSQL spatial types (`geography`, `geometry`, `hierarchyid`) are read as text rather than failing the migration; users who need fidelity must transform separately.

## Tested versions

- **Rust:** 1.75 (MSRV) through current stable
- **MSSQL:** 2008 R2, 2012, 2014, 2016, 2017, 2019, 2022, Azure SQL DB
- **PostgreSQL:** 12, 13, 14, 15, 16
- **MySQL:** 5.7, 8.0; **MariaDB:** 10.3, 10.6, 11.x
- **OS:** Ubuntu 20.04 / 22.04, Debian 11/12, RHEL 8/9, macOS 13/14, Windows Server 2019/2022, Windows 11

## Related documents

- [`philosophy.md`](philosophy.md) — *why* the tool exists, what it is and is not
- [`design.md`](design.md) — architecture, patterns, where each module fits
- [`DATA_ENGINEER_GUIDE.md`](DATA_ENGINEER_GUIDE.md) — operational guide for end users
- [`mysql-performance-tuning.md`](mysql-performance-tuning.md) — MySQL-specific tuning
- [`mssql-client-spike.md`](mssql-client-spike.md) — alternative MSSQL driver evaluation
- `../PERFORMANCE.md` — benchmark results
- `../config.example.yaml` — annotated config template
