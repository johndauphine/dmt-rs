# Tuning the MySQL migration-target container

Stock `mysql:8.0` runs with defaults that assume spinning disks and a
128 MB InnoDB buffer pool. For a dmt-rs migration target that's brutal
— once the working set exceeds 128 MB, InnoDB thrashes and every
application-level perf knob becomes noise on top of I/O. This doc
describes the `my.cnf` dmt-rs ships for the bench target, what each
knob does, and how much faster the tuned container is in practice.

## The config

[`docker/mysql-target/my.cnf`](../docker/mysql-target/my.cnf). Mount
it into the container at `/etc/mysql/conf.d/tuned.cnf` and give the
container ≥6 GB memory (`--memory=6g`).

| Setting | Stock default | Tuned | Why |
|---|---|---|---|
| `innodb_buffer_pool_size` | 128 MB | **2 GB** | Single biggest knob. 128 MB against 19 M rows thrashes constantly; 2 GB comfortably holds the StackOverflow2010 working set. |
| `innodb_redo_log_capacity` | 96 MB (2 × 48 MB) | **512 MB** | Small redo logs force checkpoints every few seconds and stall writers. |
| `innodb_doublewrite` | ON | **OFF** | Skips torn-page protection. Safe for a migration target because you can re-run the migration after a crash. |
| `innodb_flush_log_at_trx_commit` | 1 | **2** | Fsync once per second instead of per commit. Matches `doublewrite=OFF`'s durability posture. |
| `innodb_io_capacity` / `_max` | 200 / 2000 | **5000 / 20000** | Defaults assume spinning disks. Modern NVMe does 50 K+ IOPS; these let InnoDB's flusher keep up. |
| `innodb_flush_method` | fsync | **O_DIRECT** | Skip OS page cache — InnoDB's buffer pool already caches. Avoids double-buffering memory cost. |
| `innodb_autoinc_lock_mode` | 1 | **2** | Interleaved auto-increment removes per-statement serialization. Safe unless you use statement-based replication. |
| `skip-log-bin` | off | **on** | Migration target doesn't need to replicate itself. Saves the per-row binlog write. |
| `local_infile` | off | **on** | Enables the `LOAD DATA LOCAL INFILE` path (optional, off by default in dmt-rs). |
| `max_allowed_packet` | 64 MB | 256 MB | Headroom for oversized `TEXT`/`BLOB` rows. |

**All of these trade durability or safety for speed and are
migration-target-only.** Do not apply this config to an application
database.

## Measured impact

Same bench harness as [`docs/mysql-baseline.md`](mysql-baseline.md) —
MSSQL `StackOverflow2010` → MySQL target, 19.3 M rows, drop_recreate,
n=3 per variant with warm-up + interleaved ordering.

### Stock vs tuned container (both A/Bs)

| container | config              | tuning-on rows/s | tuning-off rows/s |
|-----------|---------------------|-----------------:|------------------:|
| stock 3 GB / 128 MB pool | defaults       | 45,227 | 52,864 |
| stock 3 GB / 128 MB pool | full schema    | 49,099 | 44,786 |
| **tuned 6 GB / 2 GB pool** | defaults     | **120,668** | **118,639** |
| **tuned 6 GB / 2 GB pool** | full schema  | **117,967** | **117,175** |

The container jump is ~**2.5×** across every variant we measured —
far larger than any application-code change shipped or proposed so
far. The 14% anti-gain from `mysql_bulk_session_tuning=true` on stock
completely disappears on the tuned container (0.7 – 1.7 % delta,
distributions overlap), which is consistent with the hypothesis that
the stock-container "loss" was InnoDB's change-buffer bookkeeping
becoming visible when the buffer pool was starved. Give InnoDB enough
memory and the bookkeeping lands under the noise floor.

### Per-variant detail (tuned)

**Defaults (no indexes, no FKs):**

| run | config     | wall (s) | rows/s  |
|-----|------------|---------:|--------:|
| 1   | tuning-on  | 152.22   | 126,999 |
| 1   | tuning-off | 160.06   | 120,736 |
| 2   | tuning-off | 162.95   | 118,639 |
| 2   | tuning-on  | 160.15   | 120,668 |
| 3   | tuning-on  | 168.92   | 114,443 |
| 3   | tuning-off | 163.26   | 118,426 |

**Full schema (indexes + FKs):**

| run | config          | wall (s) | rows/s  |
|-----|-----------------|---------:|--------:|
| 1   | full-tuning-on  | 166.74   | 115,918 |
| 1   | full-tuning-off | 162.35   | 119,048 |
| 2   | full-tuning-off | 165.24   | 116,957 |
| 2   | full-tuning-on  | 163.83   | 117,967 |
| 3   | full-tuning-on  | 163.34   | 118,415 |
| 3   | full-tuning-off | 164.93   | 117,175 |

## Where the MySQL target still trails

Even tuned, MySQL-target throughput is behind MSSQL and Postgres on
this host:

| target  | mssql source, drop_recreate | relative |
|---------|----------------------------:|---------:|
| Postgres (COPY BINARY) | 445 K rows/s | 3.5× |
| MSSQL (tiberius BCP)   | 196 K rows/s | 1.5× |
| **MySQL (tuned)**      | **120 K rows/s** | 1× |

The structural reason is protocol: Postgres has `COPY ... FROM BINARY`
and MSSQL has BCP — both are binary bulk protocols. MySQL has no
public equivalent; dmt-rs currently uses multi-row INSERT with `?`
placeholders (capped at 65,535 placeholders per statement = ~3 K rows
per batch for a 20-column table). Note that mysql_async's `exec_drop`
already sends values in the **binary** protocol — only the SQL
template is text. We initially thought moving to
`Queryable::exec_batch` (server-side prepared, one exec per row) would
help, but that API does one round-trip per row, which on a 19 M-row
dataset would dwarf any CPU savings. So that lever is not on the
table via the current mysql_async surface.

## LOAD DATA LOCAL INFILE, re-evaluated on the tuned container

`docs/mysql-performance-tuning.md` had previously concluded that LOAD
DATA loses to multi-row INSERT at ≥2 workers because client-side TSV
generation is CPU-expensive. That finding was on stock `mysql:8.0`
where the InnoDB I/O path was the bottleneck; we wanted to re-check
on the tuned container where I/O is no longer the gate, in case the
TSV CPU cost was previously masked.

It wasn't. LOAD DATA is still slower on the tuned container:

| config (session tuning on, 4 workers)  | n | median wall (s) | median rows/s |
|----------------------------------------|---|----------------:|--------------:|
| `mysql_load_data: never` (INSERT)      | 3 | 162.63          | 118,874       |
| `mysql_load_data: always` (LOAD DATA)  | 3 | 184.06          | 105,020       |

Raw runs:

| run | config                 | wall (s) | rows/s  |
|-----|------------------------|---------:|--------:|
| 1   | load-data-off (INSERT) | 158.10   | 122,267 |
| 1   | load-data-on           | 177.89   | 108,623 |
| 2   | load-data-on           | 184.06   | 105,020 |
| 2   | load-data-off          | 162.63   | 118,874 |
| 3   | load-data-off          | 166.38   | 116,167 |
| 3   | load-data-on           | 187.35   | 103,157 |

Every INSERT run beat every LOAD DATA run — distributions don't
overlap. LOAD DATA is ~12 % slower on the tuned container.

The CPU cost of client-side TSV escape-handling (every `\t`, `\n`,
`\\`, `\0`, NULL sentinel per value per row — see `escape_tsv_value`
in `crates/dmt-rs/src/drivers/mysql/writer.rs:371`) still outweighs
any server-side bulk-path win with 4 concurrent writers. We're
leaving `mysql_load_data: never` as the default; the feature is
retained for single-worker configs and for users whose workload
profile differs.

Conclusion for MySQL target throughput on this host: we appear to be
near the practical ceiling of the INSERT path. A protocol dmt-rs
doesn't yet use would be required to go further, and a cheap one
isn't obviously on offer — see the `exec_batch` note above.

## Buffer pool sizing — why 2 GB is the sweet spot

The intuition after the first bench was that PK rebuild (~60 % of
each run's wall time) is RAM-bound, so bumping
`innodb_buffer_pool_size` above 2 GB should help. We tested it. It
doesn't.

| pool | container cap | tuning-on median | tuning-off median | outcome |
|------|---------------|-----------------:|------------------:|---------|
| **2 GB** (default) | 6 GB | **120,668 rows/s** | **118,639 rows/s** | stable |
| 3 GB               | 6 GB | 113,946 rows/s   | 113,407 rows/s   | stable, ~5 % regression |
| 4 GB               | 6 GB | —                | —                | container OOM-killed after warm-up |

Two things went wrong with the hypothesis:

1. **PK rebuild doesn't use the buffer pool for its sort.** `ADD
   PRIMARY KEY` merge-sorts via `innodb_sort_buffer_size` (default
   1 MB per sort thread). Growing the pool doesn't speed that phase.

2. **More pool means more dirty pages**, which means the
   `log_checkpointer` has to do more work to keep the redo log from
   running out of reusable space. At 3 GB we saw `[MY-014084]`
   "Threads are unable to reserve space in redo log which can't be
   reclaimed" warnings in `docker logs mysql-target`, even during
   successful runs. The checkpointer overhead grows faster than the
   cache-hit benefit for a write-heavy bulk-load workload.

3. **4 GB pool + 512 MB redo + mysqld overhead + per-connection
   buffers** pushed the container over its 6 GB cap. The OOM killer
   took mysqld out mid-bench. Raising the container cap to 8 GB
   would work but eats into the 12 GB Docker VM budget that
   `mssql-bench` (source) also needs.

Takeaway: on this dataset, at this container size, **don't grow the
pool**. If a user has a larger VM and a larger container cap (say a
16 GB VM with an 10 GB container), the headroom calculus might
change — but within the 12 GB / 6 GB envelope documented here,
2 GB is genuinely the sweet spot.

## Docker VM sizing — 12 GB is fine, 16 GB buys cleaner benches only

Tested whether bumping the Docker Desktop VM from 12 GB to 16 GB
helps MSSQL → MySQL drop_recreate throughput. Same tuned my.cnf,
same 6 GB container cap, same bench script (`bench-mysql-tuning.sh`,
n=3 per variant, interleaved, warm-up discarded).

| VM   | tuning-on median | tuning-off median | tuning-on range      |
|------|-----------------:|------------------:|----------------------|
| 12 GB | 120,668 rows/s  | 118,639 rows/s    | 114 K – 127 K (Δ 12 K) |
| 16 GB | 124,941 rows/s  | 123,852 rows/s    | 124 K – 127 K (Δ 3 K)  |

**Throughput shift: +3.5 % on, +4.4 % off.** Below the 10 % threshold
we set as "meaningful" before the test. Consistent with the existing
project memory that says 16 GiB adds nothing for the MSSQL-side
bench — the conclusion carries over to MSSQL → MySQL.

**Variance shift: ~4× tighter on 16 GB** (Δ 3 K vs Δ 12 K in the
tuning-on spread). The 12 GB VM was close enough to memory pressure
that individual runs drifted more. 16 GB gives enough headroom that
run-to-run noise drops sharply.

Takeaway: if you're just running migrations, **stay at 12 GB**. If
you're running A/B benches and want tighter, more reproducible
numbers, bumping to 16 GB is worth it even though the medians don't
change much.

## MSSQL source-side RAM — M3 Max 36 GB, 10 GiB `max server memory`

Re-ran both A/Bs on an M3 Max 36 GB host with a 24 GB Docker VM,
`mssql-bench` raised to a 12 GB cgroup / 10 240 MB `max server memory`,
and the MySQL target unchanged (6 GB cgroup / 2 GB buffer pool). This
isolates one variable: **how much of the StackOverflow2010 working set
fits in the MSSQL source buffer pool.** Full procedure and per-run
numbers in [`m3-max-mssql-ram-experiment.md`](m3-max-mssql-ram-experiment.md).

| Bench | config | M5 Pro 24 GB (4 GiB MSSQL RAM) | M3 Max 36 GB (10 GiB MSSQL RAM) | Δ |
|---|---|---:|---:|---:|
| baseline | tuning-on | 120,668 rows/s | 165,509 rows/s | **+37 %** |
| baseline | tuning-off | 118,639 rows/s | 167,499 rows/s | **+41 %** |
| full-schema | full-tuning-on | 117,967 rows/s | 160,768 rows/s | **+36 %** |
| full-schema | full-tuning-off | 117,175 rows/s | 160,729 rows/s | **+37 %** |

The ~120 K rows/s M5 Pro ceiling is **not** a MySQL protocol or CPU
ceiling — it's an MSSQL source buffer-pool ceiling that only reveals
itself once you have enough host RAM to lift it. Every other lever
we've tested on this doc (bigger MySQL buffer pool, LOAD DATA, 16 GB
Docker VM, `mysql_bulk_session_tuning`) moves throughput by ≤5 % or
regresses. Source-side RAM moves it by +36-41 %.

Secondary observation: `mysql_bulk_session_tuning` is effectively noise
at 10 GiB MSSQL RAM (0.1 % delta in both benches). The source-side
uplift dominates any target-side session-tuning win.

Practical recommendation: if the host has ≥ 32 GB RAM, skip the
`mssql-bench` at 4 GiB cap and go straight to 10 GiB / 12 GB cgroup.
The tuned MySQL container caps (6 GB cgroup / 2 GB pool) do not
change — MySQL's sweet spot is independent of host RAM.

Reproducer for the pool-size sweep:

```bash
# edit docker/mysql-target/my.cnf to the target pool size, then:
docker rm -f mysql-target
docker run -d --name mysql-target --memory=6g --memory-swap=6g \
  -p 3307:3306 -e MYSQL_ROOT_PASSWORD=TestPass2024 \
  -v "$PWD/docker/mysql-target/my.cnf:/etc/mysql/conf.d/tuned.cnf:ro" \
  mysql:8.0
LOG_DIR=.bench-logs-pool-XG ./scripts/bench-mysql-tuning.sh
```

## Reproducing

```bash
# Recreate the target with the tuned config
docker stop mssql-target                          # keep 2-container budget
docker rm -f mysql-target
docker run -d \
  --name mysql-target \
  --memory=6g --memory-swap=6g \
  -p 3307:3306 \
  -e MYSQL_ROOT_PASSWORD=TestPass2024 \
  -v "$PWD/docker/mysql-target/my.cnf:/etc/mysql/conf.d/tuned.cnf:ro" \
  mysql:8.0

# Build dmt-rs + run the A/Bs
cargo build --release --features mysql
LOG_DIR=.bench-logs-tuned-baseline    ./scripts/bench-mysql-tuning.sh
LOG_DIR=.bench-logs-tuned-full-schema ./scripts/bench-mysql-full-schema.sh
LOG_DIR=.bench-logs-load-data         ./scripts/bench-mysql-load-data.sh
```
