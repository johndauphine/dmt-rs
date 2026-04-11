# Benchmark Playbook

A reproducible, self-contained procedure for running the dmt-rs end-to-end
benchmark across all four `{mssql,pg} × {mssql,pg}` migration directions,
plus the infrastructure gotchas and cross-hardware prediction table.

Authoring context: this doc was first written at the end of a session on
an **M5 Pro 24 GB** (MacBook) running Docker Desktop with x86_64 emulation
for MSSQL, then revised after a follow-up run on **M3 Max 36 GB** refuted
the original §2 hypothesis. See [`benchmark-results-m3-max.md`](benchmark-results-m3-max.md)
for the actual cross-hardware results.

> **Note on scope.** This playbook documents a benchmark harness against
> real databases, not the unit test suite. `cargo test --all-features`
> needs nothing beyond a working Rust toolchain. The procedures below
> are for measuring end-to-end migration throughput on real data.

> **Most important finding from cross-hardware validation**: the dominant
> factor is **the target database type, not the host chip**. PG targets
> are storage-bound (COPY flushes dirty pages direct to disk, no meaningful
> buffer pool effect); MSSQL targets are buffer-pool-bound (dirty pages
> absorb in RAM, checkpoint amortizes over slow storage). NVMe bandwidth
> matters roughly as much as RAM size — **not negligible** as this doc
> originally claimed. See §3 below for the corrected model.

---

## 1. Baseline results (M5 Pro 24 GB, 2026-04-11)

All numbers are against **Stack Overflow 2010** (19,310,703 rows across 9
tables). Docker Desktop configured to 7.75 GiB. Run-to-run variance on
x86-emulated Apple Silicon is ~5-10%, so small differences are noise.

### End-to-end (including finalization: PK creation, indexes off)

| Direction | End-to-end | Transfer-only | Throughput (e2e) | Posts duration |
|---|---:|---:|---:|---:|
| **pg → pg** | **16.5s** | **12.7s** | **1,168K rows/sec** | 12.71s |
| mssql → pg | 41.3s / 43.4s | 36.1s | 468K / 445K rows/sec | 27.37s / 26.67s |
| pg → mssql | 104.8s | ~104s | 184K rows/sec | 104.54s |
| **pg → mssql (tuned, presized + SIMPLE recovery)** | **141.8s** | **~141s** | **136K rows/sec** | 141.5s |
| mssql → mssql (untuned) | ❌ killed after 12+ min | — | ~10K rows/sec extrapolated | never finished |
| **mssql → mssql (tuned)** | **98.6s** | **~97s** | **196K rows/sec** | 82.08s |

The two `pg → mssql` rows show a counterintuitive regression after tuning.
Digging into it: the original 104s was partially luck (cold buffer pool
state), and reverting each tuning knob individually (memory cap, CPU cap,
MAXDOP) had no effect. The real win from tuning was on `mssql → mssql`
(12+ min failing → 98.6s complete); the `pg → mssql` tradeoff is acceptable.

### Posts is always the choke point

In every direction, the `Posts` table dominates wall-clock because of the
`nvarchar(max)` `Body` column. Strip Posts out and the remaining 8 tables
transfer in 3–18 seconds regardless of direction.

### Tuning journey — what actually helped and what didn't

| Change | Effect | Notes |
|---|---|---|
| Memory caps on both MSSQL containers (3-3.5 GiB Docker, 2-3 GiB `max server memory`) | **Huge win for mssql→mssql** (12 min fail → 99s complete) | Uncapped containers fought for the 8 GB VM and starved bb8 connection pools |
| MAXDOP = 4 | Neutral | Doesn't hurt, doesn't help; matches CPU cap |
| `--cpus=4` Docker cap | Neutral for our workloads | MSSQL internal parallelism doesn't exercise more cores than this under our load |
| `network packet size (B)` = 32767 | **Already working** (display bug hid this) | See gotchas below |
| Presized data file + SIMPLE recovery on pg→mssql target | +20s improvement on a 165s run | Auto-growth of the log file was pure overhead |
| CPU cap removal (15 → 4 → 15) | Neutral | Re-confirmed CPU isn't the bottleneck |

---

## 2. Hardware comparison

MSSQL has no ARM Linux build, so it's always running under Rosetta 2
regardless of chip — neither M5 Pro nor M3 Max can avoid the 2-5×
emulation penalty. Postgres containers run native ARM64
(`postgres:16-alpine` has an arm64 manifest), so they're not affected
by chip choice itself.

| Factor | M5 Pro 24 GB | M3 Max 36 GB | Impact on this workload |
|---|---|---|---|
| Per-core perf | Higher (newer P-cores) | Lower (~5-15%) | Negligible — workload isn't CPU-bound |
| Core count | 10-12 | 14-16 | Negligible — we `--cpus=4` cap containers |
| **Available RAM for Docker VM** | **~7.75 GiB** (24 - macOS - apps) | **~20-24 GiB** | **Helps MSSQL-target directions (big buffer pool). Minimal impact on PG-target directions.** |
| Rosetta 2 generation | Latest | 2 generations older | Marginal (~5-10% slower emulation) |
| **NVMe bandwidth** | **Faster** | **~50% slower** (!) | **Dominates PG-target directions and PG→PG.** This was the surprise from the M3 Max run. |

### The corrected workload model

> **The dominant factor is the target database type, not the host chip:**
>
> - **PG targets** are storage-bound. `COPY` flushes dirty pages direct to
>   disk; `shared_buffers` helps source reads a little but doesn't mask
>   target-side writes. Slower NVMe → slower migration, end of story.
> - **MSSQL targets** are buffer-pool-bound. Dirty pages absorb in RAM and
>   the dirty page flusher amortizes against slow storage. More RAM → big
>   win even on slower NVMe.
> - **Source reads on MSSQL** benefit from the buffer pool once the data
>   file is warm (the SO2010 `.mdf` is ~9 GB; on a 6 GB `max server memory`
>   cap, most of Posts' LOB pages fit after warm-up).
> - **Source reads on PG** pay storage cost on every chunk because PG's
>   COPY path doesn't warm `shared_buffers` aggressively for sequential
>   scans.

This model predicts every row of the M3 Max results correctly (see
[`benchmark-results-m3-max.md`](benchmark-results-m3-max.md) §3). The
*original* §2 of this playbook said "NVMe bandwidth: negligible once
working set fits in RAM" — that was wrong for PG-target directions
specifically.

---

## 3. Prerequisites

- **Docker Desktop** with the VM configured for at least 20 GiB RAM on a 36
  GB host. Check: `docker info 2>&1 | grep -E '^ Total Memory|^ CPUs'`.
- **Rust 1.75+** (workspace MSRV). Any stable toolchain works.
- **Stack Overflow 2010 data files** (`.mdf` + `.ldf`) for MSSQL, available
  from Brent Ozar's [downloads page](https://www.brentozar.com/archive/2015/10/how-to-download-the-stack-overflow-database-via-bittorrent/).
  The existing session stored them at `~/docker-data/mssql-bench/data/`
  (host mount path); adjust for your machine.
- **A PostgreSQL clone of the same data** for pg→* directions. Easiest path:
  first run `mssql → pg drop_recreate` after MSSQL is up, then use the
  resulting pg database as the source for pg→* tests.
- **Repository checked out at current main** (post PR #95 — so the binary
  builds as `target/release/dmt-rs`, not `mssql-pg-migrate`).

Build the binary:

```bash
cargo build --release --all-features
./target/release/dmt-rs --version   # should report: dmt-rs 1.42.2 (or later)
```

---

## 4. Container setup

### 4.1 Launch MSSQL source container with preloaded SO2010

Assumes `.mdf`/`.ldf` for StackOverflow2010 are already on the host. Point
the volume mount at whatever directory holds them. The directory is mounted
as `/var/opt/mssql` inside the container, which is where SQL Server on
Linux looks for its data files.

```bash
docker run -d --name mssql-source \
  -p 1433:1433 \
  -e ACCEPT_EULA=Y \
  -e MSSQL_SA_PASSWORD='YourStrong@Passw0rd' \
  --memory=8g --memory-swap=8g \
  --platform linux/amd64 \
  -v /path/to/mssql-bench:/var/opt/mssql \
  mcr.microsoft.com/mssql/server:2022-latest
```

Wait for readiness:

```bash
until docker exec mssql-source /opt/mssql-tools18/bin/sqlcmd \
  -S localhost -U sa -P 'YourStrong@Passw0rd' -C \
  -Q "SELECT name FROM sys.databases WHERE name='StackOverflow2010'" \
  -h -1 2>&1 | grep -q 'StackOverflow2010'; do
  echo "waiting for mssql-source..."
  sleep 3
done
echo "mssql-source ready"
```

> **If login fails** because the preloaded data has a different SA password
> baked into its `master.mdf`, reset it non-destructively via
> `mssql-conf set-sa-password`. This is required because `MSSQL_SA_PASSWORD`
> env var is only read on first-init — it does nothing on a container that's
> reusing an existing volume:
>
> ```bash
> docker stop mssql-source
> docker run --rm -i -e ACCEPT_EULA=Y \
>   -v /path/to/mssql-bench:/var/opt/mssql \
>   --platform linux/amd64 \
>   --entrypoint /opt/mssql/bin/mssql-conf \
>   mcr.microsoft.com/mssql/server:2022-latest \
>   set-sa-password <<< 'YourStrong@Passw0rd
> YourStrong@Passw0rd'
> docker start mssql-source
> ```
>
> This only modifies the SA login row inside `master.mdf`; the
> StackOverflow2010 data is untouched.

### 4.2 Launch MSSQL target container (fresh, no preloaded data)

Needed only for `mssql → mssql` direction. Separate container, separate
port, no volume mount so it inits cleanly with `MSSQL_SA_PASSWORD`:

```bash
docker run -d --name mssql-target \
  -p 1434:1433 \
  -e ACCEPT_EULA=Y \
  -e MSSQL_SA_PASSWORD='YourStrong@Passw0rd' \
  --memory=8g --memory-swap=8g \
  --platform linux/amd64 \
  mcr.microsoft.com/mssql/server:2022-latest

until docker exec mssql-target /opt/mssql-tools18/bin/sqlcmd \
  -S localhost -U sa -P 'YourStrong@Passw0rd' -C \
  -Q "SELECT @@VERSION" -h -1 2>&1 | grep -q "Microsoft SQL Server"; do
  echo "waiting for mssql-target..."
  sleep 3
done
echo "mssql-target ready"
```

### 4.3 Launch Postgres source container

```bash
docker run -d --name pg-source \
  -p 5432:5432 \
  -e POSTGRES_PASSWORD=TestPass2024 \
  --memory=3g --memory-swap=3g \
  postgres:16-alpine
```

For pg→* directions, this container needs to hold SO2010 data. Easiest
population path: run `dmt-rs` with MSSQL as source and pg-source as target
once before running pg-source as a source in subsequent tests.

### 4.4 Launch Postgres target container

```bash
docker run -d --name pg-target \
  -p 5433:5432 \
  -e POSTGRES_PASSWORD=TestPass2024 \
  --memory=3g --memory-swap=3g \
  postgres:16-alpine
```

> **Do not cap pg-target below 2 GiB.** The `pg-bench-target` container at
> 1.5 GiB dropped the COPY connection with `COPY finish: connection closed`
> on the Posts table because the `nvarchar(max)` Body data exceeded the
> available buffer budget. 3 GiB is comfortable; 2 GiB is the minimum we
> verified works.

### 4.5 Apply MSSQL server configuration

Both MSSQL containers should have the same server-level tuning applied.
These are dynamic — no restart needed for `max server memory`, but
`network packet size` takes effect on new connections only (restart is
simplest):

```bash
for container in mssql-source mssql-target; do
  docker exec $container /opt/mssql-tools18/bin/sqlcmd \
    -S localhost -U sa -P 'YourStrong@Passw0rd' -C -Q "
    EXEC sp_configure 'show advanced options', 1; RECONFIGURE;
    EXEC sp_configure 'max server memory (MB)', 6144; RECONFIGURE WITH OVERRIDE;
    EXEC sp_configure 'network packet size (B)', 32767; RECONFIGURE WITH OVERRIDE;
  "
done

docker restart mssql-source mssql-target
```

Wait for both to come back up using the readiness checks above.

### 4.6 Memory budget for 36 GB host

Target Docker Desktop VM: 20-24 GiB. Distribution:

| Container | Docker `--memory` cap | SQL Server `max server memory` | Purpose |
|---|---:|---:|---|
| mssql-source | 8 GiB | 6144 MiB | Must hold SO2010 data file in buffer pool |
| mssql-target | 8 GiB | 6144 MiB | Same, plus room for target writes |
| pg-source | 3 GiB | n/a (auto) | Holds SO2010 PG copy for pg→* tests |
| pg-target | 3 GiB | n/a (auto) | Write-side buffers for mssql→pg, pg→pg |
| **Total allocated** | **22 GiB** | | |
| VM headroom | ~2 GiB free | | Kernel, Docker daemon, slack |

On the 24 GB M5 Pro we couldn't fit this budget — the VM itself was only
7.75 GiB. That's why tuning collapsed into shrinking containers to 2.5
GiB each, which is what caused the bb8 pool contention in `mssql →
mssql`. The 36 GB machine should not need any of that scaffolding.

---

## 5. Test configurations

Write these four configs to `/tmp/` (or equivalent). They assume the
container ports from section 4.

### 5.1 `/tmp/dmt-rs-pg2pg.yaml`

```yaml
source:
  type: postgres
  host: localhost
  port: 5432
  database: so2010_bench
  schema: public
  user: postgres
  password: TestPass2024
  ssl_mode: disable

target:
  type: postgres
  host: localhost
  port: 5433
  database: dmt_test_target
  schema: public
  user: postgres
  password: TestPass2024
  ssl_mode: disable

migration:
  target_mode: drop_recreate
  create_indexes: false
  create_foreign_keys: false
  create_check_constraints: false
```

### 5.2 `/tmp/dmt-rs-mssql2pg.yaml`

```yaml
source:
  type: mssql
  host: localhost
  port: 1433
  database: StackOverflow2010
  schema: dbo
  user: sa
  password: "YourStrong@Passw0rd"
  encrypt: false
  trust_server_cert: true

target:
  type: postgres
  host: localhost
  port: 5433
  database: dmt_test_target
  schema: public
  user: postgres
  password: TestPass2024
  ssl_mode: disable

migration:
  target_mode: drop_recreate
  create_indexes: false
  create_foreign_keys: false
  create_check_constraints: false
```

### 5.3 `/tmp/dmt-rs-pg2mssql.yaml`

```yaml
source:
  type: postgres
  host: localhost
  port: 5432
  database: so2010_bench
  schema: public
  user: postgres
  password: TestPass2024
  ssl_mode: disable

target:
  type: mssql
  host: localhost
  port: 1434
  database: dmt_test_target
  schema: dbo
  user: sa
  password: "YourStrong@Passw0rd"
  encrypt: false
  trust_server_cert: true

migration:
  target_mode: drop_recreate
  create_indexes: false
  create_foreign_keys: false
  create_check_constraints: false
```

### 5.4 `/tmp/dmt-rs-mssql2mssql.yaml`

```yaml
source:
  type: mssql
  host: localhost
  port: 1433
  database: StackOverflow2010
  schema: dbo
  user: sa
  password: "YourStrong@Passw0rd"
  encrypt: false
  trust_server_cert: true

target:
  type: mssql
  host: localhost
  port: 1434
  database: dmt_test_target
  schema: dbo
  user: sa
  password: "YourStrong@Passw0rd"
  encrypt: false
  trust_server_cert: true

migration:
  target_mode: drop_recreate
  create_indexes: false
  create_foreign_keys: false
  create_check_constraints: false
```

---

## 6. Test procedure

### 6.1 Populate pg-source (one-time setup for pg→* directions)

```bash
docker exec pg-source psql -U postgres -c "CREATE DATABASE so2010_bench;"

# Run mssql → pg into pg-source to populate it
cat > /tmp/dmt-rs-populate-pg-source.yaml <<'EOF'
source:
  type: mssql
  host: localhost
  port: 1433
  database: StackOverflow2010
  schema: dbo
  user: sa
  password: "YourStrong@Passw0rd"
  encrypt: false
  trust_server_cert: true
target:
  type: postgres
  host: localhost
  port: 5432
  database: so2010_bench
  schema: public
  user: postgres
  password: TestPass2024
  ssl_mode: disable
migration:
  target_mode: drop_recreate
  create_indexes: false
  create_foreign_keys: false
  create_check_constraints: false
EOF

./target/release/dmt-rs -c /tmp/dmt-rs-populate-pg-source.yaml run
```

### 6.2 Run each direction

For each of the four config files, follow this pattern:

```bash
# 1. Drop and recreate the target DB for a clean slate
docker exec pg-target psql -U postgres \
  -c "DROP DATABASE IF EXISTS dmt_test_target;" \
  -c "CREATE DATABASE dmt_test_target;"
# or for MSSQL target:
docker exec mssql-target /opt/mssql-tools18/bin/sqlcmd \
  -S localhost -U sa -P 'YourStrong@Passw0rd' -C \
  -Q "IF DB_ID('dmt_test_target') IS NOT NULL DROP DATABASE dmt_test_target; CREATE DATABASE dmt_test_target;"

# 2. Health check
./target/release/dmt-rs -c /tmp/dmt-rs-pg2pg.yaml health-check

# 3. Run, capturing full output to a log file
./target/release/dmt-rs -c /tmp/dmt-rs-pg2pg.yaml run > /tmp/dmt-rs-pg2pg.log 2>&1
echo "exit=$?"
tail -10 /tmp/dmt-rs-pg2pg.log

# 4. Extract per-table timings
grep -a -E "transferred [0-9]+ rows in|partitioning into|Phase 4|Migration completed" /tmp/dmt-rs-pg2pg.log
```

Repeat the same sequence for `mssql2pg`, `pg2mssql`, `mssql2mssql`. For the
`mssql2mssql` case, drop-and-recreate the target on the `mssql-target`
container (port 1434), not `mssql-source`.

---

## 7. Validation

Every successful run writes state to the `_dmt_rs` schema in the target
database. The schema has **one denormalized table** (`_dmt_rs.table_state`)
with all run-level fields (`run_id`, `run_started_at`, `run_completed_at`,
`run_status`, `config_hash`) stored alongside each per-table row — there is
no separate `migration_runs` table, despite what an earlier version of
this doc and `docs/tech-specs.md` originally claimed.

Verify row integrity after a run:

```bash
# For PostgreSQL targets
docker exec pg-target psql -U postgres -d dmt_test_target -c "
SELECT table_name, rows_total, rows_transferred, table_status,
       EXTRACT(EPOCH FROM (table_completed_at - run_started_at))::numeric(10,2) AS t_plus_sec
FROM _dmt_rs.table_state
WHERE run_id = (SELECT run_id FROM _dmt_rs.table_state
                WHERE run_status = 'completed'
                ORDER BY run_started_at DESC LIMIT 1)
ORDER BY table_completed_at;
"

# For MSSQL targets
docker exec mssql-target /opt/mssql-tools18/bin/sqlcmd \
  -S localhost -U sa -P 'YourStrong@Passw0rd' -C -d dmt_test_target -Q "
SELECT table_name, rows_total, rows_transferred, table_status
FROM _dmt_rs.table_state
WHERE run_id = (SELECT TOP 1 run_id FROM _dmt_rs.table_state
                WHERE run_status = 'completed' ORDER BY run_started_at DESC)
ORDER BY table_completed_at;
"
```

Expected: 9 rows, all `table_status = completed`, `rows_transferred`
matches `rows_total` for every table. The canonical row counts are:

| Table | Rows |
|---|---:|
| Posts | 3,729,195 |
| Comments | 3,875,183 |
| Votes | 10,143,364 |
| Badges | 1,102,019 |
| Users | 299,398 |
| PostLinks | 161,519 |
| VoteTypes | 15 |
| PostTypes | 8 |
| LinkTypes | 2 |
| **Total** | **19,310,703** |

---

## 8. Gotchas (things that bit us on the M5 Pro run)

### 8.1 Tiberius ENV_CHANGE display bug: log labels are reversed

Log lines like:

```
Packet size change from '16384' to '4096'
Database change from 'StackOverflow2010' to 'master'
```

have `from` and `to` **swapped in the print format**. The correct reading is:

- `Packet size change from '16384' to '4096'` → the new packet size is **16384**, the old was 4096. (Session negotiated *up* from the 4096 default.)
- `Database change from 'StackOverflow2010' to 'master'` → the new database is `StackOverflow2010`, the old was `master`. (Normal login flow — session enters master first, then USE-s the configured database.)

This is a purely cosmetic bug in the forked tiberius (`src/tds/codec/token/token_env_change.rs`, Display impl destructures the tuple backwards). The wire-level value is correctly applied via `set_packet_size(new_size)` in `src/tds/stream/token.rs`. The packet_size optimization has always been working — we just couldn't see it clearly.

Full details in memory: `tiberius_envchange_display_bug.md`. Not filed
upstream because it's purely cosmetic and would muddy PR #400.

### 8.2 Do not shrink a running container's memory cap below its current working set

We hit exit 137 (OOM-killed) on `mssql-source` when we tried to apply
`docker update --memory=2560m` while the container was already using 4 GiB.
The Linux OOM killer fires immediately when the new cap is below current
RSS.

**Correct order:** lower SQL Server's internal `max server memory (MB)`
*first* via `sp_configure` + `RECONFIGURE WITH OVERRIDE`, wait for the
buffer pool to release memory back, *then* apply the `docker update --memory`
cap.

### 8.3 PostgreSQL target with `<2 GiB` Docker memory cap will drop COPY connections on LOB tables

Symptom: `Writer 0 failed: Transfer failed for table "public"."Posts": COPY finish: connection closed`

Cause: the `nvarchar(max)` Body column generates large COPY buffers. When
the PG target container hits its memory cap mid-COPY, the kernel kills the
receiving process and the client sees a closed connection.

Fix: set `pg-target --memory=3g` (or higher). 1.5 GiB is too small; 2 GiB
is tight; 3 GiB is comfortable.

### 8.4 mssql → mssql requires tuned memory caps

An untuned `mssql → mssql` run with both containers uncapped in an 8 GB
Docker VM produced catastrophic results: `Votes:p3` partition failed with
bb8 connection pool timeout, Comments throughput collapsed from 180K
rows/sec (p1) to 2,400 rows/sec (p3), and the run had to be killed after
12+ minutes incomplete.

Root cause: two uncapped MSSQL servers each thinking they had 7.75 GiB
fought for RAM in an 8 GiB VM, starving the target's connection pool as
the Linux OOM pressure built.

On the 36 GB machine with proper per-container caps (6 GiB internal
`max server memory`, 8 GiB Docker cap, VM set to 20+ GiB) this shouldn't
reproduce — there's enough headroom for both servers to run at full
configured memory without fighting. **Validate this is actually the case
before trusting the direction works.**

### 8.5 First run after SQL Server restart is slower

Container restart (or SQL Server restart) clears the buffer pool, plan
cache, and some statistics. The first query on a fresh instance pays
cold-cache cost. Always run each direction **twice** and report the
second (warm) result for comparison — or note "cold" explicitly.

### 8.6 Apple Silicon x86 emulation is lossy and noisy

Run-to-run variance on the M5 Pro was ~5-10% on long sustained-write
workloads. A ~15s test can show ±1-2s swing between runs. Don't
conclude a tuning change worked or didn't work from a single run.
The M3 Max should behave similarly — emulation variance is inherent
to Rosetta 2 and not chip-gen-specific.

### 8.7 Apple Silicon NVMe bandwidth varies significantly between machines

Don't assume cross-Mac storage parity. The M3 Max in the cross-hardware
run had NVMe roughly half the speed of the M5 Pro, which moved benchmark
numbers by 30-60% in the PG-target directions (and was the root cause of
the original §9 predictions being wrong — see §2). If you're comparing
results across machines, either measure NVMe bandwidth first or treat
unexplained regressions as storage-limited until proven otherwise.

### 8.8 Pre-existing Azure SQL Edge volumes are risky to mount into SQL Server 2022

If the host already has an `mssql-bench-data` volume from a prior Azure
SQL Edge session (the native arm64 image, internal version ~921,
vintage SQL Server 2017), **do not mount it directly into a
`mcr.microsoft.com/mssql/server:2022-latest` container**. SQL Server 2022
will attempt an in-place upgrade of the system databases on first start,
which can fail with cryptic errors on Edge-specific metadata.

Safe procedure for reusing the data from such a volume:

1. Create a fresh `mssql-source-data` volume and start SQL Server 2022
   against it, letting it initialize clean system databases.
2. Run a throwaway Alpine container with the old volume mounted
   read-only at `/src` and the new volume mounted read-write at `/dst`.
3. Copy **only** `StackOverflow2010.mdf` and `StackOverflow2010_log.ldf`
   (not any system DB files), `chown 10001:10001`, `chmod 660`.
4. Inside the SQL Server 2022 container, run
   `CREATE DATABASE StackOverflow2010 ... FOR ATTACH`. The version upgrade
   from 921 → 957 runs incrementally and succeeds in ~5 seconds.

The original Azure SQL Edge volume is never modified and remains as a
backup. Detailed procedure: see [`benchmark-results-m3-max.md`](benchmark-results-m3-max.md) §5.1.

### 8.9 Phase 4 PK creation is instantaneous on MSSQL targets (not a bug)

When the target is MSSQL, Phase 4 logs lines like:

```
Created PK on dbo.Votes (10143364 rows) in 0.00s
Created PK on dbo.Posts (3729195 rows) in 0.01s
```

These are not real PK creation times. The MSSQL target dialect creates
primary keys inline as part of `CREATE TABLE`, so the Phase 4 post-load
step is a no-op for MSSQL. The PK cost is already baked into the
per-table transfer times reported earlier in the run.

PG targets behave differently: PG's COPY path bypasses constraints, so
PK creation runs as a real post-load index build in Phase 4 (several
seconds for Posts/Votes/Comments).

**Consequence for analysis:** `transfer-only vs e2e` duration splits are
**not directly comparable across target types**. When comparing MSSQL
and PG targets, compare e2e durations, not transfer-only.

---

## 9. Cross-hardware results (M5 Pro 24 GB vs M3 Max 36 GB)

A first pass at this section contained predictions that were wrong in
several directions (not just magnitude — the *sign* of the pg→pg delta
was backwards). The actual measured numbers from both hosts live here
as a reference, plus the corrected model that explains them.

Full M3 Max details including per-table timings, cold vs warm runs,
and new gotchas: [`benchmark-results-m3-max.md`](benchmark-results-m3-max.md).

### Actual results (warm runs, 2026-04-11)

| Direction | M5 Pro 24 GB | M3 Max 36 GB | Delta | Throughput (M3 Max) |
|---|---:|---:|---:|---:|
| pg → pg | 16.5s | **25.6s** | **+55% slower** | 755K r/s |
| mssql → pg | 43.3s | **42.9s** | ~equal | 450K r/s |
| pg → mssql | 104.8s | **63.8s** | **-39% faster** | 302K r/s |
| mssql → mssql | 98.6s | **36.4s** | **-63% faster** | 530K r/s |
| **Total wall time** | **263.2s** | **168.7s** | **-36% overall** | |
| Catastrophic failure risk | Real (hit 2x) | Low (no failures observed) | | |

The M3 Max wins the overall cross-section by 36%, but **only two of four
directions actually improved**. PG-target directions regressed because of
slower NVMe; MSSQL-target directions won big because of larger MSSQL
buffer pools. This is the corrected model from §2 applied to real data.

### What changed vs the original §9 predictions

The original predictions assumed the workload was "memory-bound in the
Docker VM" and that more RAM would help every direction. That was wrong:

| Direction | Original prediction | Actual | Why prediction was wrong |
|---|---:|---:|---|
| pg → pg | 14-16s | 25.6s | PG-target is storage-bound, not memory-bound. Slower NVMe on M3 Max dominates; larger RAM doesn't help. |
| mssql → pg | 22-28s | 42.9s | MSSQL buffer pool wins on the read side but PG target-side writes still hit disk. Wash. |
| pg → mssql | 80-90s | 63.8s | MSSQL target buffer pool is *more* impactful than predicted — dirty pages absorb in RAM so aggressively that the tiberius LOB INSERT path was never the real ceiling. |
| mssql → mssql | 60-70s | 36.4s | Both sides benefit from the bigger buffer pools simultaneously; the win compounds. |

Error pattern: predictions assumed RAM helped source-read and target-write
equally, but in practice **extra RAM only helps target-write meaningfully
on MSSQL** (because of the checkpoint-based flush model). PG's COPY path
bypasses `shared_buffers` aggressively enough that a bigger cache doesn't
speed up the target side — only the source side, and the gain is small.

### Per-table detail: `mssql → pg`

| Table | M5 Pro (actual) | M3 Max (actual) | Δ |
|---|---:|---:|---:|
| Posts (dominant) | 26.67s | 34.3s | **+28% slower** (slower NVMe on target write hurts) |
| Comments | 5.01s | 16.4s | **+227% slower** (same reason, scaled by row count) |
| Votes (3 partitions) | 3.24s | ~4.5s | slower |
| Badges, Users, PostLinks | <1s each | <1s each | ~equal |

Posts is still the wall-clock bottleneck on both hosts, but the distribution
of time inside it shifted: on the M3 Max the read side is actually *faster*
(MSSQL buffer pool) while the write side is slower (PG target NVMe). On
the M5 Pro it was the opposite — read was slow (MSSQL hitting disk) and
write was OK.

### If you're testing on a different host

Use the §2 model to predict before running:

1. **Is the target MSSQL?** You'll benefit from more RAM proportionally to
   how much of the `max server memory` you can actually give it.
2. **Is the target PG?** Storage bandwidth matters more than RAM. A newer
   machine with faster NVMe and less RAM will likely beat an older machine
   with more RAM but slower storage.
3. **Is the source MSSQL and the target PG?** Mixed case — depends on
   which side is heavier. Posts LOB tables shift the bottleneck to the
   target-write side.
4. **Emulation overhead** is roughly constant across Apple Silicon
   generations (Rosetta 2 is mature). Don't expect chip-generation
   speedups to be large even on MSSQL-heavy directions.

---

## 10. Reporting results from new hosts

If you're running this playbook on a host not already represented in §9,
capture:

1. **Per-direction warm numbers** — end-to-end duration and rows/sec for
   each of the four directions (pg→pg, mssql→pg, pg→mssql, mssql→mssql).
   Report the *second* (warm) run of each; note cold numbers separately
   if they differ materially.
2. **Per-table timings** — at minimum the per-direction `transferred X
   rows in Y` log lines for the dominant tables (Posts, Comments, Votes).
   Grep pattern: `grep -a -E "transferred [0-9]+ rows|partitioning into|Phase 4|Migration completed"`.
3. **State schema validation** — the §7 query against `_dmt_rs.table_state`
   confirming every table shows `rows_transferred = rows_total` and
   `table_status = 'completed'`.
4. **Host profile** — chip, RAM, Docker Desktop VM memory setting, and a
   rough NVMe bandwidth number if you can measure it (`dd if=...
   of=/dev/null bs=1M count=1024` is a crude but useful proxy).
5. **New gotchas** — anything not in §8. Especially new failure modes or
   container orchestration tricks needed for the environment.

Output location: `docs/benchmark-results-<hostname>.md` (following the
existing `benchmark-results-m3-max.md` pattern), committed to `main`
alongside a one-row update to §9's cross-hardware results table in this
file so the summary stays coherent.

If any reading differs from the §2 target-write-pattern model's
prediction by more than 30%, investigate before declaring the result —
that's where the interesting findings live.

---

## Related docs

- [`benchmark-results-m3-max.md`](benchmark-results-m3-max.md) — actual M3 Max 36 GB results + the corrected target-write-pattern model
- [`tech-specs.md`](tech-specs.md) — supported versions, config schema, exit codes
- [`design.md`](design.md) — architecture, transfer engine, plugin pattern
- [`philosophy.md`](philosophy.md) — why the tool exists, what it is NOT
- [`mssql-client-spike.md`](mssql-client-spike.md) — the `mssql-client` alternative driver spike
- `../PERFORMANCE.md` — historical benchmark data from native Linux runs (sub-second counts, 162K-300K+ rows/sec ranges)
- `../BENCHMARKS.md` — Rust vs Go comparison benchmarks
- `../run-all-tests.sh` — the 18-permutation integration test matrix
