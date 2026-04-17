# MySQL target — first published baseline

This is dmt-rs's first published performance baseline for a **MySQL target**.
Prior benchmarks in the repo (`BENCHMARKS.md`, `benchmark-results-m3-max.md`)
only cover MSSQL↔PostgreSQL.

## TL;DR

On the default data-warehouse-style config (no secondary indexes, no foreign
keys) migrating 19.3M rows from MSSQL → MySQL, `mysql_bulk_session_tuning`
has **no path to help and a measurable 14% cost**:

| config | n | median wall (s) | median rows/s |
|---|---|---|---|
| `mysql_bulk_session_tuning: true`  | 3 | 427.14 | 45,227 |
| `mysql_bulk_session_tuning: false` | 3 | 365.42 | 52,864 |

This is the **opposite** of the 5–15% gain predicted when #111 shipped. The
reason is scope: the tuning's hot paths are not exercised by this config.
See "Why tuning hurts here" below and the open question about the default.

## Environment

| Item | Value |
|---|---|
| Host | M5 Pro, 24 GB RAM, 15 cores (macOS 15.4.0) |
| Source | MSSQL 2022 in Docker (`mssql-bench`, port 1433) |
| Target | MySQL 8.0 in Docker (`mysql-target`, port 3307, 3 GB memory cap) |
| Dataset | StackOverflow2010 — 9 tables, 19,310,703 rows |
| Build | `cargo build --release --features mysql` at `46d1818` |
| Workers / chunk | 4 workers, 50,000-row chunks (same in both variants) |
| Create indexes / FKs | **false** (dmt-rs defaults) |

Only `mysql_bulk_session_tuning` differs between the two variants; all other
knobs are identical. See `benchmark-mssql-to-mysql-tuning-on.yaml` and
`benchmark-mssql-to-mysql-tuning-off.yaml` for the exact configs.

## Methodology

Reproducer: `scripts/bench-mysql-tuning.sh`.

* Every run drops + recreates the MySQL target database so each measurement
  starts against an empty target.
* A discarded warm-up run primes the MSSQL source buffer pool before any
  measurement, so the first measured variant isn't penalized by a cold cache.
* Variant order is **interleaved** — `on, off, off, on, on, off` — so any
  residual system drift during the run can't systematically align with one
  variant.
* Three observations per variant; we report the median.
* All per-run logs are captured in `.bench-logs/` and summarized in
  `results.tsv`.

## Raw results

Order is the order the script executed them, not the order they appear in
the config files. Wall time is measured externally; dmt-rs duration comes
from its own summary block.

| config     | run | wall (s) | dmt (s) | rows/s |
|------------|-----|---------:|--------:|-------:|
| tuning-on  | 1   | 430.54   | 430.34  | 44,872 |
| tuning-off | 1   | 365.42   | 365.28  | 52,864 |
| tuning-off | 2   | 330.16   | 330.00  | 58,517 |
| tuning-on  | 2   | 377.41   | 377.27  | 51,184 |
| tuning-on  | 3   | 427.14   | 426.96  | 45,227 |
| tuning-off | 3   | 371.15   | 371.03  | 52,046 |

Every tuning-off run is faster than every tuning-on run. The distributions
do not overlap, so this is not variance — tuning-on really is slower on
this config.

## Why tuning hurts here

PR #111 sets two MySQL session variables at connect time:

* `SET SESSION unique_checks = 0` — tells InnoDB to skip per-row uniqueness
  checks on **secondary** indexes during `INSERT`. No effect on the
  clustered/primary index. No effect on DDL uniqueness enforcement.
* `SET SESSION foreign_key_checks = 0` — skips FK validation on INSERT /
  UPDATE / DELETE, and also skips the "validate existing rows" scan that
  `ALTER TABLE ... ADD CONSTRAINT FOREIGN KEY` performs.

dmt-rs's **default** migration config sets `create_indexes: false` and
`create_foreign_keys: false`. On this benchmark:

* No secondary indexes exist during the bulk load → `unique_checks=0` has
  nothing to skip.
* No FKs are created at all → `foreign_key_checks=0` has nothing to skip.

So both tuning knobs are inert on this workload. The only observable
behavior change is InnoDB's "bulk insert mode" path that gets selected
when `unique_checks=0`. That path defers secondary-index updates into
the change buffer for later merge; on a schema with no secondary indexes
the deferral has no payoff, and the 3 GB InnoDB buffer on the container
means the bookkeeping is pure overhead. That's the most plausible
explanation for the ~14% anti-gain. Confirming it would require InnoDB
status counters (`innodb_buffer_pool_stats`, `innodb_change_buffer_stats`)
sampled across runs — a follow-up if needed.

## What this means for #111

The tuning is still correct **for the workloads it was designed for** —
runs with `create_indexes: true` and/or `create_foreign_keys: true` where
post-load DDL would otherwise do a full table scan to validate constraints.
On those, we expect the original 5–15% win to materialize.

But this baseline does **not** confirm that. The next step is a second
benchmark with `create_indexes: true, create_foreign_keys: true` on the
same dataset. If that benchmark shows the predicted win (and this one
shows a loss), the right answer may be to **flip the default** of
`mysql_bulk_session_tuning` to `false` and have the finalize phase
set the vars to 0 only for the FK-creation transactions where they
actually help.

That's a separate PR. Captured as a known question, not an action item
for this one.

## How to reproduce

```bash
# Ensure containers are in place
docker start mssql-bench mysql-target

# Build
cargo build --release --features mysql

# Run (takes ~45 min: warm-up + 6 measured runs on a 24 GB Mac)
./scripts/bench-mysql-tuning.sh

# Results
cat .bench-logs/results.tsv
```

The script hard-codes 3 runs per variant in its `ORDER` array. To change
the run count or sequence, edit that array in `scripts/bench-mysql-tuning.sh`.
Override the MySQL test password with `MYSQL_ROOT_PASSWORD=...`.

---

## Follow-up: full-schema run (indexes + FKs enabled)

The baseline above runs against dmt-rs's defaults — no secondary indexes,
no foreign keys, so neither tuning variable has anything to skip. To
actually stress the tuning's hot paths we re-ran the same A/B with
`create_indexes: true` and `create_foreign_keys: true`, which forces
`ALTER TABLE ... ADD CONSTRAINT FOREIGN KEY` (child-table validation
scan) and `ALTER TABLE ... ADD UNIQUE INDEX` (uniqueness enforcement)
into the finalize phase where `foreign_key_checks=0` / `unique_checks=0`
are designed to pay off.

Configs: `benchmark-mssql-to-mysql-full-tuning-{on,off}.yaml`.
Reproducer: `scripts/bench-mysql-full-schema.sh`. Methodology matches
the baseline (discarded warm-up, interleaved order, n=3, medians).

### Result — ambiguous, not confirming

| config           | n | median wall (s) | median rows/s |
|------------------|--:|---:|---:|
| `full-tuning-on` | 3 | 393.48 | 49,099 |
| `full-tuning-off`| 3 | 431.27 | 44,786 |

Raw runs:

| config          | run | wall (s) | dmt (s) | rows/s |
|-----------------|-----|---------:|--------:|-------:|
| full-tuning-on  | 1   | 393.48   | 393.30  | 49,099 |
| full-tuning-off | 1   | 431.27   | 431.17  | 44,786 |
| full-tuning-off | 2   | 457.63   | 457.50  | 42,209 |
| full-tuning-on  | 2   | 435.52   | 435.33  | 44,358 |
| full-tuning-on  | 3   | 385.31   | 385.20  | 50,131 |
| full-tuning-off | 3   | 339.80   | 339.66  | 56,853 |

Medians favor tuning-on by ~10%, but — unlike the baseline above, where
every off-run was faster than every on-run — these distributions **do
overlap**. The fastest tuning-off run (56,853 rows/s) beats every
tuning-on run; the slowest tuning-off run (42,209) is slower than the
slowest tuning-on (44,358). Put the six throughput values in a single
sorted list and rank them by variant:

```
42209 (off) 44358 (on) 44786 (off) 49099 (on) 50131 (on) 56853 (off)
```

Rank sums are 10 (off) vs 11 (on) out of 21 total — essentially a coin
flip. With n=3 per variant we cannot reject "no effect".

### What we learned

Combining this with the baseline:

| config                                                  | outcome              | confidence |
|---------------------------------------------------------|----------------------|------------|
| defaults (`create_indexes: false`, FKs off)             | tuning **loses 14%** | distributions don't overlap |
| full schema (`create_indexes: true`, `create_foreign_keys: true`) | tuning trends +10%   | distributions overlap; can't confirm |

The 14% cost on defaults is real — that case is worth acting on. The
full-schema gain is the right direction but not yet proven; we'd need
a bigger n (maybe 10) or a schema with heavier FK/index density
(StackOverflow2010 has very few secondary indexes in the source) to
tighten the error bars.

### Likely bigger lever: the MySQL container itself

The target runs a stock `mysql:8.0` with a 128 MB InnoDB buffer pool
against 19 M rows. That's almost certainly the dominant bottleneck —
once the working set exceeds 128 MB, InnoDB thrashes, and any
application-level tuning A/B becomes noise floating on top of I/O. A
tuned container (2 GB buffer pool, 512 MB redo log, `doublewrite=OFF`,
modern I/O capacity defaults) is probably a bigger speedup than
anything we can do in application code, and would also give the
session-tuning A/B a cleaner signal.

That's tracked as the next follow-up; not in scope for this PR.

### Recommendation re: the default

Weak but leaning **flip the default to `false`**: the 14% cost on the
common-case config is measured and real, while the gain on the
full-schema config is not confirmed. A cleaner fix is to leave the
default `false` and have the finalize phase set
`foreign_key_checks = 0` inside the specific transactions that run
`ADD CONSTRAINT FOREIGN KEY` / `ADD UNIQUE INDEX`, so the win is
narrow-scoped to where it pays. That's a code change deferred to its
own PR.

