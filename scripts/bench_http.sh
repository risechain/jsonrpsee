#!/bin/bash

set -e

if ! command -v hyperfine &> /dev/null; then
    echo "hyperfine is not installed. Please install it first:"
    echo "cargo install --locked hyperfine"
    exit 1
fi

if ! command -v jq &> /dev/null; then
    echo "jq is not installed. Please install it first:"
    echo "apt-get install jq"   # For Debian/Ubuntu
    exit 1
fi

echo "Building examples..."
cd "$(dirname "$0")/.."
cargo build --release --example http_vs_http3 --features http3

PAYLOAD_SIZES=(3 9 27 81 243)
RESULTS_DIR="benchmark_results"
mkdir -p "$RESULTS_DIR"

echo "Increasing file descriptor limits..."
ulimit -n 100000 || echo "Warning: Failed to increase file descriptor limits. Benchmarks may fail."

clear_cache() {
    echo "Clearing OS cache..."
    if [ "$(uname)" = "Linux" ]; then
        if command -v sudo &> /dev/null && sudo -n true 2>/dev/null; then
            sudo sh -c "echo 3 > /proc/sys/vm/drop_caches"
        else
            echo "Warning: Cannot clear OS cache (need sudo). Results may be affected by caching."
        fi
    elif [ "$(uname)" = "Darwin" ]; then
        if command -v sudo &> /dev/null && sudo -n true 2>/dev/null; then
            sudo purge
        else
            echo "Warning: Cannot clear OS cache (need sudo). Results may be affected by caching."
        fi
    else
        echo "Warning: Unsupported OS for cache clearing. Results may be affected by caching."
    fi
}

run_benchmark() {
    local size=$1
    local runs=${2:-20}
    local warmup=${3:-5}

    echo "Benchmarking with payload size: $size elements"

    clear_cache

    HTTP_PORT=$(( 9000 + RANDOM % 1000 ))
    HTTP3_PORT=$(( 10000 + RANDOM % 1000 ))

    pkill -f "http_vs_http3.*-s" || true
    sleep 2

    # Create temp directory for logs
    TEMP_DIR=$(mktemp -d)
    HTTP_LOG="$TEMP_DIR/http_server.log"
    HTTP3_LOG="$TEMP_DIR/http3_server.log"

    echo "Starting HTTP server on port $HTTP_PORT..."
    SERVER_PORT=$HTTP_PORT RUST_LOG=info cargo run --release --example http_vs_http3 -- -t http -p "$size" -s > "$HTTP_LOG" 2>&1 &
    HTTP_PID=$!

    # Increased timeout to 60 seconds
    for i in {1..60}; do
        if grep -q "Server listening on" "$HTTP_LOG"; then
            echo "HTTP server started successfully"
            break
        fi
        if [ $i -eq 60 ]; then
            echo "Timed out waiting for HTTP server to start"
            cat "$HTTP_LOG"
            kill $HTTP_PID 2>/dev/null || true
            rm -rf "$TEMP_DIR"
            exit 1
        fi
        sleep 1
    done

    echo "Starting HTTP/3 server on port $HTTP3_PORT..."
    SERVER_PORT=$HTTP3_PORT RUST_LOG=info cargo run --release --features http3 --example http_vs_http3 -- -t http3 -p "$size" -s > "$HTTP3_LOG" 2>&1 &
    HTTP3_PID=$!

    # Increased timeout to 60 seconds
    for i in {1..60}; do
        if grep -q "Server listening on" "$HTTP3_LOG"; then
            echo "HTTP/3 server started successfully"
            break
        fi
        if [ $i -eq 60 ]; then
            echo "Timed out waiting for HTTP/3 server to start"
            cat "$HTTP3_LOG"
            kill $HTTP_PID $HTTP3_PID 2>/dev/null || true
            rm -rf "$TEMP_DIR"
            exit 1
        fi
        sleep 1
    done

    HTTP_ADDR=$(grep -o "Server listening on [0-9.]*:[0-9]*" "$HTTP_LOG" | head -1 | awk '{print $4}')
    HTTP3_ADDR=$(grep -o "Server listening on [0-9.]*:[0-9]*" "$HTTP3_LOG" | head -1 | awk '{print $4}')

    if [ -z "$HTTP_ADDR" ] || [ -z "$HTTP3_ADDR" ]; then
        echo "Failed to capture server addresses:"
        echo "HTTP log:"
        cat "$HTTP_LOG"
        echo "HTTP3 log:"
        cat "$HTTP3_LOG"
        kill $HTTP_PID $HTTP3_PID 2>/dev/null || true
        rm -rf "$TEMP_DIR"
        exit 1
    fi

    echo "HTTP server address: $HTTP_ADDR"
    echo "HTTP/3 server address: $HTTP3_ADDR"

    prepare_cmd="sleep 0.5 && $([ "$(uname)" = "Linux" ] && echo "sync" || echo "true")"

    echo "Running benchmarks with payload size: $size elements"
    echo "HTTP URL: http://$HTTP_ADDR"
    echo "HTTP/3 URL: http3://$HTTP3_ADDR"

    echo "Testing HTTP client..."
    if ! RUST_LOG=info timeout 30s cargo run --release --example http_vs_http3 -- -t http -p $size -r 1 -c 1 -u http://$HTTP_ADDR; then
        echo "Warning: HTTP client test failed. Skipping benchmarks for this payload size."
        kill $HTTP_PID $HTTP3_PID 2>/dev/null || true
        rm -rf "$TEMP_DIR"
        return 1
    fi

    echo "Testing HTTP/3 client..."
    if ! RUST_LOG=info timeout 30s cargo run --release --features http3 --example http_vs_http3 -- -t http3 -p $size -r 1 -c 1 -u http3://$HTTP3_ADDR; then
        echo "Warning: HTTP/3 client test failed. This may be due to QUIC protocol issues in the current environment."
        echo "Continuing with HTTP-only benchmark."

        hyperfine \
            --warmup "$warmup" \
            --runs "$runs" \
            --export-json "$RESULTS_DIR/payload_${size}.json" \
            --prepare "$prepare_cmd" \
            --time-unit millisecond \
            "env RUST_LOG=info timeout 30s cargo run --release --example http_vs_http3 -- -t http -p $size -r 100 -c 10 -u http://$HTTP_ADDR"
    else
        hyperfine \
            --warmup "$warmup" \
            --runs "$runs" \
            --export-json "$RESULTS_DIR/payload_${size}.json" \
            --prepare "$prepare_cmd" \
            --time-unit millisecond \
            "env RUST_LOG=info timeout 30s cargo run --release --example http_vs_http3 -- -t http -p $size -r 100 -c 10 -u http://$HTTP_ADDR" \
            "env RUST_LOG=info timeout 30s cargo run --release --features http3 --example http_vs_http3 -- -t http3 -p $size -r 100 -c 10 -u http3://$HTTP3_ADDR"
    fi

    if [ -f "$RESULTS_DIR/payload_${size}.json" ]; then
        HTTP_TIME=$(jq '.results[0].mean' "$RESULTS_DIR/payload_${size}.json")
        HTTP3_TIME=$(jq '.results[1].mean' "$RESULTS_DIR/payload_${size}.json" 2>/dev/null || echo "null")

        if [ "$HTTP3_TIME" != "null" ]; then
            IMPROVEMENT=$(echo "scale=2; (($HTTP_TIME - $HTTP3_TIME) / $HTTP_TIME) * 100" | bc)
            echo "HTTP/3 improvement for ${size} elements: ${IMPROVEMENT}%"
        else
            echo "HTTP/3 benchmark failed for ${size} elements. No comparison available."
        fi
    else
        echo "Benchmark results file not found for ${size} elements."
    fi

    echo "Stopping servers..."
    kill $HTTP_PID $HTTP3_PID 2>/dev/null || true
    wait $HTTP_PID $HTTP3_PID 2>/dev/null || true
    sleep 1

    rm -rf "$TEMP_DIR"
    clear_cache
}

for size in "${PAYLOAD_SIZES[@]}"; do
    run_benchmark "$size"
done

echo "Generating summary report..."
echo "# HTTP/3 Performance Benchmark Results" > "$RESULTS_DIR/summary.md"
echo "" >> "$RESULTS_DIR/summary.md"
echo "| Payload Size | HTTP/1.1 Mean (ms) | HTTP/3 Mean (ms) | Improvement (%) |" >> "$RESULTS_DIR/summary.md"
echo "|-------------|-----------------|---------------|---------------|" >> "$RESULTS_DIR/summary.md"

for size in "${PAYLOAD_SIZES[@]}"; do
    if [ -f "$RESULTS_DIR/payload_${size}.json" ]; then
        HTTP_TIME=$(jq '.results[0].mean' "$RESULTS_DIR/payload_${size}.json" 2>/dev/null || echo "N/A")
        HTTP3_TIME=$(jq '.results[1].mean' "$RESULTS_DIR/payload_${size}.json" 2>/dev/null || echo "N/A")

        if [ "$HTTP_TIME" != "N/A" ] && [ "$HTTP3_TIME" != "N/A" ]; then
            IMPROVEMENT=$(echo "scale=2; (($HTTP_TIME - $HTTP3_TIME) / $HTTP_TIME) * 100" | bc 2>/dev/null || echo "N/A")
            echo "| ${size} elements | ${HTTP_TIME} | ${HTTP3_TIME} | ${IMPROVEMENT}% |" >> "$RESULTS_DIR/summary.md"
        else
            echo "| ${size} elements | ${HTTP_TIME} | ${HTTP3_TIME} | N/A |" >> "$RESULTS_DIR/summary.md"
        fi
    else
        echo "| ${size} elements | N/A | N/A | N/A |" >> "$RESULTS_DIR/summary.md"
    fi
done

echo ""
echo "Benchmark complete! Results saved to $RESULTS_DIR/summary.md"
echo "To view the summary: cat $RESULTS_DIR/summary.md"
