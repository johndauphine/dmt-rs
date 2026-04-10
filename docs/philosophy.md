# Philosophy

This document explains *why* `mssql-pg-migrate` exists, what it is, and — equally important — what it is **not**. It describes the values and tradeoffs that should guide every design decision in the codebase.

## Why this tool exists

The original problem: moving large datasets between SQL Server and PostgreSQL (and later MySQL) inside scripted, headless environments — Kubernetes jobs, Airflow DAGs, CI pipelines, on-prem batch jobs — without:

- A persistent service to babysit
- A GUI to drive
- A managed cloud product to pay for
- A JVM, Python interpreter, or ODBC driver matrix to install

Off-the-shelf options each missed at least one of these. Commercial ETL platforms (Informatica, Talend, Fivetran, Airbyte) are too heavy and too operational. Hand-written Python scripts are too slow and too fragile. SSIS doesn't run outside Windows. ODBC-based pipelines require external runtime dependencies that defeat the "drop a static binary into a container" model.

`mssql-pg-migrate` is a single static Rust binary that you point at two databases and run. That's the entire user experience. Every other design decision flows from preserving that.

## Core values (in priority order)

When two values conflict, the higher-listed one wins. These are not aspirational — they have shaped real decisions in the codebase and should be cited in PR reviews.

1. **Correctness over speed.** A migration that loses or corrupts a single row is worse than a migration that takes twice as long. We've explicitly slowed down code paths to fix decoding bugs (see PR #94 — nullable numeric handling). We don't ship fast-and-wrong.

2. **Speed over convenience.** Once correctness is settled, we are aggressive about performance. We use a forked tiberius for 32KB packets (42% faster). We use PostgreSQL's binary COPY protocol instead of `INSERT`. We auto-tune workers based on RAM and CPU. We compress text columns in transit when it pays. The user benefits are measured in hours saved on multi-TB migrations.

3. **Convenience over flexibility.** When a sensible default exists, it should *be* the default — not a config knob. Auto-tuning is the canonical example: chunk size, parallel readers, and parallel writers are computed from system resources unless the user explicitly overrides them. Most users never touch these.

4. **Observable failure over silent retry.** Every error returns a specific exit code (`crates/mssql-pg-migrate/src/error.rs`) so Airflow / Kubernetes / shell scripts can react appropriately. Errors are categorized by recoverability (`is_recoverable()`) so retry policies can be sane. We do not paper over failures with silent retry loops.

5. **Idempotent by default.** `run` automatically resumes from database-stored state if a previous run was interrupted. Watermarked upserts mean re-running a migration that already ran is cheap and safe. The user should be able to invoke the binary in a cron job or DAG without thinking about "did this already run?"

## What this tool is NOT

These are deliberate non-goals. Requests to add them should be declined or pushed to a separate project unless the rationale below has changed.

- **Not a CDC (Change Data Capture) tool.** We do not tail transaction logs, subscribe to logical replication slots, or guarantee sub-second freshness. If you need true streaming replication, use Debezium or pglogical. Our incremental sync is *date-watermark batched* — minutes-to-hours of latency, not seconds.

- **Not a schema migration / DDL versioning tool.** We do not track schema changes, generate migration scripts, or roll back DDL. Use Flyway, Liquibase, sqlx-migrate, or Atlas for that. Our schema creation is destructive (`drop_recreate`) or skipped (`upsert` against existing tables).

- **Not a generic ETL framework.** No transformations, no UDFs, no pipelines, no DAG. Source columns map 1:1 to target columns via type-preserving rules. If you need to compute, filter, or reshape rows, do it before or after — `mssql-pg-migrate` is the bulk-load step in *your* pipeline, not the pipeline itself.

- **Not a GUI or interactive product.** The TUI (`tui` feature) exists for one purpose: making the wizard / progress visible during ad-hoc local runs. It is not a replacement for the headless CLI, and the CLI must remain fully functional without it.

- **Not a multi-tenant service.** No daemon mode, no API server, no scheduler. The orchestration layer is `cron` / Airflow / Kubernetes Jobs / a shell script. We are the worker, not the foreman.

- **Not a hand-holding tool for novices.** We document well, but the target user is a data engineer or DBA who understands their schema, their network, and their database engines. We surface errors directly, we do not silently invent fallback behavior.

## Tradeoffs we accept

Every philosophy has uncomfortable tradeoffs. These are ours.

- **PostgreSQL-centric type model.** When a SQL Server type doesn't have a clean PostgreSQL or MySQL equivalent (`hierarchyid`, `geography`, `geometry`, MSSQL-specific spatial types), we map to a string or bytea representation rather than fail the migration. This is documented in the type-mapping table; users who need fidelity in those columns must transform separately.

- **Read-only on the source.** We never `UPDATE`, `DELETE`, `TRUNCATE`, or `ALTER` the source database. The cost is that we can't do clever things like marking rows as "synced" — but the benefit is that pointing this tool at a production source is unambiguously safe.

- **No deletes on the target in upsert mode.** Upsert mode does `INSERT ... ON CONFLICT DO UPDATE`. If a row was deleted from the source, it stays in the target. This is intentional: silent target deletes are dangerous, and users who need delete propagation should use `drop_recreate` or run a separate reconciliation step.

- **Eventual consistency, not transactional consistency.** We do not snapshot the source. A long-running migration of a busy source will end up with rows from different points in time in the target. For most analytics and replica use cases this is fine; for financial / regulatory consistency it is not, and you should freeze writes on the source for the duration of the migration.

- **Static binary > smaller binary.** We bundle TLS roots, link rustls statically, and accept ~30MB binaries because the operational simplicity of "it has zero runtime dependencies" is worth more than disk space.

- **One forked dependency (tiberius) is OK; two would not be.** We carry a single fork of tiberius for `packet_size` ([prisma/tiberius#400](https://github.com/prisma/tiberius/pull/400)) because the 42% throughput win is worth the maintenance burden. We would not accept a second forked dependency without revisiting the architecture — see [`mssql-client-spike.md`](mssql-client-spike.md) for the alternative we evaluated.

## How to use this document

When making a non-obvious design decision, ask:
1. Which values does this serve?
2. Which values does it work against?
3. Is the tradeoff explicit and documented?

If you can't articulate the answers, the decision probably needs more thought before becoming code. If a value here ever stops being true in practice, update this document — silent drift between stated values and actual behavior is the worst outcome.
