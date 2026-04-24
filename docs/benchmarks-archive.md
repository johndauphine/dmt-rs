# Benchmarks Archive

Narrative writeups of dmt-rs benchmark experiments, preserved for
reasoning and methodology. **All numbers in this document are
superseded by [`benchmarks.md`](benchmarks.md)** — refer to it for
current figures. Use this file to understand *how* conclusions were
reached and *why* specific tuning choices are in place.

Ordering: most-recent-learning first.

---

## 1. PG upsert profile: merge SQL dominates, not staging (v1.46, 2026-04-23)

**Purpose:** After §6's correction removed "missing `IS DISTINCT
FROM`" as the cause of the Go-vs-dmt-rs PG-upsert gap, profile where
dmt-rs's PG upsert path actually spends time. Answer which layer to
optimize before any further attribution.

**Method:**

- **Workload:** `pg → pg upsert`, 19.3 M rows (StackOverflow 2010),
  target pre-populated identical to source so `IS DISTINCT FROM`
  skips every row. Isolates the staging-and-filter cost from actual
  write cost.
- **Binary:** dmt-rs v1.46.0 release build.
- **Host:** M3 Max 36 GB. `pg-source` / `pg-target` containers, 3 GB
  cgroups each.
- **Instrumentation:** PG target `log_min_duration_statement = 0`,
  `log_statement = 'all'` — captures every statement + its duration.
  Client wrapped in `samply record` for CPU profile (browser-side
  symbolication, not directly analyzed here since the server is
  plainly the bottleneck).

**Wall:** 35.0 s at 552 K rows/s. 3 parallel writers.

**Server-side breakdown (146 s across 3 backends):**

| Component | Total | % of server work | Avg / chunk |
|---|---:|---:|---:|
| `INSERT … ON CONFLICT` merge | 89.6 s | **61 %** | 270–435 ms |
| `COPY` into staging table | 52.5 s | 36 % | 205 ms |
| `CREATE TEMP` + `TRUNCATE` + `DROP` | 4.0 s | 2.7 % | < 10 ms |
| `_dmt_rs` state bookkeeping | 0.02 s | < 0.1 % | 0.1 ms |

**EXPLAIN ANALYZE on one 50 K-row Posts chunk:**

| Variant | Duration | Shared hits | Dirtied | Written |
|---|---:|---:|---:|---:|
| `DO UPDATE … WHERE IS DISTINCT FROM` (what dmt-rs emits) | 294 ms | 257 K | 4 851 | 0 |
| `DO NOTHING` (floor — just conflict detection) | 227 ms | 204 K | 2 617 | 0 |
| `DO UPDATE` unconditional (force rewrite) | 748 ms | 587 K | 17 313 | 3 044 |

**Findings:**

1. **Per-chunk staging roundtrip is not the culprit.** `CREATE TEMP`
   + `TRUNCATE` + `DROP` sum to 4 s — 2.7 % of server work. This
   refutes the corrected-§6 follow-up hypothesis that the staging
   roundtrip explained the Go gap.
2. **The merge SQL itself is the bottleneck.** Every chunk: 50 000
   btree probes on `posts_pkey` + 50 000 heap fetches for conflict
   resolution + `IS DISTINCT FROM` evaluation across 19 non-PK cols,
   including TOAST detoast of `body` on both sides (OR short-circuits
   on TRUE; with target = source every column evaluates FALSE, so
   every column is tested).
3. **`IS DISTINCT FROM` costs only 66 ms per chunk (23 %).** Removing
   it (switching to `DO NOTHING`) saves ~23 %. Keeping it prevents a
   454 ms per-chunk unconditional UPDATE — the filter *is* doing real
   work, not pure overhead.
4. **The floor is the PK-probe path itself: 227 ms per 50 K-row
   chunk.** Unavoidable for any `ON CONFLICT` shape. To go
   meaningfully faster, reduce the number of PK probes, not the
   conflict action.

**Implication for the Go gap (§6):**

The 34 s dmt-rs vs 24 s Go gap on `pg → pg` upsert cannot be closed
by tweaking the conflict filter. Even eliminating `IS DISTINCT FROM`
entirely would trim only ~6 s off the parallelized wall. Candidate
explanations for Go's 24 s:

- **Source-side watermark** (`date_updated_columns` — see §7). Takes
  full-run time to effectively zero. Falsified below.
- **PG 15 `MERGE`** with different arbiter semantics. Worth a
  plan-comparison experiment on the same chunk.
- **Different write parallelism** at the Go dispatcher — if Go runs
  more parallel backends against the same PG cgroup, it completes
  faster purely from parallelism. Our auto-tuner picked
  `parallel_writers=3`.
- **Client-side PK diff** (one `SELECT id FROM target WHERE id
  BETWEEN ? AND ?` per chunk range, partition into new/existing,
  plain `INSERT` for new rows with no `ON CONFLICT`). Unverified.

**Watermark hypothesis test (follow-up, 2026-04-23):**

Configured the same bench with `date_updated_columns:
[lastactivitydate, lasteditdate, creationdate, lastaccessdate, date]`
and ran twice back-to-back against the populated target.

| Run | State at start | Rows transferred | Wall |
|---|---|---:|---:|
| 1 | empty (cold) | 19 310 703 | 35.7 s |
| 2 | populated from run 1 | **25** | **0.75 s** |

Run 2 is 48× faster than run 1. The 25 rows are exactly the 3 lookup
tables (`linktypes` 2 + `posttypes` 8 + `votetypes` 15) that have no
date columns and correctly fall back to full scan, matching §7's
prediction.

**Conclusion:** Go's 24 s on `pg → pg` upsert cannot have been from
a watermark — with one configured, the same workload finishes in
0.75 s, not 24 s. The watermark-asymmetry hypothesis is **falsified**.
The ~10 s dmt-rs-vs-Go gap is within the full-upsert path itself,
not a configuration difference.

**Side-by-side Go run + parallelism test (follow-up, 2026-04-23):**

Built Go `dmt` from main (`v3.54.0-99-g1b53a16`) and ran the same
pg → pg upsert against the same containers. Captured PG target log
for SQL shape and sampled `pg_stat_activity` at 2 Hz for peak
concurrent active backends.

| Config | Peak active backends | Wall |
|---|---:|---:|
| Go dmt (workers 4, AI-tuned `max_partitions=12`) | 7 | **22.0 s** |
| dmt-rs default (`parallel_writers=3` auto) | ~3 | 31.8 s |
| dmt-rs `parallel_writers=5` + `parallel_readers=5` | — | 32.7 s |
| dmt-rs `parallel_writers=8`, `max_partitions=12`, `large_table_threshold=1 M`, `pg_conns=32` | **32** | 47.3 s |

**SQL shape is identical.** Go emits exactly the same
`INSERT … ON CONFLICT (pk) DO UPDATE SET … WHERE (target-tuple) IS
DISTINCT FROM (EXCLUDED-tuple)` statement dmt-rs does, byte-for-byte
on the column lists. No `MERGE`, no client-side PK diff — plan-shape
and client-diff hypotheses **falsified**.

**Mechanical differences:**

- Staging-table reuse: Go keeps **18 persistent `_stg_<hash>`
  tables**, `TRUNCATE`s between chunks (455 merges × 6.85 COPYs per
  merge). dmt-rs `CREATE TEMP` + `DROP TABLE` per chunk (~600 unique
  staging tables). §1 measured this lifecycle at 4 s of server work.
- COPY chunking: Go emits ~7 COPY statements per merge chunk. dmt-rs
  emits ~1.

**Write-parallelism hypothesis is also falsified.** Pushing dmt-rs to
higher concurrency makes it **slower**, not faster. At
`parallel_writers=8 + max_partitions=12` dmt-rs runs 32 concurrent
backends (vs Go's 7) and lands at 47 s — the PG target's 3 GB cgroup
can't absorb the extra write concurrency. Go at peak 7 finishes in
22 s; dmt-rs at peak 3 finishes in 32 s. Per-backend throughput is
the thing that differs, not scheduler concurrency.

**Status:** four hypotheses ruled out (missing `IS DISTINCT FROM`,
staging roundtrip, watermark asymmetry, writer parallelism), one
ruled out by the side-by-side (`MERGE` plan shape, client-side PK
diff). Remaining live candidates are all **code-level, not config**:

- **COPY BINARY encoder throughput.** Rust `postgres-protocol` COPY
  may be slower per row than Go's `pgx` COPY. Needs a targeted
  micro-benchmark (stream 50 K rows into a single temp table via
  each client, compare).
- **Staging-table reuse** (the 4 s lifecycle cost in §1's
  breakdown). Code change in the Rust writer, not config.
- **tokio-postgres per-statement / per-connection overhead.**
  Harder to isolate without a profiling probe.

No more cheap config experiments remain. Closing the gap further
requires either a per-row COPY micro-benchmark (to confirm the
encoder candidate) or a staging-reuse prototype in the writer.

---

## 2. MySQL target ceiling is not 120 K / 165 K (v1.45, 2026-04-19)

**Superseded finding:** MySQL target throughput caps at ~120 K rows/s
(M5 Pro) / ~165 K rows/s (M3 Max) due to MySQL's lack of a binary bulk
protocol.

**Actual finding:** Measured 369 K – 452 K rows/s across all MySQL
target directions on v1.45.0. The earlier ceiling was v1.44
post-load PK rebuild overhead plus Rosetta-emulated MSSQL source feed
rate, not a MySQL protocol limitation.

**Why it changed:** v1.45 inline PK in `CREATE TABLE` (commits
`7647ec4` MySQL, `3e4eef6` Postgres) removed the biggest source of
dirty-page pressure during finalization. On v1.44, Posts finalization
held the InnoDB checkpointer saturated for the back half of every
run; on v1.45, there is no separate PK build phase.

**Secondary finding:** MSSQL `max server memory` 4 096 → 10 240 MB
bump yielded only +5-8 % on v1.45 (vs +36-41 % on v1.44 — §3 below).
Inline PK also ate most of the MSSQL-buffer-pool headroom.

Current numbers: see [`benchmarks.md`](benchmarks.md) §1.1.

---

## 3. MSSQL `max server memory` 10 240 MB experiment (v1.44, 2026-04-17)

**Hypothesis:** On a 36 GB host, giving MSSQL 10 GiB `max server
memory` (vs the 4 GiB M5 Pro cap) would unlock the `mssql → mysql`
direction.

**Method:** Two A/B matrices (baseline × full-schema, each with
`mysql_bulk_session_tuning` on/off). n=3 per cell with warm-up
discard, interleaved variant ordering. Compared to M5 Pro 24 GB
baselines at 4 GiB `max server memory`.

**Result (v1.44):**

| Config | M5 Pro baseline | M3 Max 10 GiB | Δ |
|---|---:|---:|---:|
| baseline, tuning-on | 120 K rows/s | 165 K rows/s | +37.2 % |
| baseline, tuning-off | 119 K rows/s | 167 K rows/s | +41.2 % |
| full-schema, tuning-on | 118 K rows/s | 161 K rows/s | +36.3 % |
| full-schema, tuning-off | 117 K rows/s | 161 K rows/s | +37.2 % |

Hypothesis confirmed at dramatic-uplift threshold (> +30 %). Result
was generalized into the playbook recommendation to bump MSSQL to
10 240 MB on ≥ 32 GB hosts.

**Secondary finding:** `mysql_bulk_session_tuning` became noise
(< 0.1 %) at 10 GiB — source-side buffer pool dominated any
target-side session-tuning benefit. This eventually motivated
removing the `mysql_bulk_session_tuning` config knob entirely (PR #120).

**Superseded magnitude:** The v1.45 re-measurement shows only +5-8 %
from the same RAM bump (see §2 above). Inline PK already eliminated
most of the dirty-page pressure that the bigger buffer pool was
absorbing on v1.44.

**Still applies:** The playbook recipe (10 240 MB `max server memory`,
12 GB cgroup) is still the right config on 36 GB hosts — the RAM is
free; the uplift is just smaller than v1.44 measured.

Cross-direction RAM sensitivity as characterized at end of this
experiment:

| Direction | RAM-sensitive? |
|---|---|
| `mssql → mssql` | Yes (98.6 s → 36.4 s on M3 Max) |
| `pg → mssql` | Yes (104.8 → 63.8 s across RAM configs) |
| `mssql → mysql` | Yes (v1.44, +36-41 %) |
| `mssql → pg` | No (flat across RAM configs) |
| `pg → pg` | No (storage-bound) |

---

## 4. M3 Max cross-hardware validation (v1.44, 2026-04-11)

**Purpose:** Test the playbook's §2 hypothesis that Apple Silicon
dmt-rs benchmarks are "memory-bound in the Docker VM, not CPU-bound"
by running the four (`mssql`, `pg`) × (`mssql`, `pg`) directions on
both M5 Pro 24 GB and M3 Max 36 GB.

**Result:** Hypothesis half-wrong. M3 Max wins by 36 % on total wall
time (263 → 169 s), but the pattern contradicted the prediction:

| Direction | M5 Pro | M3 Max | Prediction | Predicted? |
|---|---:|---:|---:|---|
| pg → pg | 16.5 s | **25.6 s** | 14-16 s | ❌ wrong direction |
| mssql → pg | 43.3 s | 42.9 s | 22-28 s | ❌ overestimated |
| pg → mssql | 104.8 s | **63.8 s** | 80-90 s | ✅ better than predicted |
| mssql → mssql | 98.6 s | **36.4 s** | 60-70 s | ✅ better than predicted |

**Corrected model:** The dominant factor is the **target database
type**, not the chip or RAM in isolation.

> **PG target = storage-bound.** COPY flushes dirty pages direct to
> disk; `shared_buffers` helps source reads only. More RAM does almost
> nothing for PG-target throughput. The M3 Max NVMe is roughly half
> the speed of the M5 Pro NVMe, so `pg → pg` actually regressed.
>
> **MSSQL target = CPU+memory-bound.** The buffer pool absorbs LOB
> inserts and the dirty-page flusher amortizes across slow storage.
> More RAM → bigger buffer pool → much faster. `pg → mssql` and
> `mssql → mssql` both benefited dramatically.

**Key new gotcha:** Apple Silicon NVMe bandwidth varies significantly
between machines — don't assume cross-Mac storage parity. This
matters more than the original playbook acknowledged.

This experiment produced the performance model now summarized in
[`benchmarks.md`](benchmarks.md) §2.

---

## 5. MySQL target container tuning (v1.42-era, various dates)

**Purpose:** Find the right MySQL target container configuration for
the SO2010 workload.

**Headline finding (2026-04):** A tuned container (6 GB cgroup /
2 GB buffer pool) delivers **118 K rows/s** versus stock
(3 GB cgroup / 128 MB pool) at **53 K rows/s** — a **~2.5× jump from
container config alone**, larger than any dmt-rs code change shipped or
proposed at that point.

| Container | Throughput |
|---|---:|
| stock 3 GB / 128 MB pool | 52 864 rows/s |
| **tuned 6 GB / 2 GB pool** | **118 639 rows/s** |

(These numbers are v1.42-era; see [`benchmarks.md`](benchmarks.md)
§1.1 for current v1.45 figures.)

**Buffer pool ceiling is InnoDB-internal.** Tested 2 / 3 / 4 GB pools
on the 6 GB container: 2 GB fastest, 3 GB regresses ~5 %, 4 GB OOMs.
Tradeoff is InnoDB checkpointer pressure vs cache-hit rate on
append-heavy bulk load. This is **not** a host-RAM tradeoff; don't
raise above 2 GB on bigger hosts.

**LOAD DATA LOCAL INFILE is slower than batched INSERT.** Re-measured
twice: first on stock mysql:8.0 (where I/O was the bottleneck and TSV
CPU was secondary), then on the tuned container (where I/O is no
longer the gate). Same result both times:

| Config (4 workers) | Median rows/s |
|---|---:|
| `mysql_load_data: never` (INSERT) | 118 874 |
| `mysql_load_data: always` (LOAD DATA) | 105 020 |

Client-side TSV escape-handling (every `\t`, `\n`, `\\`, `\0`, NULL
sentinel per value per row — see `escape_tsv_value` in
`crates/dmt-rs/src/drivers/mysql/writer.rs`) dominates the server-side
bulk-path win. Default stays `never`; feature retained for
single-worker configs.

**Text columns dominate wall time.** Stock-container measurements
showed a **5.7× slowdown** from LOB columns (194 K → 34 K rows/s on
plain-text per-table benchmarks). Posts `nvarchar(max) Body` is the
dominant direction-independent bottleneck in every direction where
MSSQL or MySQL writes are involved.

The tuning rationale and measurement methodology are preserved in the
live doc [`mysql-target-container.md`](mysql-target-container.md) —
that doc describes *why* the tuning is what it is and should stay as
config documentation.

---

## 6. Go vs dmt-rs head-to-head (2026-04-14, M5 Pro 24 GB)

**Purpose:** Side-by-side comparison of `dmt` (Go, v3.54.0) and
`dmt-rs` across the full 2 × 2 × 2 matrix (direction × mode × engine)
on identical infrastructure and same session.

**Scorecard:** dmt-rs wins 1, ties 1, loses 6.

| # | Direction | Mode | Go | dmt-rs | Winner |
|---|---|---|---:|---:|---|
| 1 | mssql → pg | drop_recreate | **28 s** | 36 s | Go 1.30× |
| 2 | mssql → pg | upsert | **22 s** | 50 s | Go 2.26× |
| 3 | pg → mssql | drop_recreate | **46 s** | 78 s | Go 1.69× |
| 4 | pg → mssql | upsert | **85 s** | 110 s | Go 1.29× |
| 5 | pg → pg | drop_recreate | 29 s | **21 s** | **dmt-rs 1.36×** |
| 6 | pg → pg | upsert | **24 s** | 34 s | Go 1.43× |
| 7 | mssql → mssql | drop_recreate | **60 s** | 104 s | Go 1.73× |
| 8 | mssql → mssql | upsert | 95 s | **92 s** | ~tie |

**Accounting for the gap (as of 2026-04-14):**

1. **tiberius batched INSERT throughput.** Caps `drop_recreate`
   directions targeting MSSQL at roughly half of Go. Gap is 1.69× on
   `pg → mssql` and 1.73× on `mssql → mssql`. A potential replacement
   for tiberius is tracked in
   [`mssql-client-spike.md`](mssql-client-spike.md).
2. **Upsert on PG targets.** Root cause is code-level, inside the
   per-backend write path — not config, not plan shape, not
   filtering. The original writeup attributed the gap to missing
   `IS DISTINCT FROM`, but that optimization had already shipped
   in commit `3389cd5` (v1.43.0), two days before this benchmark
   ran. §1 progressively eliminated every remaining config or
   query-shape hypothesis: staging roundtrip is 2.7 % of server
   work, watermark asymmetry is falsified (dmt-rs with watermarks
   finishes in 0.75 s), and a Go side-by-side on v3.54.0-99
   showed identical `INSERT ON CONFLICT ... WHERE IS DISTINCT FROM`
   SQL — no `MERGE`, no client-side PK diff. Parallelism is also
   ruled out: pushing dmt-rs to 32 concurrent backends makes it
   slower (47 s), while Go finishes at peak 7 active backends.
   Remaining candidates are all code-level: COPY BINARY encoder
   throughput per connection, staging-table reuse (Go keeps 18
   persistent `_stg_<hash>` tables, `TRUNCATE`s between chunks),
   and tokio-postgres per-statement overhead. Costs 1.43× on
   `pg → pg` upsert and 2.26× on `mssql → pg` upsert.
3. **MSSQL source reads.** Even on directions where the target is
   fast, Go reads from MSSQL faster than dmt-rs — suggesting tiberius
   source-side query streaming is slower than Go's driver, not just
   the insert side.

**Every upsert cell is a Go win or tie.** dmt-rs's MSSQL `MERGE WITH
(TABLOCK)` per-chunk staging (PRs #100, #102) is competitive on
`mssql → mssql`, but PG-target upsert is the weakest direction.

**Status (2026-04-23):** `drop_recreate` matrix not yet re-run on
M3 Max 36 GB or against v1.45+. Inline PK in v1.45 should narrow
the `drop_recreate` `mssql → *` gaps but is unlikely to change the
upsert ranking, since upsert costs are in the update path, not
finalization. For the **PG-upsert cell specifically** (pg→pg
upsert, row 6), §1 has now eliminated every config-level and
query-shape hypothesis via direct measurement on v1.46 / M3 Max;
the remaining gap is inside the code-level write path and needs
either a COPY-encoder micro-benchmark or a staging-reuse prototype
to resolve further.

---

## 7. Latest incremental-upsert optimization (PR #108, 2026-04-16)

**Change:** `Config::hash()` no longer includes `target_mode`, so a
`drop_recreate` run followed by `upsert` against the same source /
target inherits the watermarks the drop seeded. With
`date_updated_columns` configured, upsert filters at the source and
returns essentially zero rows on unchanged data.

| Scenario (MSSQL → MSSQL, SO2010, M5 Pro) | Before | After |
|---|---:|---:|
| `drop_recreate` (cold) | 37.8 s @ 511 K rows/s | unchanged |
| `upsert` immediately following, no source changes | 85 s @ 227 K rows/s | **5.9 s, 17 rows touched** |

The 5.9 s residual is the three small lookup tables
(`LinkTypes` / `PostTypes` / `VoteTypes`) that have no date column and
correctly fall back to a full scan.

---

## 8. Retired experiments / dead scripts

Benchmarks whose scripts were removed from the repo but whose data
informed current decisions. Recover by:

```bash
git log --diff-filter=D --follow -- scripts/bench-mysql-tuning.sh
git log --diff-filter=D --follow -- scripts/bench-mysql-full-schema.sh
```

- **`bench-mysql-tuning.sh`** — A/B of the `mysql_bulk_session_tuning`
  config knob. Removed when the knob was retired (PR #120) after §3
  above showed it was noise at 10 GiB MSSQL RAM.
- **`bench-mysql-full-schema.sh`** — full-schema variant of the tuning
  bench. Same fate as the knob.

Live bench scripts: `scripts/bench-mysql-load-data.sh`,
`scripts/bench-mysql-inline-pk.sh`, `scripts/bench-postgres-inline-pk.sh`.
