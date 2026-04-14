# Go dmt vs dmt-rs — head-to-head benchmark

Side-by-side comparison of [`dmt`](https://github.com/) (Go, v3.54.0-97-g4b174fa)
and `dmt-rs` (this repo, commit `86e935f`) running the same SO2010 dataset
through every cell of the 2 × 2 × 2 matrix
**(direction × mode × engine)**.

This doc replaces the stale numbers in `../BENCHMARKS.md` (January 2026).

## Test environment

| Setting | Value |
|---|---|
| Date | 2026-04-11 (cells 1-8), 2026-04-14 (cells 4, 8 rerun) |
| Host | Apple M5 Pro, 24 GB RAM |
| Docker Desktop VM | 8 GiB, 2-container layout (one MSSQL + one PG) |
| MSSQL | SQL Server 2022 under Rosetta 2 emulation, `max server memory = 4096 MB` |
| PostgreSQL | 16 Alpine native arm64, aggressive write tuning baked into image |
| Dataset | StackOverflow2010 (Brent Ozar), **19,310,703 rows** across 9 tables |
| Go binary | `dmt v3.54.0-97-g4b174fa` |
| dmt-rs binary | release build, commit `86e935f` (cells 1-8); commit `78ceb0f` (cells 4, 8 rerun — includes PR #100 bb8 pool fix and PR #101 oversized string fix) |
| Methodology | Single-instance source/target on same container per engine; warm cache; targets reset between runs; upsert runs use a populated target (populated via Go in the discarded preload step) |

For the `pg → mssql` and `mssql → pg` cells the source and target are on
**different** containers (the only setup possible). For the `pg → pg` and
`mssql → mssql` cells, source and target are different *databases* on the
same container. This is the cleanest budget-respecting layout for an 8 GiB
Docker VM and matches §4 of the benchmark playbook.

## Results

End-to-end durations including finalization (PK creation; indexes/FKs/checks
disabled in all configs):

| # | Direction | Mode | Go | dmt-rs | Winner |
|---|---|---|---:|---:|---|
| 1 | mssql → pg | drop_recreate | 42s | 44.7s | ~tie |
| 2 | mssql → pg | upsert | 33s | 47.3s | Go 1.43× |
| 3 | pg → mssql | drop_recreate | 60s | 132.2s | Go 2.20× |
| 4 | pg → mssql | upsert | 120s | **108.2s** | **dmt-rs 1.11×** |
| 5 | pg → pg | drop_recreate | 29s | **20s** | **dmt-rs 1.45×** |
| 6 | pg → pg | upsert | 18s | 66s | Go 3.67× |
| 7 | mssql → mssql | drop_recreate | 85s | 112s | Go 1.32× |
| 8 | mssql → mssql | upsert | 142s | **104.4s** | **dmt-rs 1.36×** |

Cells 1–3, 5–7 were measured on 2026-04-11. Cells 4 and 8 were rerun on
2026-04-14 after PR #100 (cap `parallel_writers` to 1 for MSSQL upsert)
and PR #101 (oversized string detection in upsert staging) landed — both
previously FAILED due to dmt-rs#97.

### Throughput view (rows/sec, end-to-end)

| Direction | Mode | Go | dmt-rs |
|---|---|---:|---:|
| mssql → pg | drop_recreate | 460k | 432k |
| mssql → pg | upsert (pre-pop) | 585k | 408k |
| pg → mssql | drop_recreate | 322k | 146k |
| pg → mssql | upsert (pre-pop) | 161k | 178k |
| pg → pg | drop_recreate | 666k | **966k** |
| pg → pg | upsert (pre-pop) | 1.07M | 293k |
| mssql → mssql | drop_recreate | 227k | 173k |
| mssql → mssql | upsert (pre-pop) | 136k | 185k |

## dmt-rs Phase F failure mode (historical — fixed by PRs #100 + #102)

> **Update 2026-04-14:** Both upsert-to-MSSQL cells now pass. Two PRs
> addressed the issue:
>
> - **PR #100** capped `parallel_writers` to 1 for MSSQL upsert — a
>   correctness optimization since `MERGE WITH (TABLOCK)` serializes
>   writers at the DB level anyway, so extra writers just waste pool
>   connections.
> - **PR #102** fixed the underlying orchestrator bug (#97): per-table
>   `CancellationToken`s are now shared across partitions, so a writer
>   failure in any partition cancels siblings immediately instead of
>   cascading into pool exhaustion and data loss.

The original `mssql → mssql` upsert run (pre-fix) failed in 78s with `rc=2`:

```
ERROR  Bulk load data was expected but not sent. The batch will be terminated. code=4022
ERROR  dbo.Users: failed - Transfer failed for table Users:
       Writer 1 failed: Pool error: Timed out in bb8
```

What was notable:

- The error appeared on `dbo.Users` first.
- **Other tables continued and reported success** (Comments, Votes both
  finished after the error). The orchestrator did not propagate the
  writer-pool failure to cancel sibling table transfers.
- Final tally: `Migration failed: 9 tables, 1,263,563 / 19,310,703 rows in 78.3s`.
- The process exited cleanly with `rc=2` instead of hanging.

The hang vs. clean-exit asymmetry suggested `#97` has multiple branches
depending on which writer fails first and which sibling tables are still
alive. Filed as additional context on dmt-rs#97.

## Patterns

1. **dmt-rs wins three cells**: `pg → pg` `drop_recreate` (1.45×),
   `pg → mssql` upsert (1.11×), and `mssql → mssql` upsert (1.36×).
   The upsert wins are new as of 2026-04-14 after PR #100 capped
   `parallel_writers` to 1 for MSSQL upsert — a correctness
   optimization since `MERGE WITH (TABLOCK)` serializes writers at
   the DB level anyway.

2. **dmt-rs ties on `mssql → pg` `drop_recreate`**. Same reason: binary
   COPY into the target carries the workload, source overhead is minor.

3. **MSSQL `drop_recreate` writes** (`pg → mssql`, `mssql → mssql`) are
   dmt-rs's loss zone. The forked tiberius batched-INSERT path is the
   ceiling. Confirmed across two source engines — this isn't a config
   artifact.

4. **`pg → pg` upsert is 3.67× slower in dmt-rs** even with MSSQL out of
   the picture. Go's `pg → pg` upsert is *faster* than its `pg → pg`
   `drop_recreate` (18s vs 29s) because pre-populated targets allow `IS
   DISTINCT FROM` change detection to skip unchanged rows. dmt-rs's
   upsert codepath does not exploit this.
   **Still worth investigating** — the MSSQL bottleneck is not the
   only thing capping dmt-rs's upsert performance; the
   `INSERT … ON CONFLICT DO UPDATE` chunking codepath itself is slow
   independently of the tiberius bottleneck.

## Per-table detail (dmt-rs only — Go binary doesn't log per-table timings to stderr)

`pg → pg` `drop_recreate` (the dmt-rs win):

| Table | Rows | Duration | Throughput |
|---|---:|---:|---:|
| public.votes | 10,143,364 | 5.0s | 2.02M rows/s |
| public.comments | 3,875,183 | 5.1s | 760k rows/s |
| public.posts | 3,729,195 | 15.4s | 242k rows/s |

(End-to-end including finalization: 19.4s for 19.3M rows = **993k rows/s**.)

`mssql → mssql` `drop_recreate`:

| Table | Rows | Duration | Throughput |
|---|---:|---:|---:|
| dbo.Posts | 3,729,195 (3 partitions) | up to 94.5s | 13–15k rows/s |
| dbo.Votes | 10,143,364 (3 partitions) | up to 32.6s | 104–129k rows/s |
| dbo.Comments | 3,875,183 (3 partitions) | up to 12.6s | 102–107k rows/s |

(End-to-end: 111.8s for 19.3M rows = **173k rows/s**.) dbo.Posts is the
choke point here — the LOB-heavy table dominates wall time.

`pg → pg` upsert (pre-populated, the surprise loss):

| Table | Rows | Duration | Throughput |
|---|---:|---:|---:|
| public.votes | 10,143,364 | 60.1s | 169k rows/s |
| public.posts | 3,729,195 | 66.1s | 56k rows/s |
| public.comments | 3,875,183 | 43.4s | 89k rows/s |

(End-to-end: 66.3s for 19.3M rows = **291k rows/s**, vs Go at ~1.07M rows/s.)
The per-table throughputs above are 7×–11× lower than the same tables in
`drop_recreate` mode. The upsert codepath is the bottleneck, not the
underlying `COPY` plumbing.

## What this means

dmt-rs **wins 3 cells** (pg→pg drop_recreate, pg→mssql upsert,
mssql→mssql upsert), **ties 1** (mssql→pg drop_recreate), and **loses
4** (the remaining cells). The scorecard improved from 1-1-6 to 3-1-4
after PR #100 unblocked the two upsert-to-MSSQL cells.

Two known issues account for the remaining lost ground:

1. **tiberius batched INSERT throughput** — caps `drop_recreate`
   directions targeting MSSQL at roughly half of Go. Replacing or
   supplementing tiberius is tracked in `mssql-client-spike.md`.
2. **dmt-rs upsert codepath is slow on PG → PG.** The
   `INSERT … ON CONFLICT DO UPDATE` chunking does not benefit from the
   `IS DISTINCT FROM` skip-unchanged optimization that Go's upsert path
   uses. This is independent of issue #1 and worth a separate
   investigation.

One issue has been **fully fixed**:

3. **dmt-rs#97 — orchestrator deadlock / data-loss on writer failure**
   — The orchestrator now shares per-table `CancellationToken`s across
   partitions, so a writer failure in any partition immediately cancels
   sibling partitions' dispatchers. Previously, sibling partitions
   continued running unaware, cascading into pool exhaustion and data
   loss. The `parallel_writers` cap to 1 for MSSQL upsert (PR #100) is
   retained as a correctness optimization (TABLOCK serializes at the DB
   level), not a workaround.

## Reproduction

The exact configs and sweep script live in `/tmp` and are cleaned up on
host reboot. The procedure is documented in `benchmark-playbook.md` §6;
this comparison uses the same configs but with a single-instance pg-pg
and single-instance mssql-mssql target layout, plus parallel YAMLs for
the Go binary that match the dmt-rs configs key-for-key.

To reproduce on a fresh host:

1. Build the Go binary at `/Users/johndauphine/repos/dmt`
   (`go build -o dmt ./cmd/dmt`).
2. Build the Rust binary (`cargo build --release`).
3. Stand up the 2-container Docker layout from playbook §4 (one MSSQL,
   one PG).
4. Generate matching YAML pairs (Go and dmt-rs) for each of the 8 cells.
5. Reset target between every measured run; populate via Go for upsert
   measured runs to avoid dmt-rs state contamination.
6. All 8 cells should complete without intervention (dmt-rs#97 is fixed).

## Related docs

- [`benchmark-playbook.md`](benchmark-playbook.md) — full reproducible
  procedure for dmt-rs benchmarks (Go layered on top of the same setup)
- [`benchmark-results-m3-max.md`](benchmark-results-m3-max.md) —
  cross-hardware validation (dmt-rs only, no Go comparison)
- [`mssql-client-spike.md`](mssql-client-spike.md) — alternative MSSQL
  driver evaluation
- [dmt-rs#97](https://github.com/) — orchestrator deadlock / silent
  data-loss on writer failure (the blocker for upsert-to-MSSQL)
- `../BENCHMARKS.md` — older Go vs Rust comparison numbers (January 2026,
  superseded by this doc)
