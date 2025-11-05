#!/bin/bash
# Run tests multiple times and detect flaky tests

set -e

TOTAL_RUNS=${1:-10}
TEST_SUITE=${2:-e2e_samples}

echo "Running $TEST_SUITE tests $TOTAL_RUNS times to detect flaky tests..."
echo "======================================================================"
echo ""

FAILED_TESTS_FILE="/tmp/failed_${TEST_SUITE}_tests.txt"
> "$FAILED_TESTS_FILE"

for i in $(seq 1 $TOTAL_RUNS); do
    echo "Run $i/$TOTAL_RUNS"
    
    # Run tests and capture output
    OUTPUT=$(cargo test --test "$TEST_SUITE" -- --test-threads=1 2>&1)
    echo "$OUTPUT" > "/tmp/run_${i}_${TEST_SUITE}.log"
    
    # Check if tests passed
    if echo "$OUTPUT" | grep -q "test result: ok"; then
        echo "  ✓ PASSED"
    else
        echo "  ✗ FAILED"
        # Extract failed test names
        echo "$OUTPUT" | grep -E "test.*\.\.\..*FAILED" | sed 's/.*test //; s/\.\.\..*//' >> "$FAILED_TESTS_FILE"
        # Also get the error message for the first failure
        if [ $i -eq 1 ]; then
            echo ""
            echo "First failure details:"
            echo "$OUTPUT" | grep -A 10 "FAILED" | head -15
            echo ""
        fi
    fi
    
    echo ""
done

echo ""
echo "======================================================================"
echo "Test Run Summary"
echo "======================================================================"
echo "Total runs: $TOTAL_RUNS"
echo ""

if [ -s "$FAILED_TESTS_FILE" ]; then
    echo "Flaky Tests Detected:"
    echo "---------------------"
    sort "$FAILED_TESTS_FILE" | uniq -c | sort -rn | while read count test; do
        percentage=$(echo "scale=1; $count * 100 / $TOTAL_RUNS" | bc)
        echo "  $test: failed $count/$TOTAL_RUNS times (${percentage}%)"
    done
    echo ""
    echo "Total unique flaky tests: $(sort "$FAILED_TESTS_FILE" | uniq | wc -l)"
else
    echo "✓ No flaky tests detected! All tests passed consistently."
fi

echo ""
echo "Detailed failure logs saved in /tmp/run_*_${TEST_SUITE}.log"

