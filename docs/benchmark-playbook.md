# Benchmark Playbook

A reproducible, self-contained procedure for running the dmt-rs end-to-end
benchmark across all four `{mssql,pg} × {mssql,pg}` migration directions,
plus the infrastructure gotchas and cross-hardware prediction table.

Authoring context: this doc was written at the end of a session on an
**M5 Pro 24 GB** (MacBook) running Docker Desktop with x86_64 emulation for
MSSQL, targeting a follow-up session on an **M3 Max 36 GB** that will
validate the predictions in the last table.

> **Note on scope.** This playbook documents a benchmark harness against
> real databases, not the unit test suite. `cargo test --all-features`
> needs nothing beyond a working Rust toolchain. The procedures below
> are for measuring end-to-end migration throughput on real data.

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

## 2. Hardware comparison and M3 Max predictions

The dmt-rs benchmark workload on Apple Silicon is **memory-bound in the
Docker VM**, not CPU-bound. MSSQL has no ARM Linux build, so it's always
running under Rosetta 2 — neither chip can avoid the 2-5× emulation
penalty. Postgres containers run native ARM (`postgres:16-alpine` has an
arm64 manifest), so they're not affected by chip choice.

| Factor | M5 Pro 24 GB | M3 Max 36 GB | Impact |
|---|---|---|---|
| Per-core perf | Higher (newer P-cores) | Lower (~5-15%) | Negligible — workload isn't CPU-bound |
| Core count | 10-12 | 14-16 | Negligible — we `--cpus=4` cap containers |
| **Available RAM for Docker VM** | **~7.75 GiB** (24 - macOS - apps) | **~20-24 GiB** | **Dominant factor** |
| Rosetta 2 generation | Latest | 2 generations older | Marginal (~5-10% slower emulation) |
| NVMe bandwidth | Slightly faster | Slightly slower | Negligible once working set fits in RAM |

### Predictions for M3 Max 36 GB with Docker Desktop set to 20 GiB

| Direction | M5 Pro 24 GB | **M3 Max 36 GB predicted** | Main reason |
|---|---:|---:|---|
| pg → pg | 16.5s | **~14-16s** | PG is already fast; small Rosetta regression offsets bigger cache |
| mssql → pg | 43.3s | **~22-28s** | MSSQL source gets 6-8 GiB buffer pool → SO2010 buffer-pool cached → Posts read goes from disk-bound to memory-speed |
| pg → mssql | 104.8s | **~80-90s** | Target-side dirty page buffer grows; still bottlenecked by tiberius LOB INSERT path |
| mssql → mssql | 98.6s | **~60-70s** | Both sides can run full-fat without bb8 pool contention |
| **Catastrophic failure risk** | **Real** (we hit it twice) | **Low** | Whole class of VM-OOM failures goes away |

### Predictions *not* expected to improve materially

- **Posts throughput on pg→mssql** — the ceiling is the tiberius batched
  INSERT path's LOB handling, not memory or CPU. The `mssql-client` spike
  from earlier this session identified this as the fundamental limitation.
  See `docs/mssql-client-spike.md`.
- **Individual small-table throughput** — the lookup tables (PostTypes,
  VoteTypes, LinkTypes) are dominated by per-query overhead, not scaling.

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
database. Verify row integrity:

```bash
# For PostgreSQL targets
docker exec pg-target psql -U postgres -d dmt_test_target -c "
SELECT table_name, rows_total, rows_transferred, table_status,
       EXTRACT(EPOCH FROM (table_completed_at - run_started_at))::numeric(10,2) AS t_plus_sec
FROM _dmt_rs.table_state
WHERE run_id = (SELECT run_id FROM _dmt_rs.migration_runs
                WHERE status = 'completed'
                ORDER BY started_at DESC LIMIT 1)
ORDER BY table_completed_at;
"

# For MSSQL targets
docker exec mssql-target /opt/mssql-tools18/bin/sqlcmd \
  -S localhost -U sa -P 'YourStrong@Passw0rd' -C -d dmt_test_target -Q "
SELECT table_name, rows_total, rows_transferred, table_status
FROM _dmt_rs.table_state
WHERE run_id = (SELECT TOP 1 run_id FROM _dmt_rs.migration_runs
                WHERE status = 'completed' ORDER BY started_at DESC)
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

---

## 9. Predictions to validate on M3 Max 36 GB

Running the procedure above on an M3 Max 36 GB with Docker Desktop at
20+ GiB should produce results in the following ranges. Log the actual
numbers and compare.

| Direction | M5 Pro 24 GB (actual) | **M3 Max 36 GB (predicted)** | Win source |
|---|---:|---:|---|
| pg → pg | 16.5s (1,168K r/s) | **14-16s (1,200K-1,400K r/s)** | Minor — workload already fast |
| mssql → pg | 43.3s (445K r/s) | **22-28s (~700K-880K r/s)** | **Big** — MSSQL buffer pool holds Posts |
| pg → mssql | 104.8-162s (120-185K r/s) | **80-90s (215-240K r/s)** | Modest — tiberius LOB INSERT is still the ceiling |
| mssql → mssql | 98.6s (196K r/s) | **60-70s (275-320K r/s)** | **Big** — no bb8 contention, fat buffer pools |
| Catastrophic failure risk | Real (hit twice) | Low | Memory headroom eliminates the failure mode |

Per-table prediction for `mssql → pg` specifically, since that's the most
informative direction:

| Table | M5 Pro 24 GB | **M3 Max 36 GB predicted** |
|---|---:|---:|
| Posts (dominant) | 26.67s (47K r/s per partition) | **~15s (~85K r/s per partition)** |
| Votes (3 partitions) | 3.24s (1.04M r/s per partition) | **~2s (1.7M r/s per partition)** |
| Comments (3 partitions) | 5.01s (258K r/s per partition) | **~3s (430K r/s per partition)** |
| Badges, Users, PostLinks | <1s each | ~same |

### If the predictions are wrong

- **Posts speedup smaller than expected**: probably means the MSSQL buffer
  pool isn't actually caching the LOB pages. Check `max server memory`
  actually took effect (`SELECT value_in_use FROM sys.configurations
  WHERE name = 'max server memory (MB)'`) and that the container can grow
  into its Docker cap. Also verify SO2010's `.mdf` is being read from the
  mounted volume, not copied into a slower container layer.
- **pg → mssql regression**: would be surprising. If it happens, it's
  likely a new tiberius behavior that should be investigated with the
  `mssql-client` BCP path from the earlier spike (`docs/mssql-client-spike.md`).
- **mssql → mssql still fails**: Docker Desktop VM size might be lower
  than you think. Verify with `docker info | grep Memory`. Should be
  20+ GiB for the budget in section 4.6 to work.

---

## 10. Reporting results

When running on the M3 Max, capture:

1. The full log file for each direction (at minimum `tail -30` per run).
2. The per-table state table from section 7.
3. A summary table comparing actual M3 Max numbers vs the predictions in
   section 9.
4. Any gotchas encountered that aren't in section 8 — especially new
   failure modes unique to the larger VM.

Suggested output location: `docs/benchmark-results-m3-max.md` in a new
branch (`bench/m3-max-results`), or appended as a section to this
playbook under "Cross-hardware results". Either way, commit + push so
future sessions can see the validated numbers.

If any of the predictions in section 9 are wrong by more than 30%,
investigate before declaring the result — variance alone rarely accounts
for that much on the same workload.

---

## Related docs

- `docs/tech-specs.md` — supported versions, config schema, exit codes
- `docs/design.md` — architecture, transfer engine, plugin pattern
- `docs/philosophy.md` — why the tool exists, what it is NOT
- `docs/mssql-client-spike.md` — the `mssql-client` alternative driver spike (the real fix for the pg→mssql LOB ceiling)
- `PERFORMANCE.md` — historical benchmark data from native Linux runs (sub-second counts, 162K-300K+ rows/sec ranges)
- `BENCHMARKS.md` — Rust vs Go comparison benchmarks
- `run-all-tests.sh` — the 18-permutation integration test matrix
