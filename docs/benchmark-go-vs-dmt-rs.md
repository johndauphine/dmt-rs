# Go dmt vs dmt-rs — head-to-head benchmark

Side-by-side comparison of [`dmt`](https://github.com/) (Go, v3.54.0)
and `dmt-rs` (this repo) running the same SO2010 dataset through every cell
of the 2 × 2 × 2 matrix **(direction × mode × engine)**.

This doc replaces the stale numbers in `../BENCHMARKS.md` (January 2026).

## Test environment

| Setting | Value |
|---|---|
| Date | 2026-04-14 |
| Host | Apple M5 Pro, 24 GB RAM |
| Docker Desktop VM | 3-container layout (mssql-bench, pg-bench, pg-target) |
| MSSQL | SQL Server 2022 under Rosetta 2 emulation (mssql-bench:1433) |
| PostgreSQL | 16 Alpine native arm64 (pg-bench:5432, pg-target:5434) |
| Dataset | StackOverflow2010 (Brent Ozar), **19,310,703 rows** across 9 tables |
| Go binary | `dmt v3.54.0` |
| dmt-rs binary | release build, commit `93357f7` (includes PRs #100, #101, #102) |
| Methodology | Both binaries run back-to-back on the same session with warm caches; targets reset between runs; upsert runs use a pre-populated target (populated via the preceding `drop_recreate` cell); indexes/FKs/checks disabled in all configs |

For `pg → mssql` and `mssql → pg` the source and target are on **different**
containers. For `pg → pg`, source (`pg-bench:5432`) and target (`pg-target:5434`)
are separate containers. For `mssql → mssql`, source (`StackOverflow2010`) and
target (`dmt_test_target`) are different databases on the same container.

## Results

End-to-end durations including finalization (PK creation; indexes/FKs/checks
disabled in all configs):

| # | Direction | Mode | Go | dmt-rs | Winner |
|---|---|---|---:|---:|---|
| 1 | mssql → pg | drop_recreate | **28s** | 36.3s | Go 1.30× |
| 2 | mssql → pg | upsert | **22s** | 49.7s | Go 2.26× |
| 3 | pg → mssql | drop_recreate | **46s** | 77.8s | Go 1.69× |
| 4 | pg → mssql | upsert | **85s** | 109.7s | Go 1.29× |
| 5 | pg → pg | drop_recreate | 29s | **21.3s** | **dmt-rs 1.36×** |
| 6 | pg → pg | upsert | **24s** | 34.4s | Go 1.43× |
| 7 | mssql → mssql | drop_recreate | **60s** | 104.1s | Go 1.73× |
| 8 | mssql → mssql | upsert | 95s | **92.2s** | **~tie (dmt-rs 1.03×)** |

### Throughput view (rows/sec, end-to-end)

| Direction | Mode | Go | dmt-rs |
|---|---|---:|---:|
| mssql → pg | drop_recreate | **678k** | 533k |
| mssql → pg | upsert (pre-pop) | **872k** | 388k |
| pg → mssql | drop_recreate | **419k** | 248k |
| pg → mssql | upsert (pre-pop) | **228k** | 176k |
| pg → pg | drop_recreate | 656k | **905k** |
| pg → pg | upsert (pre-pop) | **815k** | 561k |
| mssql → mssql | drop_recreate | **324k** | 186k |
| mssql → mssql | upsert (pre-pop) | 204k | **209k** |

## Patterns

1. **dmt-rs wins 1 cell clearly**: `pg → pg` drop_recreate (1.36×).
   Pure binary COPY on both ends, no MSSQL involvement. This is the
   configuration where dmt-rs's architecture shines.

2. **dmt-rs ties on `mssql → mssql` upsert** (92.2s vs 95s). The MERGE
   WITH (TABLOCK) per-chunk staging path is competitive with Go's
   approach.

3. **Go wins 6 cells**, with the largest gaps on MSSQL-target directions.
   The forked tiberius batched-INSERT path is the bottleneck for
   `drop_recreate` (cells 3, 7), and Go's upsert implementation is
   significantly faster across all directions (cells 2, 4, 6).

4. **Every upsert cell is a Go win or tie.** Go's upsert path benefits
   from `IS DISTINCT FROM` skip-unchanged optimization on PG targets,
   and a more efficient MERGE implementation on MSSQL targets. dmt-rs's
   `INSERT … ON CONFLICT DO UPDATE` chunking does not exploit
   skip-unchanged yet.

5. **The MSSQL write bottleneck is larger than previously measured.**
   On the old 2-container/8 GiB layout, MSSQL was memory-starved,
   making both Go and dmt-rs slower. The 3-container layout gives MSSQL
   more headroom, which benefits Go more than dmt-rs — suggesting Go's
   driver utilizes MSSQL's buffer pool more efficiently.

## What this means

dmt-rs **wins 1 cell**, **ties 1**, and **loses 6**. The only clear win
is `pg → pg` drop_recreate, where binary COPY on both ends bypasses
all the MSSQL bottlenecks.

Three issues account for the lost ground:

1. **tiberius batched INSERT throughput** — caps `drop_recreate`
   directions targeting MSSQL at roughly half of Go. The gap is 1.69×
   on pg→mssql and 1.73× on mssql→mssql. Replacing or supplementing
   tiberius is tracked in `mssql-client-spike.md`.

2. **dmt-rs upsert codepath is slow on PG targets.** The
   `INSERT … ON CONFLICT DO UPDATE` chunking does not benefit from the
   `IS DISTINCT FROM` skip-unchanged optimization that Go's upsert path
   uses. This costs 1.43× on pg→pg and 2.26× on mssql→pg.

3. **MSSQL source reads are slower in dmt-rs.** Even on mssql→pg
   drop_recreate (where the PG target is fast), Go is 1.30× faster.
   This suggests tiberius source-side query streaming is also slower
   than Go's MSSQL driver.

## Note on previous numbers

Earlier versions of this doc reported a 4-0-4 scorecard based on
comparing Go numbers from a 2-container/8 GiB layout (April 11) against
dmt-rs numbers from a 3-container layout (April 14). That comparison was
misleading — the infrastructure change benefited both binaries, but Go
improved more. This doc now uses same-session, same-infrastructure
numbers for a fair comparison.

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

## Reproduction

The exact configs live in `/tmp/go-*.yaml` and `/tmp/dmt-rs-*.yaml` and
are cleaned up on host reboot. The procedure is documented in
`benchmark-playbook.md` §6.

To reproduce on a fresh host:

1. Build the Go binary at `/Users/johndauphine/repos/dmt`
   (`go build -o dmt ./cmd/dmt`).
2. Build the Rust binary (`cargo build --release`).
3. Stand up the 3-container Docker layout (one MSSQL, two PG).
4. Run dmt-rs sweep, then Go sweep (or vice versa) in the same session.
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
