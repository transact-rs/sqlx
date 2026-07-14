#!/bin/bash

TARGETS=(
    "fuzz_mysql_lenenc"
    "fuzz_mysql_row_binary"
    "fuzz_mysql_handshake"
    "fuzz_postgres_data_row"
    "fuzz_postgres_response"
)

LOG_DIR="$(pwd)/fuzz_logs"
mkdir -p "$LOG_DIR"

echo "🚀 Starting sqlx fuzzers..."
echo "📁 Logs directory: $LOG_DIR"
echo ""

for target in "${TARGETS[@]}"; do
    timestamp=$(date +%Y%m%d_%H%M%S)
    log_file="$LOG_DIR/${target}_${timestamp}.log"
    
    echo "Starting ${target}..."
    echo "  Log: $log_file"
    
    cd fuzz && nohup cargo fuzz run "$target" > "$log_file" 2>&1 &
    pid=$!
    echo "  PID: $pid"
    echo ""
    cd ..
done

echo "✅ All fuzzers started!"
echo ""
echo "To check status: ./check_fuzzing_status.sh"
echo "To stop all fuzzers: ./stop_fuzzing.sh"
