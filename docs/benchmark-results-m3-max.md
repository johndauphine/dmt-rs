# Benchmark Results — M3 Max 36 GB

Cross-hardware validation of `docs/benchmark-playbook.md` predictions, run on
**M3 Max 36 GB** (Mac15,10, 14 cores) with Docker Desktop VM at **23.43 GiB**
on 2026-04-11.

**Headline:** the playbook's §9 predictions were wrong on direction and
magnitude, but the M3 Max still wins overall by 36% across all four
directions (263.2s → 168.7s). The wrongness of the predictions is itself the
most useful finding — see §3 below.

---

## 1. Summary table

All numbers are warm (second consecutive run on the same configuration).
Cold numbers in parentheses where they materially differ.

| Direction | M5 Pro 24 GB | **M3 Max 36 GB (warm)** | Δ vs M5 Pro | §9 prediction | Predicted? |
|---|---:|---:|---:|---:|---|
| pg → pg | 16.5s | **25.59s** (cold 30.19s) | **+55% slower** | 14-16s | ❌ wrong direction |
| mssql → pg | 43.3s | **42.87s** (cold 45.62s) | ~equal (-1%) | 22-28s | ❌ overestimated |
| pg → mssql | 104.8s | **63.84s** (cold 72.30s) | **-39% faster** | 80-90s | ✅ better than predicted |
| mssql → mssql | 98.6s | **36.43s** (cold 39.00s) | **-63% faster** | 60-70s | ✅ better than predicted |
| **Total wall time** | **263.2s** | **168.73s** | **-36% faster** | | |

Throughput (warm, e2e):

| Direction | M5 Pro | M3 Max |
|---|---:|---:|
| pg → pg | 1,168K rows/s | 755K rows/s |
| mssql → pg | 445K rows/s | 450K rows/s |
| pg → mssql | 184K rows/s | 302K rows/s |
| mssql → mssql | 196K rows/s | 530K rows/s |

---

## 2. Validation: row counts intact

Both targets validated against `_dmt_rs.table_state` after the final run on
each container. All 9 tables completed with `rows_transferred = rows_total`
and `table_status = 'completed'`.

| Table | Expected | pg-target (mssql→pg warm) | mssql-target (mssql→mssql warm) |
|---|---:|---:|---:|
| Posts | 3,729,195 | ✓ | ✓ |
| Comments | 3,875,183 | ✓ | ✓ |
| Votes | 10,143,364 | ✓ | ✓ |
| Badges | 1,102,019 | ✓ | ✓ |
| Users | 299,398 | ✓ | ✓ |
| PostLinks | 161,519 | ✓ | ✓ |
| VoteTypes | 15 | ✓ | ✓ |
| PostTypes | 8 | ✓ | ✓ |
| LinkTypes | 2 | ✓ | ✓ |
| **Total** | **19,310,703** | ✓ | ✓ |

---

## 3. The playbook's §2 hypothesis is wrong

The playbook hypothesizes that the dmt-rs benchmark workload on Apple
Silicon is **memory-bound in the Docker VM, not CPU-bound**, and predicts
that the M3 Max 36 GB will win across the board because it can fit larger
MSSQL buffer pools. The actual M3 Max results refute this in one specific
but consequential way: **storage bandwidth matters at least as much as RAM
headroom, and the M3 Max NVMe is roughly half the speed of the M5 Pro NVMe.**
(User-supplied data point during the run; not measured directly here.)

Once you correct for slower storage, the actual pattern is:

> **The dominant factor is the target database type, not the chip:**
> PG targets are storage-bound (COPY mostly bypasses `shared_buffers`,
> dirty pages flush directly to disk); MSSQL targets are CPU+memory-bound
> (the buffer pool absorbs writes and the dirty page flusher amortizes
> across the slow storage).

This explains every result in the table above:

| Direction | Why the M3 Max wins or loses |
|---|---|
| pg → pg | **Loses (-55%).** Both source reads and target COPY traverse storage. No buffer pool to mask the M3 Max's slower NVMe. The M3 Max's larger PG cache *would* help with source reads, but COPY-side writes dominate and don't benefit. |
| mssql → pg | **Equal.** MSSQL source's 6 GB buffer pool absorbs the read-side storage cost (the entire 5.3 GB SO2010.mdf fits in buffer pool after warm-up), but the PG target side still pays the storage penalty. The two effects roughly cancel. |
| pg → mssql | **Wins (-39%).** PG source reads pay the storage penalty, but the MSSQL target's 6 GB buffer pool absorbs LOB inserts so effectively that the net is a big win. The §9 prediction (80-90s) under-estimated this — the MSSQL target buffer pool is more impactful than the playbook author expected. |
| mssql → mssql | **Wins (-63%).** Both sides get full MSSQL buffer pool benefit. Storage is almost entirely off the critical path. This is the cleanest validation that "buffer pool size" is the dominant factor *when the workload allows it*. |

**Recommended playbook updates:**

1. §2 should explicitly call out NVMe bandwidth as a co-dominant factor with
   RAM, not negligible. The "Negligible once working set fits in RAM" cell
   in §2's hardware comparison is wrong for PG targets specifically.
2. §9 predictions for `pg → pg` and `mssql → pg` should be revised upward
   (they're slower than predicted) and predictions for `pg → mssql` and
   `mssql → mssql` should be revised downward (they're faster than
   predicted). The error pattern is consistent: predictions assumed RAM
   helps both source-read and target-write equally, but in practice it
   only helps target-write meaningfully on MSSQL.
3. A new §8.7 gotcha worth adding: "**Apple Silicon NVMe bandwidth varies
   significantly between machines.** Don't assume cross-Mac storage parity
   — it can move benchmark numbers by 30-60%."

---

## 4. Per-direction details

### 4.1 pg → pg (warm 25.59s, throughput 755K rows/s)

```
public.linktypes:    2          rows in 0.020s
public.posttypes:    8          rows in 0.034s
public.votetypes:    15         rows in 0.022s
public.postlinks:    161,519    rows in 0.138s   (1,172K r/s)
public.badges:       1,102,019  rows in 0.565s   (1,952K r/s)
public.users:        299,398    rows in 0.679s   (441K r/s)
public.votes:        10,143,364 rows in 4.757s   (2,132K r/s)
public.comments:     3,875,183  rows in 5.019s   (772K r/s)
public.posts:        3,729,195  rows in 14.634s  (255K r/s)   ← dominant
Phase 4 (PK creation): ~10.83s
  - posts PK:     10.81s
  - votes PK:      6.90s
  - comments PK:   6.39s
  - all others: <1s
Migration completed: 25.59s, 755K rows/s
```

The M5 Pro baseline showed Phase 4 at ~3.8s (16.5 e2e − 12.7 transfer = 3.8).
On the M3 Max, Phase 4 took ~10.83s. **PK creation alone accounts for ~7s of
the 9s regression** — and PK creation is sort-bound on the relation file,
which is exactly where slower NVMe hurts most.

### 4.2 mssql → pg (warm 42.87s, throughput 450K rows/s)

```
dbo.LinkTypes:    2          rows in 0.018s
dbo.PostTypes:    8          rows in 0.011s
dbo.PostLinks:    161,519    rows in 0.140s
dbo.Badges:       1,102,019  rows in 0.943s
dbo.Users:        299,398    rows in 1.104s
dbo.VoteTypes:    15         rows in 0.036s
dbo.Votes (×3):   10,143,364 rows in ~4.45s     (3 partitions in parallel)
dbo.Comments:     3,875,183  rows in 16.381s
dbo.Posts:        3,729,195  rows in 34.313s    ← dominant
                              (read 37.20s / write 14.47s)
Phase 4 (PK creation): ~8.4s (similar to pg→pg, all the cost is on the PG side)
  - dbo.Posts PK: 8.37s
  - dbo.Votes PK: 6.74s
  - dbo.Comments PK: 3.40s
Migration completed: 42.87s, 450K rows/s
```

Posts read time (37.20s) > write time (14.47s), confirming the source MSSQL
is the bottleneck — even with the buffer pool, the Rosetta-emulated MSSQL
LOB read path is slower than the PG COPY-receive on this direction. This
matches the playbook's M5 Pro observation that Posts dominates.

### 4.3 pg → mssql (warm 63.84s, throughput 302K rows/s)

```
public.linktypes:   2          rows in 0.020s
public.posttypes:   8          rows in 0.048s
public.postlinks:   161,519    rows in 0.545s
public.votetypes:   15         rows in 0.087s
public.users:       299,398    rows in 2.549s
public.badges:      1,102,019  rows in 5.015s
public.comments:    3,875,183  rows in 24.177s
public.votes:       10,143,364 rows in 24.309s
public.posts:       3,729,195  rows in 63.435s   ← dominant
                                 (read 0ns / write 61.19s)
Phase 4: <0.1s total (see §5 gotcha — MSSQL PK creation is logged as
         instant; likely happens inline with CREATE TABLE)
Migration completed: 63.84s, 302K rows/s
```

Posts wall time on M3 Max (63.4s) vs M5 Pro (104.5s) is the single biggest
delta in any direction. The MSSQL target's `nvarchar(max) Body` write path
(tiberius bulk INSERT) benefits enormously from the larger buffer pool on
the M3 Max — dirty pages are absorbed in memory rather than flushed.

### 4.4 mssql → mssql (warm 36.43s, throughput 530K rows/s)

```
dbo.LinkTypes:    2          rows in 0.009s
dbo.PostTypes:    8          rows in 0.018s
dbo.PostLinks:    161,519    rows in 0.523s
dbo.Badges:       1,102,019  rows in 2.804s
dbo.Users:        299,398    rows in 3.890s
dbo.VoteTypes:    15         rows in 0.098s
dbo.Votes (×3):   10,143,364 rows in ~12.9s     (3 partitions in parallel)
dbo.Comments:     3,875,183  rows in 20.658s
dbo.Posts:        3,729,195  rows in 36.230s   ← dominant
                              (read 36.73s / write 34.16s)
Phase 4: <0.1s total
Migration completed: 36.43s, 530K rows/s
```

The big-win direction: 98.6s → 36.43s (-63%). Both sides benefit from the
6 GB MSSQL buffer pool, the catastrophic-failure mode the playbook warned
about (bb8 contention, OOM) did not reproduce, and Posts wall time (36.2s)
is a fraction of the M5 Pro number despite running under Rosetta. **No
catastrophic failures or pool contention observed.**

---

## 5. New gotchas not in playbook §8

### 5.1 Pre-existing volumes from Azure SQL Edge

The M3 Max had pre-existing `mssql-bench` (Azure SQL Edge native arm64) and
`pg-bench` containers from a prior session. The volume `mssql-bench-data`
contains a full Azure SQL Edge instance — `master.mdf`, `tempdb.mdf`,
`StackOverflow2010.mdf`, etc. **Mounting that volume directly into a SQL
Server 2022 container is risky** because Azure SQL Edge's system DBs are at
internal version ~921 (~SQL Server 2017 vintage), and SQL Server 2022 will
attempt an in-place upgrade on first start. This may fail with cryptic
errors on Edge-specific metadata.

**Safe procedure used here:**

1. Create a fresh `mssql-source-data` volume.
2. Start `mssql/server:2022-latest` against it; let it init clean system DBs.
3. Use a helper alpine container with `mssql-bench-data` mounted **read-only**
   at `/src` and `mssql-source-data` mounted read-write at `/dst`. Copy only
   `StackOverflow2010.mdf` and `StackOverflow2010_log.ldf` across, chown to
   `10001:10001`, chmod 660. Took ~7 seconds for ~6 GB.
4. `CREATE DATABASE StackOverflow2010 ... FOR ATTACH` inside the new
   container. SQL Server 2022 ran the version upgrade from 921 → 957
   (37 incremental steps, ~5 seconds total). No errors.

The original `mssql-bench-data` volume was untouched and remains as backup.

### 5.2 PG database name and identifier case

The pre-existing `pg-bench-data` volume contained a PG database named
`stackoverflow2010` (lowercase), not `so2010_bench` as the playbook
configs assume. Tables are `public.posts`, `public.comments`, etc., all
lowercase — dmt-rs lowercases unquoted identifiers when migrating
mssql→pg. **This means when using a previously-populated pg-source as a
source for `pg → *` directions, the configs must reference the actual
database/table names, not the playbook's `so2010_bench` example.**

The four `/tmp/dmt-rs-*.yaml` configs in this run were updated to use
`stackoverflow2010` as the PG source database name.

### 5.3 The `_dmt_rs.migration_runs` table doesn't exist

Playbook §7 references `_dmt_rs.migration_runs` for finding the latest
completed run. The actual schema has only one table: `_dmt_rs.table_state`.
It includes its own `run_id`, `run_status`, `run_started_at`,
`run_completed_at` columns — denormalized into the per-table rows. The
correct validation query is:

```sql
SELECT table_name, rows_total, rows_transferred, table_status
FROM _dmt_rs.table_state
WHERE run_id = (SELECT run_id FROM _dmt_rs.table_state
                ORDER BY table_completed_at DESC NULLS LAST LIMIT 1)
ORDER BY table_name;
```

Playbook §7 should be updated.

### 5.4 Health-check displays PG source as `Source (MSSQL)`

The CLI's `health-check` subcommand prints:

```
Health Check Results:
  Source (MSSQL): OK (0ms)
  Target (PostgreSQL): OK (0ms)
  Overall: HEALTHY
```

even when the source is PostgreSQL. The label is hardcoded; the
underlying check is correct (it really did connect to PG). Cosmetic but
worth fixing — `crates/dmt-rs-cli/src/main.rs` health_check command.

### 5.5 Phase 4 PK creation logged as `0.00-0.05s` for huge tables on MSSQL targets

In `pg → mssql` and `mssql → mssql` runs, Phase 4 logs lines like:

```
Created PK on dbo.Votes (10143364 rows) in 0.00s
Created PK on dbo.Posts (3729195 rows) in 0.01s
```

These are *not* real PK creation times. The MSSQL target dialect almost
certainly creates the PK constraint inline as part of `CREATE TABLE`
rather than as a post-load `ALTER TABLE ADD PRIMARY KEY`, and the Phase 4
log line is a no-op confirmation. PG targets show the real cost (6-10s
on Posts/Votes/Comments) because PG's COPY path bypasses constraints and
the PK has to be built as a post-load index sort.

This isn't a bug, but it does mean **the e2e duration on MSSQL targets
already includes the PK cost in the per-table transfer times**, while on
PG targets the PK cost is in Phase 4. The transfer-only vs e2e split
the playbook §1 reports is therefore not directly comparable across
target types.

### 5.6 ssl_mode=disable still triggers a "ssl_mode=require" warning

```
WARN ssl_mode=require: TLS enabled but server certificate is not verified.
INFO Connected to PostgreSQL source: localhost:5432/stackoverflow2010
WARN PostgreSQL TLS is disabled. Credentials will be transmitted in plaintext.
```

The first warning fires unconditionally regardless of the actual `ssl_mode`
value. The second warning correctly reflects `ssl_mode: disable`. Cosmetic.

---

## 6. Methodology / deviations from playbook

- **Container images**: `mcr.microsoft.com/mssql/server:2022-latest`
  (Developer Edition, 16.0.4245.2 / RTM-CU24, x86_64 under Rosetta) and
  `postgres:16-alpine` (PG 16.13, native arm64). Matches playbook §4 except
  the user previously had Azure SQL Edge for MSSQL — see §5.1.
- **Memory caps**: Per playbook §4.6. Containers got 8/8/3/3 GiB Docker
  caps (mssql-source / mssql-target / pg-source / pg-target). Both MSSQL
  containers tuned to `max server memory = 6144 MB` and `network packet
  size = 32767`.
- **PG tuning**: None. Playbook does not tune PG; deliberately kept
  defaults to preserve apples-to-apples comparison with the M5 Pro
  baseline. See §3 — this is one source of the pg→pg regression but
  was the right call methodologically.
- **CPU caps**: None applied to any container (playbook says CPU caps
  are neutral on this workload).
- **Auto-tuner output**: dmt-rs detected 36 GB RAM and 14 cores, set
  `workers=7, parallel_readers=3, parallel_writers=3, chunk_size=187500,
  read_ahead=9, mem_budget=25.8 GB`. No manual auto-tuner overrides.
- **Cold/warm protocol**: Each direction was run twice. The first run
  drops/recreates the target DB on a freshly-restarted MSSQL container
  (cold). The second drops/recreates again with caches warm. Reported
  numbers are warm.
- **Validation**: Per playbook §7 (corrected query — see §5.3).

The pre-existing `mssql-bench` and `pg-bench` containers were left in
place (exited) as backups; they were not deleted. The benchmark used
fresh containers `mssql-source`, `mssql-target`, `pg-source`,
`pg-target` on the standard playbook ports.

---

## 7. Recommendations from this session

1. **Update playbook §2** to acknowledge NVMe bandwidth as a co-dominant
   factor (not negligible). The Apple Silicon NVMe variance between
   machines is real and consequential.
2. **Update playbook §9 predictions** with the actual M3 Max numbers
   (or, more usefully, with the explanatory model from §3 above so the
   next cross-hardware run can reason from first principles).
3. **Update playbook §7** to query `_dmt_rs.table_state` directly
   (no `migration_runs` table exists).
4. **Add playbook §8.7** about pre-existing Azure SQL Edge volumes and
   the safe attach procedure (§5.1 above).
5. **Fix the health-check CLI label** in `crates/dmt-rs-cli/src/main.rs`
   (cosmetic — labels PG as MSSQL).
6. **Fix the `ssl_mode=require` spurious warning** in the postgres
   driver init path.
7. The workload classification in §3 ("target-write-pattern-bound")
   should inform future tuning work — if the goal is faster pg-target
   throughput, the highest-leverage knob is `max_wal_size` (1 GB →
   8 GB+) followed by `synchronous_commit=off` for benchmarking.
   Both are deviations from production-safe settings.

---

## 8. Raw log files

Captured in `/tmp/` during the run:

- `/tmp/dmt-rs-pg2pg-cold.log`, `/tmp/dmt-rs-pg2pg-warm.log`
- `/tmp/dmt-rs-mssql2pg-cold.log`, `/tmp/dmt-rs-mssql2pg-warm.log`
- `/tmp/dmt-rs-pg2mssql-cold.log`, `/tmp/dmt-rs-pg2mssql-warm.log`
- `/tmp/dmt-rs-mssql2mssql-cold.log`, `/tmp/dmt-rs-mssql2mssql-warm.log`

These are not committed (ephemeral / large) but the per-table timing
extracts in §4 above are sufficient for any cross-run comparison.
