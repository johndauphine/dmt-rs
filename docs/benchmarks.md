# dmt-rs Benchmarks

Current throughput numbers, reproduction procedure, and findings for
dmt-rs end-to-end migrations. This is the single source of truth for
benchmark results — older experiment writeups live in
[`benchmarks-archive.md`](benchmarks-archive.md) and are superseded by
what's here.

**Dataset:** StackOverflow 2010 (Brent Ozar), 19 310 703 rows across 9
tables. **Current reference host:** M3 Max 36 GB (Mac15,10, 14 cores).
**Binary:** dmt-rs v1.46.0 with `--features mysql,tui,ai`.

---

## 1. Results matrix

End-to-end throughput (schema + transfer + PK + finalize),
`drop_recreate` mode, 4 workers, 50 K chunk size. Auto-tuned to 3
parallel readers / 3 write-ahead writers. Each target database is
dropped and recreated before its measurement; MSSQL-involved
directions use a warm-up discard + single measurement.

### 1.1 Current (v1.46.0, M3 Max 36 GB)

| # | Direction | Duration | **Throughput** | Measured |
|--:|---|---:|---:|---|
| 1 | mysql → pg | 20.09 s | **961 K rows/s** | 2026-04-19 (v1.45.0) |
| 2 | pg → pg | 24.73 s | 781 K rows/s | 2026-04-20 (v1.46.0) |
| 3 | mssql → pg | 32.40 s | 596 K rows/s | 2026-04-20 (v1.46.0) |
| 4 | mssql → mssql | 34.75 s | 556 K rows/s | 2026-04-20 (v1.46.0) |
| 5 | mysql → mysql (cross-container) | 42.76 s | 452 K rows/s | 2026-04-19 (v1.45.0) |
| 6 | mssql → mysql | 43.78 s | 441 K rows/s | 2026-04-19 (v1.45.0) |
| 7 | pg → mysql | 45.23 s | 427 K rows/s | 2026-04-19 (v1.45.0) |
| 8 | mysql → mssql | 52.32 s | 369 K rows/s | 2026-04-19 (v1.45.0) |
| 9 | pg → mssql | 60.07 s | 321 K rows/s | 2026-04-20 (v1.46.0) |

v1.46 vs v1.44 for the non-MySQL directions (2026-04-20 rerun): `pg →
pg` +3 %, `mssql → pg` **+32 %**, `mssql → mssql` +5 %, `pg → mssql`
+6 %. Only `mssql → pg` is a real improvement; the other three are
within single-run measurement noise.

**Why it's lopsided.** The only v1.45+ change touching these
directions is **inline PRIMARY KEY on PG targets** ([PR #122](https://github.com/johndauphine/dmt-rs/pull/122)).
Before: `CREATE TABLE` without PK, COPY rows, then
`ALTER TABLE ADD CONSTRAINT PRIMARY KEY` in a serial Phase 4. After:
PK declared inline, PG maintains the btree incrementally during
COPY, Phase 4 is a no-op. The PK-build cost **moves from Phase 4
into COPY** — net outcome depends on whether the writer has spare
CPU during COPY:

- **`mssql → pg`**: MSSQL source reads Posts at ~100 K rows/s via
  Rosetta 2. PG writer sits idle most of COPY waiting for data.
  Inline PK maintenance fits into those idle windows at near-zero
  marginal cost. Phase 4's ~8 s is effectively eliminated.
- **`pg → pg`**: PG source reads at ~2 M rows/s. Writer is CPU-bound
  during COPY with no idle windows. Inline PK extends COPY by almost
  as much as it saves on Phase 4. Net wash.
- **`mssql → mssql`, `pg → mssql`**: MSSQL target, **no inline-PK
  change exists for MSSQL** (only [#121](https://github.com/johndauphine/dmt-rs/pull/121)
  MySQL and [#122](https://github.com/johndauphine/dmt-rs/pull/122)
  PG). Both still pay full Phase 4 cost; the +5–6 % is noise.

Rule: **inline PK on PG helps proportionally to how slow the source
is** — the commit message on #122 called this out explicitly
(*"may increase COPY cost for large tables as btree is maintained
incrementally"*). Fast sources pay back what the commit gives.

### 1.2 Key takeaways

- **Fastest direction: `mysql → pg` at 961 K rows/s** — Postgres COPY
  BINARY absorbs the native-ARM MySQL source feed at near-native-PG
  speed.
- **Slowest direction: `pg → mssql` at 321 K rows/s** — MSSQL target
  via Rosetta 2 with LOB-heavy Posts table dominates.
- **MySQL-as-target range: 369 K – 452 K rows/s.** Substantially
  higher than older published figures (see §3.1).
- **MySQL-as-source range: 369 K – 961 K rows/s** — competitive with
  PG-as-source across all targets.

---

## 2. Performance model

The dominant factor is the **target database type**, mediated by source
feed rate. Neither CPU nor total RAM is the first-order bottleneck on
this workload.

### 2.1 Target write-path profile

| Target | Bottleneck | RAM effect | Storage effect |
|---|---|---|---|
| **PG** | Storage-bound — COPY BINARY flushes dirty pages direct to disk | Minimal (shared_buffers helps source reads, not target writes) | Dominant |
| **MSSQL** | Buffer-pool-bound — dirty pages absorbed in RAM, checkpoint amortizes | Dominant above threshold (`max server memory`) | Secondary |
| **MySQL** | INSERT protocol overhead + InnoDB checkpointer | Moderate (2 GB `innodb_buffer_pool_size` is the sweet spot; larger regresses) | Secondary |

### 2.2 Source read-path profile

| Source | Protocol | Native ARM on Apple Silicon? |
|---|---|---|
| PG (`postgres:16-alpine`) | Binary COPY or streaming | Yes — no emulation penalty |
| MySQL (`mysql:8.0`) | Binary protocol (mysql_async) | Yes — no emulation penalty |
| MSSQL (`mcr.microsoft.com/mssql/server`) | TDS (tiberius, 32 KB packets) | **No — Rosetta 2**, ~2-5× emulation penalty |

MSSQL-as-source is the one direction that meaningfully benefits from
larger buffer pool (`max server memory`), because the Rosetta-emulated
LOB read path is slow enough that keeping the working set in memory
matters.

### 2.3 Common failure modes

- **Under-provisioned container memory**: MSSQL at < 4 GiB
  `max server memory` caps `mssql → *` throughput at M5 Pro-era 120 K
  rows/s regardless of host. Playbook value for M3 Max 36 GB is
  **10 240 MB** on a **12 GB** container.
- **Over-provisioned MySQL buffer pool**: > 2 GB regresses ~5 %
  (3 GB) or OOMs (4 GB) on the standard 6 GB container cap. This is
  an InnoDB checkpointer-vs-cache-hit tradeoff, not a host issue.
- **pg-target cgroup < 2 GiB**: COPY drops on LOB-heavy tables with
  `COPY finish: connection closed`. Minimum 2 GiB verified; 3 GiB is
  comfortable.

---

## 3. Findings

### 3.1 MySQL target protocol ceiling is ~450 K rows/s, not 120 K

Older docs cited MySQL target throughput caps of **120 K** (M5 Pro) or
**165 K** (M3 Max) rows/s, attributed to "MySQL has no binary bulk
protocol like PG COPY or MSSQL BCP." Measured on v1.45.0 across three
MySQL target directions:

| Direction | Throughput |
|---|---:|
| mysql → mysql cross-container | 452 K rows/s |
| mssql → mysql | 441 K rows/s |
| pg → mysql | 427 K rows/s |

Every direction lands **2.2 – 2.7×** the published M3 Max number. The
earlier figures were measured against v1.44 (post-load PK rebuild
dominated finalization) and often against Rosetta-emulated MSSQL
sources (feed rate bound). Neither is a MySQL-target-protocol
limitation.

**Inline PK in `CREATE TABLE`** (v1.45 commits `7647ec4` MySQL,
`3e4eef6` Postgres) removed the biggest source of dirty-page pressure
during finalization and is the dominant cause of the uplift.

### 3.2 MSSQL `max server memory` bump: +5–8 % on v1.45

Playbook recommends bumping MSSQL from 4 096 MB (M5 Pro-era) to
10 240 MB (M3 Max). Measured deltas on v1.45:

| Direction | 4 096 MB | 10 240 MB | Δ |
|---|---:|---:|---:|
| mssql → mysql | 421 K rows/s | 441 K rows/s | +4.7 % |
| mysql → mssql | 342 K rows/s | 369 K rows/s | +7.9 % |

Smaller than the +36–41 % measured on v1.44 (archived experiment). Do
the bump anyway — it's free on a 36 GB host — but don't expect the
earlier magnitude.

### 3.3 LOAD DATA LOCAL INFILE is slower than batched INSERT

Both on stock `mysql:8.0` and on the tuned container (2 GB buffer
pool), **`mysql_load_data: always` runs ~12 % slower than batched
INSERT** at 4 workers. Client-side TSV escape CPU (every `\t`, `\n`,
`\\`, `\0`, NULL sentinel per value per row) dominates the
server-side bulk-path win.

Default is `mysql_load_data: never`. The feature is retained for
single-worker configs where TSV CPU isn't contested.

### 3.4 Text-heavy tables dominate wall time

In every direction, the `Posts` table (3.7 M rows with `nvarchar(max)
Body`) accounts for the majority of wall-clock. Stripping Posts
leaves the remaining 8 tables transferring in 3-18 seconds regardless
of direction.

Plain-text INSERT benchmarks on stock MySQL measured a **5.7× slowdown**
from LOB columns (194 K → 34 K rows/s), which is a stricter per-table
view than the end-to-end numbers above.

---

## 4. Environment

### 4.1 Host

- **Mac:** M3 Max 36 GB (Mac15,10), 14 cores
- **OS:** macOS Darwin 25.4.0
- **Docker Desktop VM:** 22-24 GB (Settings → Resources → Memory)

### 4.2 Containers

Rule: **only 2 containers running at a time.** Start the two you need
for a direction; stop the rest with `docker stop`.

| Container | Image | Port | `--memory` | Engine tuning |
|---|---|---:|---:|---|
| `pg-source` | `postgres:16-alpine` (arm64) | 5432 | 3 g | defaults |
| `pg-target` † | `postgres:16-alpine` (arm64) | 5434 | 3 g | defaults |
| `mssql-bench` | `mssql/server:2022-latest` (Rosetta) | 1433 | **12 g** | `max server memory = 10240 MB`; `network packet size = 32767` |
| `mssql-target` † | `mssql/server:2022-latest` (Rosetta) | 1434 | **12 g** | same |
| `mysql-source` | `mysql:8.0` (arm64) | 3306 | 6 g | tuned `my.cnf` (2 GB buffer pool, 512 MB redo, doublewrite off) |
| `mysql-target` | `mysql:8.0` (arm64) | 3307 | 6 g | same tuned `my.cnf` |

† Second container of the same engine, needed only for `X → X`
directions where source and target must be separate (`pg → pg`,
`mssql → mssql`). Not needed for `mysql → mysql` — both mysql
containers above serve that.

MySQL config file: `docker/mysql-target/my.cnf` (mounted read-only
into both mysql containers).

### 4.3 Why these caps

- **MSSQL 10 240 MB / 12 GB cgroup:** host-RAM-dependent. On M5 Pro
  24 GB, use 4 096 MB / 5 GB instead. This is the biggest lever for
  `mssql → *` directions; under-provisioning caps throughput at the
  M5 Pro-era 120 K rows/s ceiling.
- **MySQL 2 GB `innodb_buffer_pool_size` / 6 GB cgroup:**
  host-RAM-independent. Empirically 2 GB is the sweet spot; larger
  pools regress throughput on append-heavy bulk load.
- **PG 3 GB cgroup:** below 2 GB causes COPY drops on LOB tables;
  above 3 GB wastes host VM budget.

---

## 5. Reproduction

### 5.1 Start containers

Per direction, start only source + target. Example for `mysql → pg`:

```bash
docker start pg-source mysql-source
# ... run benchmark ...
docker stop pg-source mysql-source
```

First-time container creation:

```bash
# pg-source
docker run -d --name pg-source -p 5432:5432 \
  --memory=3g --memory-swap=3g \
  -e POSTGRES_PASSWORD=TestPass2024 \
  postgres:16-alpine

# mssql-bench (10 240 MB max server memory applied after first run)
docker run -d --name mssql-bench -p 1433:1433 \
  --memory=12g --memory-swap=12g \
  -e ACCEPT_EULA=Y -e MSSQL_SA_PASSWORD=TestPass2024 \
  mcr.microsoft.com/mssql/server:2022-latest
docker exec mssql-bench /opt/mssql-tools18/bin/sqlcmd \
  -S localhost -U sa -P TestPass2024 -C -Q "
  EXEC sp_configure 'show advanced options', 1; RECONFIGURE;
  EXEC sp_configure 'max server memory (MB)', 10240; RECONFIGURE WITH OVERRIDE;
  EXEC sp_configure 'network packet size (B)', 32767; RECONFIGURE WITH OVERRIDE;"

# mysql-source (3306) and mysql-target (3307), identical tuned my.cnf
for NAME_PORT in mysql-source:3306 mysql-target:3307; do
  NAME="${NAME_PORT%:*}"; PORT="${NAME_PORT#*:}"
  docker run -d --name "$NAME" -p "$PORT:3306" \
    --memory=6g --memory-swap=6g \
    -e MYSQL_ROOT_PASSWORD=TestPass2024 \
    -v "$PWD/docker/mysql-target/my.cnf:/etc/mysql/conf.d/tuned.cnf:ro" \
    mysql:8.0
done
```

### 5.2 Load source data

StackOverflow 2010 MDF files on the host go into `mssql-bench` via
volume mount + `CREATE DATABASE ... FOR ATTACH` (SQL Server 2022 will
run an automatic version upgrade from Azure SQL Edge's 921 schema —
takes ~5 seconds, non-destructive). See
[`benchmarks-archive.md`](benchmarks-archive.md) §"Pre-existing
volumes" for the safe-attach procedure.

Populate `pg-source` and `mysql-source` by running one-time
migrations from `mssql-bench`:

```bash
./target/release/dmt-rs -c .bench-load-pg-source.yaml run
./target/release/dmt-rs -c .bench-load-mysql-source.yaml run
```

### 5.3 Run a benchmark

All 9 permutation configs live as `.bench-<src>-to-<tgt>.yaml` at the
repo root (untracked). Template:

```yaml
source: { type: <src>, host: localhost, port: <port>, database: <db>, ... }
target: { type: <tgt>, host: localhost, port: <port>, database: <db>, ... }
migration:
  target_mode: drop_recreate
  workers: 4
  chunk_size: 50000
```

Reset the target, then time:

```bash
# Drop + recreate target DB (engine-specific — see configs)
START=$(python3 -c 'import time; print(time.time())')
./target/release/dmt-rs -c .bench-mysql-to-pg.yaml run
END=$(python3 -c 'import time; print(time.time())')
awk -v s="$START" -v e="$END" 'BEGIN { printf "wall=%.2fs\n", e - s }'
```

For MSSQL-involved directions, run once as warm-up (discard), reset
target, then measure.

### 5.4 Validate row counts

```sql
-- On the target
SELECT table_name, rows_total, rows_transferred, table_status
FROM _dmt_rs.table_state
WHERE run_id = (SELECT run_id FROM _dmt_rs.table_state
                ORDER BY run_completed_at DESC LIMIT 1)
ORDER BY rows_total DESC;
```

All 9 tables should report `rows_transferred = rows_total` and
`table_status = 'completed'`.

---

## 6. Gotchas

### 6.1 MSSQL memory resize order

If **lowering** MSSQL memory, lower `max server memory (MB)` first
(wait ~5 s for buffer pool to release), *then* `docker update
--memory`. Other order OOM-kills the container because the cgroup cap
drops below the buffer pool's current working set.

### 6.2 MySQL buffer pool ceiling is InnoDB-internal

The 2 GB cap on `innodb_buffer_pool_size` has nothing to do with host
RAM — it's a checkpointer-vs-cache-hit tradeoff on append-heavy bulk
load. 3 GB regresses ~5 %, 4 GB OOMs. Don't raise it on a bigger host.

### 6.3 `LOAD DATA` is slower, not faster

Counterintuitive but measured repeatedly: `mysql_load_data: always`
runs ~12 % slower than batched INSERT at ≥ 2 workers. Default
`never`. See §3.3.

### 6.4 Same-container source+target inflates throughput

Running mysql→mysql with both source and target in the *same*
container measures 490 K rows/s; cross-container measures 452 K. The
single-container number is reading from a warm shared buffer pool and
not honest. Always use two containers for `X → X` measurements.

### 6.5 PG database name case sensitivity

PG lowercases unquoted identifiers. A pre-existing `stackoverflow2010`
database (all lowercase) works with configs referencing
`stackoverflow2010`, not `SO2010` or `StackOverflow2010`. Bench
configs match what's actually in the volume.

### 6.6 pg-target OOM deadlocks dmt-rs (known bug)

Symptom: `Writer 0 failed: Transfer failed for table "public"."Posts":
COPY finish: connection closed`, followed by dmt-rs hanging at 0 % CPU
with all PG connections idle in `ClientRead`. Root cause is PG being
SIGKILL'd by the cgroup OOM-killer mid-COPY (confirm with
`docker logs pg-target | grep "signal 9"`). PG crash-recovers in ~30 s
but dmt-rs doesn't recover cleanly from the dropped connection — the
writer pool error path deadlocks instead of returning
`EXIT_TRANSFER_ERROR`.

Workaround: size `pg-target --memory=3g` for SO2010, `≥ 4g` for
SO2013+. This is a real dmt-rs bug, tracked upstream. Until fixed,
the defense is not OOM'ing PG in the first place.

### 6.7 tiberius ENV_CHANGE log labels are reversed

Forked tiberius prints `Packet size change from '16384' to '4096'`
with `from` and `to` swapped. The wire-level value is correctly
applied — the log is purely cosmetic. When reading tiberius logs,
the "from" value is the *new* value, the "to" is the *old*.

### 6.8 NVMe bandwidth varies between Macs

M5 Pro NVMe measured ~2× faster than M3 Max on this workload. For
storage-bound directions (`* → pg`, `pg → pg`), chip and RAM matter
less than drive bandwidth. Don't assume cross-Mac storage parity.

---

## 7. Go vs dmt-rs

Head-to-head comparison against `dmt` (Go) lives in
[`benchmarks-archive.md`](benchmarks-archive.md) §Go-vs-dmt-rs. Last
measured 2026-04-14 on M5 Pro 24 GB; not yet re-run on M3 Max or
against v1.45. Summary of that result:

| | Rust | Go |
|---|---|---|
| `drop_recreate` directions | Roughly comparable | Roughly comparable |
| `mssql → pg upsert` | 388 K rows/s | 872 K rows/s |
| `pg → pg drop_recreate` | **Faster** | Slower |
| `mssql → mssql upsert` | ~tie | ~tie |

dmt-rs trails Go significantly on `mssql → pg upsert`; ~ties on
`drop_recreate`. A re-run on v1.45 + M3 Max is outstanding work.

---

## 8. Open questions

- **n=3 medians.** Current numbers are single-run measurements. The
  archived experiments used n=3; for statistical comparability a
  follow-up should match.
- **Where does MySQL target actually cap?** 452 K rows/s is a floor,
  not a ceiling. A faster source or reduced LOB width would find the
  real limit.
- **Cross-host.** All numbers are same-VM cross-container. Real
  network-bound migrations are an unmeasured regime.
