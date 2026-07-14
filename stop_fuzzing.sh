#!/bin/bash

echo "🛑 Stopping sqlx fuzzers..."
echo ""

PIDS=$(pgrep -f "cargo-fuzz" || true)

if [ -z "$PIDS" ]; then
    echo "No fuzzer processes found"
    exit 0
fi

COUNT=0
for PID in $PIDS; do
    CMDLINE=$(ps -p $PID -o command= 2>/dev/null || echo "unknown")
    if [[ $CMDLINE == *"cargo-fuzz"* ]]; then
        TARGET=$(echo "$CMDLINE" | grep -o 'fuzz_[a-z_]*' | head -1)
        echo "Stopping $TARGET (PID: $PID)..."
        kill $PID 2>/dev/null && ((COUNT++))
    fi
done

echo ""
if [ $COUNT -gt 0 ]; then
    echo "✅ Stopped $COUNT fuzzer(s)"
else
    echo "⚠️  No fuzzers were stopped"
fi

sleep 1
REMAINING=$(pgrep -f "cargo-fuzz" | wc -l)
if [ "$REMAINING" -gt 0 ]; then
    echo "⚠️  Warning: $REMAINING fuzzer process(es) still running"
    echo "   Use 'pkill -9 -f cargo-fuzz' to force kill if needed"
fi
