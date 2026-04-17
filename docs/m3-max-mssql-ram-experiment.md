# M3 Max experiment — does MSSQL-side RAM unlock MSSQL → MySQL throughput?

A targeted bench playbook for the M3 Max 36 GB machine. Everything
else — code, MySQL tuning, bench methodology — stays identical to
what's shipped on `main`. **Only the MSSQL source side gets more
RAM.** The goal is a clean answer to "does giving MSSQL more of the
StackOverflow2010 dataset in its buffer pool move MSSQL → MySQL
throughput?"

## Context

Published numbers (M5 Pro 24 GB host, 12 GB Docker VM, tuned MySQL
container per `docs/mysql-target-container.md`):

| config                | median wall (s) | median rows/s |
|-----------------------|---:|---:|
| tuning-on             | 160.15 | 120,668 |
| tuning-off            | 162.95 | 118,639 |

Every MySQL-side lever we tried (binary prepared INSERT, LOAD DATA,
bigger buffer pool, bigger VM on the same machine) came back as
noise or a regression — the MySQL target appears to be at a
protocol/CPU ceiling at ~120 K rows/s on this host.

One lever we **haven't** stressed is MSSQL source RAM. `mssql-bench`
is capped at 4096 MB of `max server memory` on the M5 Pro and never
got more, so we don't know whether source-side buffer-pool pressure
has been quietly costing us throughput.

The M3 Max 36 GB host (see `docs/benchmark-results-m3-max.md`) shows
MSSQL → MSSQL run **2.7× faster** than on M5 Pro (530 K vs 196 K
rows/s), which hints that MSSQL benefits from more RAM / bandwidth
in ways the M5 Pro can't expose. If that carries over to the
MSSQL → MySQL path, we'd expect a meaningful uplift here too.

## Hypothesis

Giving MSSQL a 10 GB `max server memory` (so the entire
StackOverflow2010 working set fits in its buffer pool) in a
correspondingly bigger Docker VM will raise MSSQL → MySQL median
throughput **≥ 15 %** over the M5 Pro baseline. Below 15 % is
"not a lever." Above 30 % would be dramatic.

The MySQL target side stays exactly the same, because we've
already exhausted its tuning space on the M5 Pro.

## What you need

- M3 Max (or any Apple Silicon) with ≥ 32 GB RAM.
- Docker Desktop.
- `git clone` of this repo at or past commit `c8fa010` (has the
  tuned `docker/mysql-target/my.cnf`).
- MSSQL Server 2022 image with the `StackOverflow2010` DB restored
  (same dataset as all other benches in the repo).
- ~90 minutes of wall time — one warm-up + two 6-run bench matrices
  (baseline + full-schema).

## Docker VM sizing

Docker Desktop → Settings → Resources → Memory → **22 GB** → Apply.

macOS + apps will want ~14 GB of the 36 GB, leaving 22 GB for Docker.
This is the M3-Max analogue of the M5 Pro's 12 GB / 12 GB split.
Values between 20 GB and 24 GB should all be safe; below 18 GB and
you start squeezing MSSQL source; above 26 GB and macOS starts
paging.

## Container caps

Two containers running concurrently during the bench (per the
project rule):

| Container     | cgroup | MySQL / MSSQL memory |
|---------------|-------:|---:|
| `mssql-bench` | 12 GB  | `max server memory = 10240` MB |
| `mysql-target`| 6 GB   | `innodb_buffer_pool_size = 2G` (unchanged) |

The M5 Pro caps were 5 GB / 4096 MB for MSSQL and 6 GB / 2 GB for
MySQL. MySQL stays identical because `docs/mysql-target-container.md`
has already shown 3 GB pool regresses and 4 GB OOMs — that's a
MySQL-internal tradeoff, not a function of host RAM.

## Setup steps

### 1. Set Docker Desktop VM to 22 GB

Restart Docker when prompted, then confirm:

```bash
docker info --format '{{.MemTotal}}' | awk '{printf "%.1f GB\n", $1/1024/1024/1024}'
# Expect ≈ 21.x GB
```

### 2. Start MSSQL source container

If `mssql-bench` already exists on the M3 Max with the
`StackOverflow2010` database restored, recreate it with the bigger
cgroup:

```bash
docker stop mssql-bench 2>/dev/null
docker update --memory=12g --memory-swap=12g mssql-bench
docker start mssql-bench
```

If you need a fresh container, use the repo's usual MSSQL setup
with a 12 GB cap. The dataset restore is out of scope for this doc.

Then raise MSSQL's max server memory:

```bash
docker exec mssql-bench /opt/mssql-tools18/bin/sqlcmd \
  -S localhost -U sa -P TestPass2024 -C -Q "
  EXEC sp_configure 'show advanced options', 1; RECONFIGURE;
  EXEC sp_configure 'max server memory (MB)', 10240;
  RECONFIGURE;
  SELECT CAST(value_in_use AS int) AS max_server_memory_MB
  FROM sys.configurations WHERE name = 'max server memory (MB)';
"
# Expect: max_server_memory_MB = 10240
```

### 3. (Re)create the MySQL target container

From the repo root:

```bash
docker rm -f mysql-target 2>/dev/null
docker run -d \
  --name mysql-target \
  --memory=6g --memory-swap=6g \
  -p 3307:3306 \
  -e MYSQL_ROOT_PASSWORD=TestPass2024 \
  -v "$PWD/docker/mysql-target/my.cnf:/etc/mysql/conf.d/tuned.cnf:ro" \
  mysql:8.0

until docker exec mysql-target mysqladmin -uroot -pTestPass2024 ping --silent 2>/dev/null; do sleep 2; done
```

Sanity-check:

```bash
docker exec -e MYSQL_PWD=TestPass2024 mysql-target mysql -uroot -N \
  -e "SELECT @@innodb_buffer_pool_size/1024/1024/1024 AS pool_gb, @@local_infile AS local_infile;"
# Expect: 2.0  1
```

### 4. Build dmt-rs

```bash
cargo build --release --features mysql
```

## Running the bench

Two A/Bs, each ~30-45 minutes (warm-up run + 6 interleaved runs).
They measure different parts of the migration.

### Baseline (no indexes, no FKs — what most dmt-rs users actually run)

```bash
LOG_DIR=.bench-logs-m3max-baseline ./scripts/bench-mysql-tuning.sh
```

### Full schema (create_indexes + create_foreign_keys enabled)

```bash
LOG_DIR=.bench-logs-m3max-full-schema ./scripts/bench-mysql-full-schema.sh
```

(Optional) LOAD DATA re-test — only if you want to confirm the
TSV-CPU finding holds on the M3 Max too:

```bash
LOG_DIR=.bench-logs-m3max-load-data ./scripts/bench-mysql-load-data.sh
```

## Interpretation

### Baseline A/B — compare the median `rows/s` to M5 Pro:

| config     | M5 Pro 24 GB | M3 Max 36 GB (to fill in) | Δ |
|------------|---:|---:|---:|
| tuning-on  | 120,668 rows/s | ___ | ___ |
| tuning-off | 118,639 rows/s | ___ | ___ |

Decision rule:

| Δ                | Interpretation |
|------------------|----------------|
| **< +15 %**      | Hypothesis rejected. MSSQL source RAM is not a meaningful lever. The M5 Pro documented ceiling holds. |
| **+15 % to +30 %** | Hypothesis confirmed at modest magnitude. Worth documenting the `max server memory = 10240` recommendation for users with ≥ 32 GB hosts. |
| **> +30 %**      | Dramatic uplift, echoing the mssql→mssql 2.7× jump. Warrants a code-side follow-up to see whether source-side parallel readers could push further on beefy hosts. |

### Variance check (bonus)

On the M5 Pro, the 12 GB VM produced a 12 K rows/s spread on
`tuning-on` across 3 runs; the 16 GB VM tightened that to 3 K. If
the M3 Max spread is also in the 3 K-or-tighter range, it confirms
the VM-pressure-causes-variance finding. If it isn't, something
else is going on.

### Full-schema A/B

Same comparison, using the published M3 Max cell:

| config           | M5 Pro 24 GB | M3 Max 36 GB (to fill in) | Δ |
|------------------|---:|---:|---:|
| full-tuning-on   | 49,099 rows/s (stock container) / ~117,967 (tuned) | ___ | ___ |
| full-tuning-off  | 44,786 rows/s (stock container) / ~117,175 (tuned) | ___ | ___ |

The tuned-container numbers are from
`docs/mysql-target-container.md`. Use those as the comparison point
— the stock-container numbers are historical.

## Sharing results back

Drop the raw `.bench-logs-m3max-*/results.tsv` files from each run
into a PR branch plus the filled-in tables above, plus a one-line
system snapshot:

```bash
{ sysctl -n hw.model hw.ncpu ; sysctl -n hw.memsize | awk '{printf "%.1f GB RAM\n", $1/1024/1024/1024}' ; docker info --format 'Docker VM: {{.MemTotal}}' | awk '/GB/ {printf "Docker VM: %.1f GB\n", $3/1024/1024/1024}' ; } 2>/dev/null
```

If the hypothesis confirms (≥ 15 %), expected follow-up commits:

1. Add an M3-Max results section to `docs/mysql-target-container.md`
   with the measured improvement.
2. Update the `docker_container_tuning.md` project memory with the
   M3 Max container caps + MSSQL max server memory split.
3. Consider whether the M5 Pro's 5 GB / 4096 MB MSSQL cap is
   leaving performance on the table too — if a 10 GB cap on a
   12 GB VM is feasible with the 2-container rule, worth re-testing
   there.

If the hypothesis rejects (< 15 %), drop a short "tested, not a
lever" note into the same doc and close the question.

## Restoring state after the bench

```bash
docker stop mysql-target
docker start mssql-target   # if you run two MSSQL containers normally
# Optionally: lower MSSQL max server memory back to 4096 if you want
# to preserve the M5 Pro parity profile on the same machine.
```
