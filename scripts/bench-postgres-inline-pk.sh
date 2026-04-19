#!/bin/bash
# PostgreSQL inline-PK A/B benchmark.
#
# Question under test: does emitting CONSTRAINT ... PRIMARY KEY inline in
# CREATE TABLE (vs ALTER TABLE ADD CONSTRAINT at finalize) reduce total
# wall-clock time for a drop_recreate migration to a PostgreSQL target?
#
# Unlike MySQL/InnoDB, PG heap storage is NOT PK-clustered — the PK is a
# separate btree. Two opposing effects to measure:
#   - Inline PK: btree maintained incrementally during COPY (per-row cost)
#   - Finalize PK: sort-based bulk btree build at end (bulk advantage on
#     large tables) vs. per-table DDL overhead (round-trip per table)
#
# Whichever dominates depends on the table-size distribution. This script
# doesn't assume; it measures.
#
# Methodology mirrors bench-mysql-inline-pk.sh: warm-up discard, interleaved
# variant order, target DB dropped between runs, n=3 per variant, median
# reported. Parses "Primary keys created in X.XXs" from orchestrator logs to
# isolate the finalize PK delta from overall wall-clock.
#
# Expects:
#   - mssql-bench running, StackOverflow2010 on :1433
#   - pg-source   running on :5432 with a stackoverflow_target DB reachable
#   - Two binaries: baseline (pre-fix) and inline-pk (post-fix). Build both.
#
# BUILD (run from repo root):
#   git worktree add --detach /tmp/dmt-baseline-pg main
#   (cd /tmp/dmt-baseline-pg && cargo build --release)
#   export BINARY_BASELINE=/tmp/dmt-baseline-pg/target/release/dmt-rs
#
#   cargo build --release
#   export BINARY_INLINE_PK=$(pwd)/target/release/dmt-rs

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
LOG_DIR="${LOG_DIR:-$SCRIPT_DIR/.bench-logs-inline-pk-pg}"

BINARY_BASELINE="${BINARY_BASELINE:-}"
BINARY_INLINE_PK="${BINARY_INLINE_PK:-$SCRIPT_DIR/target/release/dmt-rs}"

if [ -z "$BINARY_BASELINE" ] || [ ! -x "$BINARY_BASELINE" ]; then
  echo "error: BINARY_BASELINE env var must point to a pre-fix binary built with --release" >&2
  echo "       see BUILD instructions at the top of this script" >&2
  exit 1
fi
if [ ! -x "$BINARY_INLINE_PK" ]; then
  echo "error: BINARY_INLINE_PK ($BINARY_INLINE_PK) not found or not executable" >&2
  exit 1
fi

mkdir -p "$LOG_DIR"

PG_PASSWORD="${PG_PASSWORD:-TestPass2024}"
PG_CMD=(docker exec -i -e "PGPASSWORD=$PG_PASSWORD" pg-source psql -U postgres -d postgres -tAq)

reset_target() {
  "${PG_CMD[@]}" -c "DROP DATABASE IF EXISTS stackoverflow_target;" >/dev/null 2>&1
  "${PG_CMD[@]}" -c "CREATE DATABASE stackoverflow_target;" >/dev/null 2>&1
}

now_sec() {
  python3 -c 'import time; print(time.time())'
}

parse_metric() {
  local log="$1"
  local label="$2"
  grep -aE "^  ${label}: " "$log" | tail -1 | sed -E "s/^  ${label}: //"
}

parse_pk_finalize_sec() {
  local log="$1"
  grep -aoE 'Primary keys created in [0-9.]+s' "$log" \
    | tail -1 \
    | grep -oE '[0-9.]+' \
    | head -1 \
    || echo "0"
}

run_one() {
  local binary="$1"
  local config="$2"
  local label="$3"
  local run_idx="$4"
  local log="$LOG_DIR/${label}-run${run_idx}.log"

  reset_target

  local start end wall
  start=$(now_sec)
  "$binary" -c "$config" run >"$log" 2>&1
  end=$(now_sec)
  wall=$(awk -v s="$start" -v e="$end" 'BEGIN { printf "%.2f", e - s }')

  local duration_raw throughput_raw rows_raw
  duration_raw=$(parse_metric "$log" "Duration")
  throughput_raw=$(parse_metric "$log" "Throughput")
  rows_raw=$(parse_metric "$log" "Rows")

  local duration throughput rows pk_sec
  duration=$(echo "$duration_raw" | grep -oE '^[0-9.]+' || echo "0")
  throughput=$(echo "$throughput_raw" | grep -oE '^[0-9]+' || echo "0")
  rows=$(echo "$rows_raw" | grep -oE '^[0-9]+' || echo "0")
  pk_sec=$(parse_pk_finalize_sec "$log")

  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$label" "$run_idx" "$wall" "$duration" "$pk_sec" "$rows" "$throughput"
}

CONFIG="$SCRIPT_DIR/benchmark-pg-to-pg-inline-pk.yaml"

RESULTS_TSV="$LOG_DIR/results.tsv"
printf 'variant\trun\twall_sec\tdmt_duration_sec\tpk_finalize_sec\trows\tthroughput_rows_per_sec\n' >"$RESULTS_TSV"

echo ">>> warm-up (discarded)"
run_one "$BINARY_INLINE_PK" "$CONFIG" "warmup" "0" >/dev/null || true

ORDER=(
  "baseline:1"
  "inline-pk:1"
  "inline-pk:2"
  "baseline:2"
  "baseline:3"
  "inline-pk:3"
)

for entry in "${ORDER[@]}"; do
  variant="${entry%%:*}"
  idx="${entry##*:}"
  case "$variant" in
    baseline)  binary="$BINARY_BASELINE" ;;
    inline-pk) binary="$BINARY_INLINE_PK" ;;
    *) echo "unknown variant $variant" >&2; exit 1 ;;
  esac
  echo ">>> $variant run $idx"
  row=$(run_one "$binary" "$CONFIG" "$variant" "$idx")
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
    by.setdefault(r['variant'], []).append(r)

def med(xs):
    return s.median(xs)

print("| variant | n | median wall (s) | median dmt (s) | median PK finalize (s) | median rows/s | rows |")
print("|---|---|---|---|---|---|---|")
medians = {}
for variant, rs in by.items():
    walls = sorted(float(r['wall_sec']) for r in rs)
    dmts  = sorted(float(r['dmt_duration_sec']) for r in rs)
    pks   = sorted(float(r['pk_finalize_sec']) for r in rs)
    tps   = sorted(int(r['throughput_rows_per_sec']) for r in rs)
    rows_ = int(rs[0]['rows'])
    medians[variant] = (med(walls), med(dmts), med(pks))
    print(f"| {variant} | {len(rs)} | {med(walls):.2f} | {med(dmts):.2f} | {med(pks):.2f} | {med(tps):,} | {rows_:,} |")

if 'baseline' in medians and 'inline-pk' in medians:
    bw, bd, bp = medians['baseline']
    iw, id_, ip = medians['inline-pk']
    print()
    print(f"wall delta:        {iw - bw:+.2f}s  ({(iw - bw) / bw * 100:+.1f}%)")
    print(f"dmt delta:         {id_ - bd:+.2f}s  ({(id_ - bd) / bd * 100:+.1f}%)")
    print(f"PK finalize delta: {ip - bp:+.2f}s  (baseline {bp:.2f}s -> inline-pk {ip:.2f}s)")
PY
