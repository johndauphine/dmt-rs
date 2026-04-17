#!/bin/bash
# MySQL tuning A/B benchmark.
#
# Runs MSSQL -> MySQL drop_recreate of StackOverflow2010 with
# `mysql_bulk_session_tuning` on and off, three times each, and prints
# a markdown table of durations and throughput.
#
# Methodology:
#  * The target DB is dropped + recreated between runs so every run starts
#    from an empty target. The MSSQL source buffer pool is intentionally
#    NOT flushed — both variants see the same warm cache by the time
#    measurement starts (see warm-up below).
#  * A discarded warm-up run is performed first to prime the MSSQL buffer
#    pool. Without this, whichever variant ran first would be systematically
#    penalized by the cold cache.
#  * Variant order is interleaved (on/off/off/on/on/off) rather than
#    grouped, so any residual drift in system state cannot align with one
#    variant. Three observations per variant; we report the median.
#
# Expects:
#   - mssql-bench    container running, StackOverflow2010 on :1433
#   - mysql-target   container running, listening on :3307
#   - ./target/release/dmt-rs built with --features mysql

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BINARY="$SCRIPT_DIR/target/release/dmt-rs"
LOG_DIR="${LOG_DIR:-$SCRIPT_DIR/.bench-logs}"

# Runs per variant are hard-coded in the ORDER array below (see "Interleaved
# order" comment). Edit ORDER to change either count or sequence.

mkdir -p "$LOG_DIR"

if [ ! -x "$BINARY" ]; then
  echo "error: $BINARY not found. Build with: cargo build --release --features mysql" >&2
  exit 1
fi

MYSQL_CMD=(docker exec -i mysql-target mysql -uroot -pTestPass2024 -N -B)

reset_target() {
  "${MYSQL_CMD[@]}" -e "DROP DATABASE IF EXISTS stackoverflow_target; CREATE DATABASE stackoverflow_target CHARACTER SET utf8mb4;" 2>/dev/null
}

# Parse dmt-rs's trailing summary block. Uses -a so ANSI color codes in the
# log don't trip grep's binary-file detection, and anchors against the exact
# summary lines (two-space indent) so mid-run INFO lines can't leak in.
parse_metric() {
  local log="$1"
  local label="$2"   # Duration | Rows | Throughput
  grep -aE "^  ${label}: " "$log" | tail -1 | sed -E "s/^  ${label}: //"
}

run_one() {
  local config="$1"
  local label="$2"
  local run_idx="$3"
  local log="$LOG_DIR/${label}-run${run_idx}.log"

  reset_target

  local start end wall
  start=$(date +%s.%N)
  "$BINARY" -c "$config" run >"$log" 2>&1
  end=$(date +%s.%N)
  wall=$(awk -v s="$start" -v e="$end" 'BEGIN { printf "%.2f", e - s }')

  local duration_raw throughput_raw rows_raw
  duration_raw=$(parse_metric "$log" "Duration")
  throughput_raw=$(parse_metric "$log" "Throughput")
  rows_raw=$(parse_metric "$log" "Rows")

  # Normalize: Duration "441.14s" -> 441.14; Throughput "43774 rows/sec" -> 43774;
  # Rows "19310703" -> 19310703.
  local duration throughput rows
  duration=$(echo "$duration_raw" | grep -oE '^[0-9.]+' || echo "0")
  throughput=$(echo "$throughput_raw" | grep -oE '^[0-9]+' || echo "0")
  rows=$(echo "$rows_raw" | grep -oE '^[0-9]+' || echo "0")

  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$label" "$run_idx" "$wall" "$duration" "$rows" "$throughput"
}

RESULTS_TSV="$LOG_DIR/results.tsv"
printf 'config\trun\twall_sec\tdmt_duration_sec\trows\tthroughput_rows_per_sec\n' >"$RESULTS_TSV"

# Warm-up: discard. Primes MSSQL buffer pool with the hot tables.
echo ">>> warm-up (discarded)"
run_one "$SCRIPT_DIR/benchmark-mssql-to-mysql-tuning-on.yaml" "warmup" "0" >/dev/null || true

# Interleaved order: on/off/off/on/on/off => 3 observations per variant.
ORDER=(
  "tuning-on:1"
  "tuning-off:1"
  "tuning-off:2"
  "tuning-on:2"
  "tuning-on:3"
  "tuning-off:3"
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

# Markdown summary: median of each group.
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
