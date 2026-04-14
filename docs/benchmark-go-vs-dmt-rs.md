# Go dmt vs dmt-rs — head-to-head benchmark

Side-by-side comparison of [`dmt`](https://github.com/) (Go, v3.54.0-97-g4b174fa)
and `dmt-rs` (this repo) running the same SO2010 dataset through every cell
of the 2 × 2 × 2 matrix **(direction × mode × engine)**.

This doc replaces the stale numbers in `../BENCHMARKS.md` (January 2026).

## Test environment

| Setting | Value |
|---|---|
| Date | 2026-04-14 (full re-sweep) |
| Host | Apple M5 Pro, 24 GB RAM |
| Docker Desktop VM | 3-container layout (one MSSQL, two PG) |
| MSSQL | SQL Server 2022 under Rosetta 2 emulation |
| PostgreSQL | 16 Alpine native arm64 |
| Dataset | StackOverflow2010 (Brent Ozar), **19,310,703 rows** across 9 tables |
| Go binary | `dmt v3.54.0-97-g4b174fa` (numbers from 2026-04-11 session) |
| dmt-rs binary | release build, commit `93357f7` (includes PRs #100, #101, #102) |
| Methodology | Warm cache; targets reset between runs; upsert runs use a pre-populated target (populated via dmt-rs `drop_recreate` in the preceding cell); indexes/FKs/checks disabled in all configs |

For `pg → mssql` and `mssql → pg` the source and target are on **different**
containers. For `pg → pg`, source (`pg-bench:5432`) and target (`pg-target:5434`)
are separate containers. For `mssql → mssql`, source (`StackOverflow2010`) and
target (`dmt_test_target`) are different databases on the same container.

## Results

End-to-end durations including finalization (PK creation; indexes/FKs/checks
disabled in all configs):

| # | Direction | Mode | Go | dmt-rs | Winner |
|---|---|---|---:|---:|---|
| 1 | mssql → pg | drop_recreate | 42s | **36.3s** | **dmt-rs 1.16×** |
| 2 | mssql → pg | upsert | 33s | 49.7s | Go 1.51× |
| 3 | pg → mssql | drop_recreate | 60s | 77.8s | Go 1.30× |
| 4 | pg → mssql | upsert | 120s | **109.7s** | **dmt-rs 1.09×** |
| 5 | pg → pg | drop_recreate | 29s | **21.3s** | **dmt-rs 1.36×** |
| 6 | pg → pg | upsert | 18s | 34.4s | Go 1.91× |
| 7 | mssql → mssql | drop_recreate | 85s | 104.1s | Go 1.22× |
| 8 | mssql → mssql | upsert | 142s | **92.2s** | **dmt-rs 1.54×** |

### Throughput view (rows/sec, end-to-end)

| Direction | Mode | Go | dmt-rs |
|---|---|---:|---:|
| mssql → pg | drop_recreate | 460k | **533k** |
| mssql → pg | upsert (pre-pop) | 585k | 388k |
| pg → mssql | drop_recreate | 322k | 248k |
| pg → mssql | upsert (pre-pop) | 161k | 176k |
| pg → pg | drop_recreate | 666k | **905k** |
| pg → pg | upsert (pre-pop) | 1.07M | 561k |
| mssql → mssql | drop_recreate | 227k | 186k |
| mssql → mssql | upsert (pre-pop) | 136k | **209k** |

## Patterns

1. **dmt-rs wins 4 cells**: `mssql → pg` drop_recreate (1.16×),
   `pg → pg` drop_recreate (1.36×), `pg → mssql` upsert (1.09×), and
   `mssql → mssql` upsert (1.54×). The common thread: binary COPY
   source reads and/or the MSSQL MERGE upsert path with per-chunk
   staging.

2. **Go wins 4 cells**: `mssql → pg` upsert (1.51×), `pg → mssql`
   drop_recreate (1.30×), `pg → pg` upsert (1.91×), and `mssql → mssql`
   drop_recreate (1.22×).

3. **MSSQL `drop_recreate` writes** (`pg → mssql`, `mssql → mssql`) are
   dmt-rs's loss zone. The forked tiberius batched-INSERT path is the
   ceiling. Confirmed across two source engines — this isn't a config
   artifact.

4. **PG upsert is 1.91× slower in dmt-rs** (down from 3.67× in the
   previous run). Go's `pg → pg` upsert is *faster* than its `pg → pg`
   `drop_recreate` (18s vs 29s) because pre-populated targets allow `IS
   DISTINCT FROM` change detection to skip unchanged rows. dmt-rs's
   upsert codepath does not exploit this yet.

## What this means

dmt-rs **wins 4 cells** and **loses 4** — an even split. The scorecard
improved from 1-1-6 (April 11 baseline) to 3-1-4 (after PRs #100–#102)
to **4-0-4** (this re-sweep on the same commit).

The improvement on this sweep vs the April 11 numbers is partly from the
#102 cancellation-token fix reducing overhead, and partly from a
different container layout (3 containers instead of 2, giving MSSQL more
breathing room).

Two known issues account for the remaining lost ground:

1. **tiberius batched INSERT throughput** — caps `drop_recreate`
   directions targeting MSSQL. Replacing or supplementing tiberius is
   tracked in `mssql-client-spike.md`.
2. **dmt-rs upsert codepath is slow on PG → PG.** The
   `INSERT … ON CONFLICT DO UPDATE` chunking does not benefit from the
   `IS DISTINCT FROM` skip-unchanged optimization that Go's upsert path
   uses. This is independent of issue #1 and worth a separate
   investigation.

## Historical: dmt-rs#97 failure mode (fixed by PRs #100 + #102)

Both upsert-to-MSSQL cells previously FAILED due to dmt-rs#97. Two PRs
addressed the issue:

- **PR #100** capped `parallel_writers` to 1 for MSSQL upsert — a
  correctness optimization since `MERGE WITH (TABLOCK)` serializes
  writers at the DB level anyway, so extra writers just waste pool
  connections.
- **PR #102** fixed the underlying orchestrator bug (#97): per-table
  `CancellationToken`s are now shared across partitions, so a writer
  failure in any partition cancels siblings immediately instead of
  cascading into pool exhaustion and data loss.

The original failure mode (before fixes):

```
ERROR  Bulk load data was expected but not sent. The batch will be terminated. code=4022
ERROR  dbo.Users: failed - Transfer failed for table Users:
       Writer 1 failed: Pool error: Timed out in bb8
```

- Sibling tables continued after the error — the orchestrator did not
  propagate the writer-pool failure.
- Final tally: `Migration failed: 9 tables, 1,263,563 / 19,310,703 rows in 78.3s`.

## Reproduction

The exact configs and sweep script live in `/tmp` and are cleaned up on
host reboot. The procedure is documented in `benchmark-playbook.md` §6;
this comparison uses the same configs but with a 3-container layout
(mssql-bench, pg-bench, pg-target).

To reproduce on a fresh host:

1. Build the Go binary at `/Users/johndauphine/repos/dmt`
   (`go build -o dmt ./cmd/dmt`).
2. Build the Rust binary (`cargo build --release`).
3. Stand up the 3-container Docker layout (one MSSQL, two PG).
4. Generate matching YAML pairs (Go and dmt-rs) for each of the 8 cells.
5. Reset target between every measured run; populate via `drop_recreate`
   for upsert measured runs.
6. All 8 cells should complete without intervention (dmt-rs#97 is fixed).

## Related docs

- [`benchmark-playbook.md`](benchmark-playbook.md) — full reproducible
  procedure for dmt-rs benchmarks (Go layered on top of the same setup)
- [`benchmark-results-m3-max.md`](benchmark-results-m3-max.md) —
  cross-hardware validation (dmt-rs only, no Go comparison)
- [`mssql-client-spike.md`](mssql-client-spike.md) — alternative MSSQL
  driver evaluation
- [dmt-rs#97](https://github.com/) — orchestrator deadlock / silent
  data-loss on writer failure (fixed by PRs #100 + #102)
- `../BENCHMARKS.md` — older Go vs Rust comparison numbers (January 2026,
  superseded by this doc)
