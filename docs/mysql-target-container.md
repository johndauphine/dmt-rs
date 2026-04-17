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
near the practical ceiling of the INSERT path. Further gains likely
require either more buffer-pool RAM (to speed PK rebuild, which is
~60 % of each run's time) or a protocol dmt-rs doesn't yet use.

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
