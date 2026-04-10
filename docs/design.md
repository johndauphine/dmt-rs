# Design

This document describes how `mssql-pg-migrate` is built. It complements [`philosophy.md`](philosophy.md) (which explains *why* the tool exists) and [`tech-specs.md`](tech-specs.md) (which lists *what* the tool supports). The audience is contributors who need to understand the architecture before changing it.

For end-user operational guidance see [`DATA_ENGINEER_GUIDE.md`](DATA_ENGINEER_GUIDE.md).

## Workspace structure

```
crates/
├── mssql-pg-migrate/         Core library — all migration logic lives here
│   └── src/
│       ├── core/              Trait definitions, schema types, value types
│       ├── config/            Configuration loading, validation, auto-tuning
│       ├── orchestrator/      Migration workflow coordinator + connection pools
│       ├── transfer/          Parallel read-ahead/write-ahead transfer engine
│       ├── drivers/           Per-database implementations (mssql/postgres/mysql)
│       │   └── common/        Shared TLS, binary COPY parser, helpers
│       ├── dialect/           SQL dialect strategies, cross-DB type mapping
│       ├── state/             Migration state backends (postgres/mssql/mysql/no-op)
│       ├── source/            Re-exports from core::schema (legacy compat shim)
│       ├── target/            Re-exports from core (legacy compat shim)
│       └── error.rs           Custom error types + Airflow-compatible exit codes
└── mssql-pg-migrate-cli/     CLI binary — argument parsing, wizard, TUI, output
```

The library/CLI split exists so other tools can embed the migration engine without pulling in clap, ratatui, or the wizard UI. The CLI is a thin wrapper.

## Core abstractions

Three traits define the entire surface area of database support. Adding a new database engine means implementing these three traits and registering them with the catalog.

```rust
// crates/mssql-pg-migrate/src/core/traits.rs

pub trait SourceReader: Send + Sync {
    async fn extract_schema(&self, schema: &str) -> Result<Vec<Table>>;
    async fn load_indexes(&self, table: &mut Table) -> Result<()>;
    async fn load_foreign_keys(&self, table: &mut Table) -> Result<()>;
    async fn load_check_constraints(&self, table: &mut Table) -> Result<()>;
    fn read_table(&self, opts: ReadOptions) -> mpsc::Receiver<Result<Batch>>;
    async fn get_partition_boundaries(&self, table: &Table, num_partitions: usize) -> Result<Vec<Partition>>;
    async fn get_row_count(&self, schema: &str, table: &str) -> Result<i64>;
    async fn get_max_pk(&self, schema: &str, table: &str, pk_col: &str) -> Result<i64>;
    fn db_type(&self) -> &str;
    async fn close(&self);
}

pub trait TargetWriter: Send + Sync {
    // schema ops: create_schema, create_table, create_table_unlogged, drop_table, table_exists
    // constraint ops: create_primary_key, create_index, create_foreign_key, create_check_constraint
    // data ops: write_batch, upsert_batch
    // utility: has_primary_key, get_row_count, reset_sequence, set_table_logged/unlogged
    // ...
}

pub trait Dialect {
    fn name(&self) -> &str;
    fn quote_ident(&self, name: &str) -> String;
    fn build_select_query(&self, opts: &SelectQueryOptions) -> String;
    fn build_upsert_query(&self, target: &str, staging: &str, cols: &[String], pks: &[String]) -> String;
    fn param_placeholder(&self, index: usize) -> String;
    fn build_keyset_where(&self, pk_col: &str, last_pk: i64) -> String;
    fn build_row_number_query(&self, inner: &str, pk_col: &str, start: i64, end: i64) -> String;
}
```

`SourceReader::read_table` returns an `mpsc::Receiver<Result<Batch>>` rather than a stream. This is deliberate: it lets the reader spawn a background task that produces batches with **backpressure** — when the channel buffer fills, the reader naturally stops pulling from the database. This is the foundation of the read-ahead pipeline (see "Transfer engine" below).

## Plugin architecture: enum dispatch over `dyn Trait`

The drivers do not use `Box<dyn SourceReader>`. Instead, every driver is wrapped in an enum:

```rust
// crates/mssql-pg-migrate/src/drivers/mod.rs

pub enum SourceReaderImpl {
    Mssql(Arc<MssqlReader>),
    Postgres(Arc<PostgresReader>),
    #[cfg(feature = "mysql")]
    Mysql(Arc<MysqlReader>),
}

pub enum TargetWriterImpl {
    Mssql(Arc<MssqlWriter>),
    Postgres(Arc<PostgresWriter>),
    #[cfg(feature = "mysql")]
    Mysql(Arc<MysqlWriter>),
}

pub enum DialectImpl {
    Mssql(MssqlDialect),
    Postgres(PostgresDialect),
    #[cfg(feature = "mysql")]
    Mysql(MysqlDialect),
}
```

`SourceReader` is `impl`'d on `SourceReaderImpl` directly — every method is a `match` over the variants. The compiler generates a static dispatch table at every call site instead of going through a vtable.

**Why this matters:** The transfer engine calls `read_table` and `write_batch` in a hot loop. Devirtualization at compile time is measurable for the read-ahead/write-ahead pipeline. We accept the maintenance cost (every new driver requires touching three enums) in exchange for zero-cost polymorphism.

`Arc<T>` wrapping makes the enum cheaply cloneable so the orchestrator can pass it across spawned tasks without lifetime gymnastics.

### Adding a new database driver

The recipe lives in `drivers/mod.rs:23-30` but in summary:

1. Create a module under `drivers/<engine>/`
2. Implement `Dialect`, `SourceReader`, and/or `TargetWriter`
3. Add a variant to each of `DialectImpl`, `SourceReaderImpl`, `TargetWriterImpl`
4. Register type mappers in `DriverCatalog::with_builtins()` (`core/catalog.rs`)
5. Feature-gate the module in `Cargo.toml` if appropriate (e.g. mysql)

The fact that this is mechanical and self-contained is the payoff for the enum-dispatch design. There is no central "driver registry" that needs to be discovered at runtime — the compiler enforces that every new variant handles every trait method.

## Transfer engine: parallel read-ahead → write-ahead

The transfer engine in `transfer/mod.rs` is the hot path. Its job is to move rows from a `SourceReader` to a `TargetWriter` as fast as the slower of the two will allow, with no idle CPU on either side.

```
                        ┌───────────────────────────────────────────────┐
                        │                  Transfer Engine               │
                        │                                                │
   ┌──────────┐         │   ┌───────────┐    ┌──────────┐    ┌─────────┐│        ┌──────────┐
   │  Source  │ ──read──┼─▶ │ Parallel  │ ─▶ │  Bounded │ ─▶ │Parallel ││ ─write▶│  Target  │
   │ database │         │   │  Readers  │    │ mpsc     │    │ Writers ││         │ database │
   └──────────┘         │   │ (PK split)│    │ channel  │    │  pool   ││         └──────────┘
                        │   └───────────┘    └──────────┘    └─────────┘│
                        │       ▲                                  │    │
                        │       │       backpressure when full     │    │
                        │       └──────────────────────────────────┘    │
                        └───────────────────────────────────────────────┘
```

### Parallel readers via PK range splitting

For tables with a numeric primary key, `SourceReader::get_partition_boundaries` divides the PK range into N partitions (where N = `parallel_readers`). Each partition becomes an independent `WHERE pk BETWEEN min AND max` query running in its own task.

This is **keyset pagination**, not OFFSET/FETCH. The query plans are stable, the source only needs index seeks, and there's no per-chunk cost growth as the migration progresses.

For tables without a single-column numeric PK, the engine falls back to `ROW_NUMBER()` partitioning (see `Dialect::build_row_number_query`). It's slower, but it works.

### Read-ahead buffering

Each reader produces `Batch` chunks into a bounded `mpsc` channel. The buffer depth (`read_ahead`, default 16 chunks) is sized so that if writes pause briefly the readers don't block, but if writes pause for long the channel fills and the readers stop pulling from the source. This is how we get backpressure without a complex flow-control protocol.

### Writer pool

A pool of writer tasks pulls from the channel and calls `TargetWriter::write_batch` (or `upsert_batch`) in parallel. PostgreSQL targets use binary COPY (`drivers/common/binary_copy.rs`) which is dramatically faster than parameterized INSERTs. MySQL targets optionally use `LOAD DATA LOCAL INFILE` for very large text tables.

### Range tracking for safe resume

Because writes complete out of order across partitions, the engine maintains a `RangeTracker` (`transfer/mod.rs:206`) that records contiguous completed PK ranges and reports a "safe resume point" — the highest PK below which *every* prior PK is known-written. On a crash, resume picks up from this point. This prevents data loss from naively recording the highest PK seen (which would skip rows in incomplete earlier partitions).

### Optional in-flight LZ4 compression

For tables with large text columns, the engine can LZ4-compress strings as they leave the reader and decompress them at the writer. This trades CPU for memory pressure and is enabled via `compress_text` in `TransferConfig`. It is off by default — turn it on for tables where the read-ahead buffer would otherwise blow the memory budget.

## Orchestrator: workflow coordinator

`orchestrator/mod.rs` is the highest-level layer. It owns the migration workflow:

```
1. Load and validate config             (config/)
2. Detect system resources              (sysinfo)
3. Apply initial auto-tuning            (config::with_auto_tuning)
4. Build connection pools               (orchestrator/pools.rs)
5. Health-check connections             (SourceReader/TargetWriter)
6. Extract source schema                (SourceReader::extract_schema)
7. Refine auto-tuning from stats        (config::apply_auto_tuning_from_tables)
8. Persist run + initial state          (StateBackend::start_run)
9. For each table:
     a. Check if table should be skipped (state, watermark)
     b. Compute partition boundaries     (parallel reading)
     c. Run TransferEngine::execute      (the hot loop)
     d. Update state with progress
10. Finalization phase:
     - Create indexes in parallel
     - Create foreign keys
     - Create check constraints
     - Reset sequences (PG SERIAL/IDENTITY columns)
11. Persist run completion and stats
```

Steps 9 and 10 are the only ones with parallelism inside them. Everything else is sequential to keep the workflow understandable. The wins from parallelizing schema extraction or pool setup would be tiny compared to the data transfer itself.

## Auto-tuning

`config::Config::with_auto_tuning` and `apply_auto_tuning_from_tables` (in `config/types.rs`) compute reasonable defaults for `workers`, `chunk_size`, `parallel_readers`, `parallel_writers` based on:

- Total system RAM (via `sysinfo`)
- CPU core count
- A configurable `memory_budget_percent` (default 70)
- Average row size (estimated up front, refined after schema extraction with actual table stats)

The two-stage refinement matters: before schema extraction we don't know how big rows are, so we use a 500-byte default. After we see actual `(row_count, estimated_row_size)` per table, we recompute a weighted average and re-tune. This means tables with very wide rows (e.g. lots of nvarchar(max)) get smaller chunks automatically.

Any explicitly-set value in the config file takes precedence over auto-tuning. The user is always in charge.

## State backend: idempotent runs

State storage is abstracted via the `StateBackend` trait (`state/backend.rs`). Implementations:

- `DbStateBackend` — PostgreSQL (the production default)
- `MssqlStateBackend` — MSSQL targets
- `MysqlStateBackend` — MySQL targets (feature-gated)

State is stored *in the target database* under a `_mssql_pg_migrate` schema. This is a deliberate design choice — see the rationale in [`philosophy.md`](philosophy.md) under "Idempotent by default":

- Atomic with data writes (same transaction guarantees)
- Survives container/pod restarts without external persistent volumes
- Multi-instance coordination via row locks
- No filesystem dependencies → works in restricted environments
- The audit trail is queryable with SQL the user already knows

The schema is in `state/schema.sql`. It's initialized on first run and migrated forward as needed. State integrity is protected by HMAC-SHA256 of the config (`config_hash`) — if the user changes the config and tries to resume, the engine refuses (`MigrateError::ConfigChanged`) rather than silently doing the wrong thing.

### Date-based incremental sync

Upsert mode uses *date watermarks*: the engine looks for the first available date column from a configurable priority list (e.g. `LastActivityDate`, `ModifiedDate`, `UpdatedAt`, `CreationDate`). If found, subsequent runs only fetch rows where `<watermark_col> > last_sync_timestamp`. This is what makes routine incremental syncs of an unchanged 19M-row dataset complete in seconds rather than minutes.

`DateFilter::new` (`transfer/mod.rs:44`) validates the timestamp bounds (rejects future timestamps and timestamps older than ~100 years) and `timestamp_sql_safe()` formats it as ISO 8601 with no SQL metacharacters. This is one of two layers of SQL injection defense for the watermark path; the other is column-name validation in `core/identifier.rs`.

## Error model

Errors are an enum (`error::MigrateError`) where each variant maps to a specific Airflow-compatible exit code:

| Variant | Exit code | Recoverable? |
|---|---|---|
| `Config`, `Yaml`, `Json` | 1 | No |
| `Source`, `Target`, `Pool` | 2 | Yes |
| `Transfer`, `SchemaExtraction` | 3 | No |
| `Validation`, `NoPrimaryKey` | 4 | No |
| `Cancelled` (SIGINT/SIGTERM) | 5 | Yes |
| `State`, `ConfigChanged` | 6 | No |
| `Io` | 7 | Yes |

`is_recoverable()` is the contract for retry policies in DAGs and shell scripts. `format_detailed()` walks the error chain so wrapped errors don't lose context. `error_type()` produces a stable string for JSON output (`--output-json`).

The "no" entries are the important ones: a config error or schema extraction error will not be fixed by retrying, and we communicate that explicitly so retry loops don't waste time and budget.

## Identifier safety

All user-supplied identifiers (table names, column names, schema names) flow through `core/identifier.rs`:

- `validate_identifier` rejects anything outside `[A-Za-z0-9_]` plus a length limit
- Per-dialect quoting: `quote_pg`, `quote_mysql`, `quote_mssql`
- Per-dialect qualification: `qualify_pg`, etc.
- `validate_check_constraint` for the harder case of CHECK clauses

This is defense in depth — even though most identifiers come from `information_schema` queries (already trustworthy), the validation layer means we never concatenate raw strings into SQL anywhere downstream.

## Test architecture

Two layers:

- **Unit tests** live inline as `#[cfg(test)]` modules next to the code under test
- **Integration tests** live in each crate's `tests/` directory

Tests that hit real databases must be config-gated so `cargo test` is runnable on a developer machine without infrastructure. The 18-permutation source/target/mode integration matrix is driven by `run-all-tests.sh` against real Docker-hosted databases.

## Where each module fits

| Module | What lives here | Touch this when… |
|---|---|---|
| `core/traits.rs` | `SourceReader`, `TargetWriter`, `Dialect`, `TypeMapper` | Adding methods that all drivers must implement |
| `core/schema.rs` | `Table`, `Column`, `Index`, `ForeignKey`, `CheckConstraint`, `Partition`, `PkValue` | Changing the metadata model |
| `core/value.rs` | `SqlValue`, `Batch`, compression thresholds | Changing the in-flight value representation |
| `core/identifier.rs` | Identifier validation + quoting | Adding a new dialect or tightening security |
| `core/catalog.rs` | `DriverCatalog` registration | Wiring up a new driver or type mapper |
| `drivers/<engine>/` | Engine-specific reader, writer, dialect, type encoders | Per-engine logic only |
| `drivers/mod.rs` | The three dispatch enums | Adding a new driver variant |
| `dialect/canonical.rs` | Canonical type representation | Cross-DB type mapping changes |
| `dialect/typemap.rs` | Per-pair type mapping rules | Adding `mssql→postgres` or similar pair |
| `transfer/mod.rs` | The hot loop + range tracking | Performance work, parallelism changes |
| `orchestrator/mod.rs` | Workflow sequencing | Adding new run phases |
| `orchestrator/pools.rs` | Connection pool construction | Pool config, TLS, auth |
| `state/*.rs` | State backends + run/table state model | New backends, schema migrations |
| `config/types.rs` | Config structs, auto-tuning math | New config knobs |
| `config/validation.rs` | Config sanity checks | New validation rules |
| `error.rs` | Error enum + exit codes | New error categories |

## Things not in the codebase but worth knowing

- **No async runtime abstraction.** We are tied to Tokio. The drivers, the channels, and the orchestrator all assume Tokio. If we ever needed to support `smol` or `async-std`, that would be a real architectural change, not a flag.
- **No DI framework.** Wiring is explicit. The orchestrator constructs the readers, writers, state backend, and engine in `run()` and passes them down by reference or by `Arc`. No service locator.
- **No macros for trait dispatch.** `enum_dispatch` is mentioned in `drivers/mod.rs` comments but the actual code uses hand-written `match` arms because cross-module trait complexities made the macro brittle. The performance is identical and the explicitness is worth the verbosity.
- **No `unsafe` in our crates.** We rely on `unsafe` in dependencies (tiberius, tokio-postgres, ring) but the application code itself does not use it. If you find yourself reaching for `unsafe`, the answer is almost always "no".

## Related documents

- [`philosophy.md`](philosophy.md) — *why* the tool exists, what it is and is not
- [`tech-specs.md`](tech-specs.md) — supported versions, type mappings, configuration reference
- [`DATA_ENGINEER_GUIDE.md`](DATA_ENGINEER_GUIDE.md) — operational guide for end users
- [`mssql-client-spike.md`](mssql-client-spike.md) — evaluation of an alternative MSSQL driver
- `../PERFORMANCE.md` — benchmark results and tuning guidance
