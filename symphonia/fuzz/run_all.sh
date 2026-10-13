#!/bin/bash
set -e

# Default duration: 10 seconds per target
DURATION=${1:-10}

# Derive the target list from the fuzz_targets directory so it cannot drift.
TARGETS=()
for f in "$(dirname "$0")"/fuzz_targets/*.rs; do
    TARGETS+=("$(basename "$f" .rs)")
done

echo "Running each fuzzer for $DURATION seconds..."

for target in "${TARGETS[@]}"; do
    echo "----------------------------------------------------------------"
    echo "Fuzzing target: $target"
    echo "----------------------------------------------------------------"
    # -max_total_time is a libFuzzer flag passed via cargo fuzz
    cargo fuzz run "$target" -- -max_total_time="$DURATION"
done

echo "----------------------------------------------------------------"
echo "All fuzzers finished running for $DURATION seconds each."
