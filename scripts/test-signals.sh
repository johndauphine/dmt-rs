#!/bin/bash
#
# Signal Handling Test Script
# Tests SIGINT (Ctrl-C) and SIGTERM handling for graceful shutdown
#
# Prerequisites:
#   - Running MSSQL and PostgreSQL instances with data
#   - Valid config file at the path specified
#
# Usage:
#   ./scripts/test-signals.sh [config-file] [signal] [delay]
#
# Arguments:
#   config-file - Path to config YAML (default: test-config.yaml)
#   signal      - Signal to send: INT or TERM (default: TERM)
#   delay       - Seconds to wait before sending signal (default: 5)

set -e

CONFIG_FILE="${1:-test-config.yaml}"
SIGNAL="${2:-TERM}"
DELAY="${3:-5}"
BINARY="./target/release/dmt-rs"

echo "=== Signal Handling Test ==="
echo "Config: $CONFIG_FILE"
echo "Signal: SIG$SIGNAL"
echo "Delay: ${DELAY}s"
echo ""

# Build release binary if needed
if [ ! -f "$BINARY" ]; then
    echo "Building release binary..."
    cargo build --release
fi

# Start migration in background
# State is persisted in the target DB's `_dmt_rs` schema — no file needed.
echo "Starting migration..."
$BINARY -c "$CONFIG_FILE" run &
PID=$!

echo "Migration PID: $PID"
echo "Waiting ${DELAY}s before sending signal..."
sleep "$DELAY"

# Check if process is still running
if ! kill -0 $PID 2>/dev/null; then
    echo "ERROR: Process already exited before signal could be sent"
    exit 1
fi

# Send signal
echo "Sending SIG$SIGNAL to PID $PID..."
kill -"$SIGNAL" $PID

# Wait for process to exit
echo "Waiting for graceful shutdown..."
set +e
wait $PID
EXIT_CODE=$?
set -e

echo ""
echo "=== Results ==="
echo "Exit code: $EXIT_CODE"

# Check expected exit code
if [ $EXIT_CODE -eq 5 ]; then
    echo "PASS: Exit code is 5 (cancelled)"
else
    echo "FAIL: Expected exit code 5, got $EXIT_CODE"
fi

echo ""
echo "Note: migration state is stored in the target DB's _dmt_rs schema."
echo "To verify state was persisted, query the target DB:"
echo "  SELECT run_id, status FROM _dmt_rs.migration_runs ORDER BY started_at DESC LIMIT 1;"
echo "Done"
