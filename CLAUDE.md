# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build Commands

```bash
# Standard release build
cargo build --release

# With MySQL/MariaDB support
cargo build --release --features mysql

# With Terminal UI
cargo build --release --features tui

# With Kerberos authentication (MSSQL only)
cargo build --release --features kerberos

# With AI-powered type mapping
cargo build --release --features ai

# All features
cargo build --release --all-features
```

## Testing

```bash
# Run all tests
cargo test

# Run specific test
cargo test test_name

# Run tests for a specific crate
cargo test -p dmt-rs
cargo test -p dmt-rs-cli

# Run tests with output
cargo test -- --nocapture
```

## Code Quality

```bash
cargo fmt -- --check
cargo clippy --all-targets --all-features
```

## Running

```bash
# Run migration (idempotent: auto-resumes from database state if it exists)
./target/release/dmt-rs -c config.yaml run

# Dry run (validate plan without transferring data)
./target/release/dmt-rs -c config.yaml run --dry-run

# Explicit crash recovery (errors if no state exists in target DB)
./target/release/dmt-rs -c config.yaml resume

# Validate row counts between source and target
./target/release/dmt-rs -c config.yaml validate

# Health check connections
./target/release/dmt-rs -c config.yaml health-check

# Interactive config wizard (use --advanced for tuning options)
./target/release/dmt-rs init
./target/release/dmt-rs init --advanced -o my-config.yaml

# Launch interactive TUI (requires --features tui)
./target/release/dmt-rs tui

# JSON output for Airflow XCom
./target/release/dmt-rs -c config.yaml --output-json run
```

`run` is idempotent: state is stored in the target database (`_dmt_rs` schema). First invocation creates state; subsequent invocations resume / do incremental sync via date watermarks. `resume` is for explicit crash recovery and errors when no state exists.

## Helper Scripts

- `run-all-tests.sh` — runs the 18-permutation source/target/mode integration matrix against real DBs (note: uses a hardcoded binary path that may need editing)
- `benchmark.sh`, `benchmark-rust-only.sh` — performance benchmarks
- `scripts/test-airflow.sh`, `test-dry-run.sh`, `test-health-check.sh`, `test-signals.sh` — targeted scenario tests

## Architecture

This is a Rust workspace with two crates:

- **dmt-rs** (`crates/dmt-rs/`) - Core library
- **dmt-rs-cli** (`crates/dmt-rs-cli/`) - CLI application

### Plugin Architecture (GoF Patterns)

The codebase uses enum dispatch for zero-cost polymorphism instead of trait objects. All readers/writers are wrapped in `Arc<T>` for cheap cloning:

```
SourceReaderImpl (enum)          TargetWriterImpl (enum)
├── Mssql(Arc<MssqlReader>)      ├── Mssql(Arc<MssqlWriter>)
├── Mysql(Arc<MysqlReader>)      ├── Mysql(Arc<MysqlWriter>)
└── Postgres(Arc<PostgresReader>)└── Postgres(Arc<PostgresWriter>)

DialectImpl (enum)
├── Mssql(MssqlDialect)
├── Mysql(MysqlDialect)
└── Postgres(PostgresDialect)
```

To add a new database driver:
1. Create module under `drivers/` (e.g., `drivers/newdb/`)
2. Implement `Dialect`, `SourceReader`, and/or `TargetWriter` traits
3. Add enum variant to `DialectImpl`, `SourceReaderImpl`, `TargetWriterImpl`
4. Register type mappers in `DriverCatalog::with_builtins()`
5. Feature-gate in `Cargo.toml`

### Core Traits (`crates/dmt-rs/src/core/`)

- `SourceReader` - Extract schema, stream rows, parallel reading via PK range splitting
- `TargetWriter` - Create schema, write data (binary COPY), manage constraints
- `Dialect` - SQL syntax generation for different database engines
- `TypeMapper` - Convert types between source and target databases
- `StateBackend` - Persist migration state (Postgres/MSSQL/MySQL/no-op backends)

### Data Flow

```
Config → Auto-Tuning → Schema Extraction → [AI Type Warm-up] → Connection Pools
    → Transfer Engine (parallel readers → read-ahead buffer → parallel writers)
    → Finalization (indexes, FKs, check constraints) → State Persistence
```

### Key Modules

All paths relative to `crates/dmt-rs/src/`:

| Path | Purpose |
|------|---------|
| `drivers/` | Database driver implementations (mssql, postgres, mysql) |
| `drivers/common/` | Shared utilities: TLS, binary COPY parser |
| `transfer/` | Transfer engine with parallel read-ahead/write-ahead |
| `orchestrator/` | Migration workflow coordinator, connection pools |
| `state/` | Migration state backends (postgres, mssql, mysql, no-op) |
| `dialect/` | SQL dialect strategies and cross-database type mapping |
| `core/` | Traits (`SourceReader`, `TargetWriter`, `Dialect`), schema types, value types |
| `config/` | Configuration loading, validation, auto-tuning |
| `ai/` | AI-powered type mapping: LLM providers, cache, prompt construction (feature-gated) |

### Target Modes

- **drop_recreate** - Drop and recreate tables (fastest, destructive)
- **upsert** - INSERT...ON CONFLICT DO UPDATE (incremental sync with date watermarks)

### Error Handling

Custom error types in `src/error.rs` with Airflow-compatible exit codes:
- 0: Success
- 1: Config errors
- 2: Connection errors
- 3: Transfer errors
- 4: Validation errors
- 5: Cancelled (SIGINT/SIGTERM)
- 6: State errors
- 7: IO errors

## Feature Flags

| Feature | Crate | Description |
|---------|-------|-------------|
| `mysql` | dmt-rs | MySQL/MariaDB source support via SQLx |
| `kerberos` | both | MSSQL Kerberos auth via GSSAPI |
| `tui` | dmt-rs-cli | Terminal UI with ratatui |
| `ai` | both | AI-powered type mapping via LLM (Anthropic, OpenAI, Ollama, LM Studio) |

## Dependencies Notes

- Uses a **forked tiberius** with 32KB packet size support (42% faster than default)
- PostgreSQL uses binary COPY protocol for optimal ingestion
- MySQL support is feature-gated to avoid pulling SQLx when not needed
- AI feature adds `reqwest` and `dirs` — feature-gated to avoid bloat for users who don't need it
- `main.rs` installs the **rustls ring** crypto provider before any TLS work — required because `tokio-postgres-rustls` and `mysql_async` both pull rustls with different defaults. Don't remove this initialization.

## AI Type Mapping

When built with `--features ai`, dmt-rs can use LLMs to resolve unknown database types. Configuration lives in a **global config file** (separate from the per-migration config):

```yaml
# ~/.dmt-rs/dmt-rs-config.yaml
ai:
  api_key: ${env:ANTHROPIC_API_KEY}  # or hardcoded (file is chmod 600)
  provider: anthropic                 # anthropic | openai | ollama | lmstudio
  model: claude-haiku-4-5-20251001   # optional, sensible defaults
```

- **Static mapper first**: AI is only consulted for types the static mapper marks as `is_fallback`
- **Warm-up phase**: After schema extraction, unknown types are batch-resolved before DDL generation
- **Persistent cache**: Results stored in `~/.dmt-rs/type-cache.json` — each type resolved once
- **Security**: Config directory `700`, files `600`, warns if permissions too open. AI responses validated against character allowlist to prevent SQL injection in DDL
- **CLI flag**: `--global-config /path/to/config.yaml` overrides the default location

## Project Conventions

- Log via `tracing`; do not use `println!` in library code (`crates/dmt-rs/`).
- Keep modules cohesive: transfer logic in `transfer/`, DB-specific code in `drivers/<db>/`, dialect SQL in `dialect/`.
- Tests live either inline as `#[cfg(test)]` modules or in each crate's `tests/` directory. Tests that hit real databases should be config-gated so `cargo test` stays runnable without infrastructure.
- The CLI defines its subcommands as a `clap` enum in `crates/dmt-rs-cli/src/main.rs` — adding a command means extending that enum and dispatching in `run()`.
