#!/bin/bash
# MySQL LOAD DATA LOCAL INFILE vs multi-row INSERT A/B benchmark.
#
# Ran against the TUNED mysql-target container (docker/mysql-target/my.cnf).
# Purpose: re-evaluate docs/mysql-performance-tuning.md's finding that LOAD
# DATA loses at >=2 workers. That finding was on stock mysql:8.0 with a
# 128 MB buffer pool, where I/O was the bottleneck and TSV-generation CPU
# was secondary. On the tuned container I/O is no longer the gate, so this
# bench re-measures whether LOAD DATA's zero-per-row-SQL-overhead advantage
# now beats the INSERT path's CPU-efficient path.
#
# Same methodology as bench-mysql-tuning.sh: warm-up discard, interleaved
# variant order, target DB dropped between runs, n=3 per variant, median
# reported.
#
# Expects:
#   - mssql-bench      running, StackOverflow2010 on :1433
#   - mysql-target     running, tuned my.cnf mounted, :3307
#   - ./target/release/dmt-rs built with --features mysql

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BINARY="$SCRIPT_DIR/target/release/dmt-rs"
LOG_DIR="${LOG_DIR:-$SCRIPT_DIR/.bench-logs-load-data}"

mkdir -p "$LOG_DIR"

if [ ! -x "$BINARY" ]; then
  echo "error: $BINARY not found. Build with: cargo build --release --features mysql" >&2
  exit 1
fi

# Local benchmark default only. Override with MYSQL_ROOT_PASSWORD if your
# mysql-target container uses a different password.
MYSQL_ROOT_PASSWORD="${MYSQL_ROOT_PASSWORD:-TestPass2024}"
MYSQL_CMD=(docker exec -i -e "MYSQL_PWD=$MYSQL_ROOT_PASSWORD" mysql-target mysql -uroot -N -B)

reset_target() {
  "${MYSQL_CMD[@]}" -e "DROP DATABASE IF EXISTS stackoverflow_target; CREATE DATABASE stackoverflow_target CHARACTER SET utf8mb4;" 2>/dev/null
}

# Portable high-resolution wall clock. macOS/BSD `date` doesn't support `%N`.
now_sec() {
  python3 -c 'import time; print(time.time())'
}

parse_metric() {
  local log="$1"
  local label="$2"
  grep -aE "^  ${label}: " "$log" | tail -1 | sed -E "s/^  ${label}: //"
}

run_one() {
  local config="$1"
  local label="$2"
  local run_idx="$3"
  local log="$LOG_DIR/${label}-run${run_idx}.log"

  reset_target

  local start end wall
  start=$(now_sec)
  "$BINARY" -c "$config" run >"$log" 2>&1
  end=$(now_sec)
  wall=$(awk -v s="$start" -v e="$end" 'BEGIN { printf "%.2f", e - s }')

  local duration_raw throughput_raw rows_raw
  duration_raw=$(parse_metric "$log" "Duration")
  throughput_raw=$(parse_metric "$log" "Throughput")
  rows_raw=$(parse_metric "$log" "Rows")

  local duration throughput rows
  duration=$(echo "$duration_raw" | grep -oE '^[0-9.]+' || echo "0")
  throughput=$(echo "$throughput_raw" | grep -oE '^[0-9]+' || echo "0")
  rows=$(echo "$rows_raw" | grep -oE '^[0-9]+' || echo "0")

  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$label" "$run_idx" "$wall" "$duration" "$rows" "$throughput"
}

RESULTS_TSV="$LOG_DIR/results.tsv"
printf 'config\trun\twall_sec\tdmt_duration_sec\trows\tthroughput_rows_per_sec\n' >"$RESULTS_TSV"

echo ">>> warm-up (discarded)"
run_one "$SCRIPT_DIR/benchmark-mssql-to-mysql-load-data-off.yaml" "warmup" "0" >/dev/null || true

ORDER=(
  "load-data-off:1"
  "load-data-on:1"
  "load-data-on:2"
  "load-data-off:2"
  "load-data-off:3"
  "load-data-on:3"
)

for entry in "${ORDER[@]}"; do
  variant="${entry%%:*}"
  idx="${entry##*:}"
  config="$SCRIPT_DIR/benchmark-mssql-to-mysql-${variant}.yaml"
  echo ">>> $variant run $idx"
  row=$(run_one "$config" "$variant" "$idx")
  echo "    $row"
  echo "$row" >>"$RESULTS_TSV"
done

echo
echo "Raw TSV: $RESULTS_TSV"
echo

python3 - "$RESULTS_TSV" <<'PY'
import csv, sys, statistics as s
path = sys.argv[1]
rows = [r for r in csv.DictReader(open(path), delimiter='\t')]
by = {}
for r in rows:
    by.setdefault(r['config'], []).append(r)

print("| config | n | median wall (s) | median dmt (s) | median rows/s | rows |")
print("|---|---|---|---|---|---|")
for cfg, rs in by.items():
    walls = sorted(float(r['wall_sec']) for r in rs)
    dmts = sorted(float(r['dmt_duration_sec']) for r in rs)
    tps = sorted(int(r['throughput_rows_per_sec']) for r in rs)
    rows = int(rs[0]['rows'])
    print(f"| {cfg} | {len(rs)} | {s.median(walls):.2f} | {s.median(dmts):.2f} | {s.median(tps):,} | {rows:,} |")
PY
