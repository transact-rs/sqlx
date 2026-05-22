#!/bin/bash

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOG_DIR="$SCRIPT_DIR/fuzz_logs"

echo "🔍 Fuzzing Status Check"
echo "======================="
echo ""

TARGETS=(
    "fuzz_mysql_lenenc"
    "fuzz_mysql_row_binary"
    "fuzz_mysql_handshake"
    "fuzz_postgres_data_row"
    "fuzz_postgres_response"
)

RUNNING_COUNT=0
for TARGET in "${TARGETS[@]}"; do
    PID=$(pgrep -f "cargo-fuzz.*$TARGET" || true)
    if [ -n "$PID" ]; then
        echo "✅ Fuzzer $((++RUNNING_COUNT)) (PID $PID): RUNNING"
    else
        echo "❌ Fuzzer for $TARGET: NOT RUNNING"
    fi
done

echo ""
echo "Summary: $RUNNING_COUNT/5 fuzzers running"
echo ""
echo "📊 Latest Stats:"
echo "==============="
echo ""

for TARGET in "${TARGETS[@]}"; do
    LATEST_LOG=$(ls -t "$LOG_DIR"/${TARGET}_*.log 2>/dev/null | head -1)
    if [ -n "$LATEST_LOG" ]; then
        echo "${TARGET}:"
        tail -3 "$LATEST_LOG" 2>/dev/null || echo "  No stats yet"
        echo ""
    fi
done

echo "💥 Crashes Found:"
echo "================="
CRASH_COUNT=$(find "$SCRIPT_DIR/fuzz/artifacts" -name "crash-*" -type f 2>/dev/null | wc -l)
if [ "$CRASH_COUNT" -gt 0 ]; then
    echo "  Found $CRASH_COUNT crash(es)!"
    find "$SCRIPT_DIR/fuzz/artifacts" -name "crash-*" -type f 2>/dev/null
else
    echo "  None found yet (keep running!)"
fi
