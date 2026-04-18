#!/bin/bash
# MySQL inline-PK A/B benchmark.
#
# Question under test: does emitting `PRIMARY KEY (...)` inline in CREATE TABLE
# (vs. adding it via ALTER TABLE ADD PRIMARY KEY at finalize) actually reduce
# total wall-clock time for a drop_recreate migration to an InnoDB target?
#
# Why we expect a win: InnoDB tables are clustered on PK. Post-load
# `ALTER TABLE ADD PRIMARY KEY` rewrites the entire table to reorganize the
# clustered index — O(table size). Inline PK avoids that rewrite by building
# the clustered index incrementally as rows are inserted.
#
# Methodology mirrors bench-mysql-load-data.sh: warm-up discard, interleaved
# variant order, target DB dropped between runs, n=3 per variant, median
# reported. Parses "Primary keys created in X.XXs" from orchestrator logs to
# isolate the finalize PK delta from overall wall-clock.
#
# Expects:
#   - mssql-bench  running, StackOverflow2010 on :1433
#   - mysql-target running, :3307
#   - Two binaries: baseline (pre-fix) and inline-pk (post-fix). Build both
#     with `--features mysql`. See BUILD steps below.
#
# BUILD (run from repo root):
#   # 1. Build baseline from the commit *before* the inline-PK change:
#   git worktree add /tmp/dmt-baseline <commit-before-fix>
#   (cd /tmp/dmt-baseline && cargo build --release --features mysql)
#   export BINARY_BASELINE=/tmp/dmt-baseline/target/release/dmt-rs
#
#   # 2. Build inline-PK from current HEAD:
#   cargo build --release --features mysql
#   export BINARY_INLINE_PK=$(pwd)/target/release/dmt-rs

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
LOG_DIR="${LOG_DIR:-$SCRIPT_DIR/.bench-logs-inline-pk}"

BINARY_BASELINE="${BINARY_BASELINE:-}"
BINARY_INLINE_PK="${BINARY_INLINE_PK:-$SCRIPT_DIR/target/release/dmt-rs}"

if [ -z "$BINARY_BASELINE" ] || [ ! -x "$BINARY_BASELINE" ]; then
  echo "error: BINARY_BASELINE env var must point to a pre-fix binary built with --features mysql" >&2
  echo "       see BUILD instructions at the top of this script" >&2
  exit 1
fi
if [ ! -x "$BINARY_INLINE_PK" ]; then
  echo "error: BINARY_INLINE_PK ($BINARY_INLINE_PK) not found or not executable" >&2
  exit 1
fi

mkdir -p "$LOG_DIR"

MYSQL_ROOT_PASSWORD="${MYSQL_ROOT_PASSWORD:-TestPass2024}"
MYSQL_CMD=(docker exec -i -e "MYSQL_PWD=$MYSQL_ROOT_PASSWORD" mysql-target mysql -uroot -N -B)

reset_target() {
  "${MYSQL_CMD[@]}" -e "DROP DATABASE IF EXISTS stackoverflow_target; CREATE DATABASE stackoverflow_target CHARACTER SET utf8mb4;" 2>/dev/null
}

now_sec() {
  python3 -c 'import time; print(time.time())'
}

parse_metric() {
  local log="$1"
  local label="$2"
  grep -aE "^  ${label}: " "$log" | tail -1 | sed -E "s/^  ${label}: //"
}

# Isolate the PK-creation portion of finalize. orchestrator/mod.rs:1565 logs:
#   "Primary keys created in 12.34s"
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

CONFIG="$SCRIPT_DIR/benchmark-mssql-to-mysql-load-data-on.yaml"

RESULTS_TSV="$LOG_DIR/results.tsv"
printf 'variant\trun\twall_sec\tdmt_duration_sec\tpk_finalize_sec\trows\tthroughput_rows_per_sec\n' >"$RESULTS_TSV"

echo ">>> warm-up (discarded)"
run_one "$BINARY_INLINE_PK" "$CONFIG" "warmup" "0" >/dev/null || true

# Interleaved to absorb any host-level drift across the run window.
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
