#!/bin/bash
# Run stress tests for duroxide-pg
#
# Usage:
#   ./scripts/run-stress-tests.sh                    # Run all stress tests
#   ./scripts/run-stress-tests.sh longpoll           # Run long-polling stress tests only
#   ./scripts/run-stress-tests.sh continue_as_new    # Run continue-as-new stress tests only
#   ./scripts/run-stress-tests.sh <test_name>        # Run specific test

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

cd "$PROJECT_DIR"

echo "=============================================="
echo "  duroxide-pg Stress Tests"
echo "=============================================="
echo ""

# Check if DATABASE_URL is set
if [ -z "$DATABASE_URL" ]; then
    if [ -f .env ]; then
        echo "Loading DATABASE_URL from .env file..."
        export $(grep -v '^#' .env | xargs)
    else
        echo "ERROR: DATABASE_URL not set and no .env file found"
        exit 1
    fi
fi

echo "Database: ${DATABASE_URL%%@*}@..."
echo ""

run_longpoll_stress() {
    echo "=== Long-Polling Stress Tests ==="
    echo ""
    cargo test --test stress_tests_longpoll -- --ignored --nocapture
}

run_continue_as_new_stress() {
    echo "=== Continue-as-New Stress Tests ==="
    echo ""
    cargo test --test continue_as_new_stress_tests -- --ignored --nocapture
}

run_general_stress() {
    echo "=== General Stress Tests (excluding longpoll comparison tests) ==="
    echo ""
    # Run all stress tests EXCEPT the longpoll comparison tests (they need single-thread for metrics)
    cargo test --test stress_tests -- --ignored --nocapture --skip stress_test_longpoll_comparison 2>/dev/null || true
}

run_longpoll_comparison_stress() {
    echo "=== Long-Poll Comparison Stress Tests (single-threaded for metrics) ==="
    echo ""
    # These tests use the global metrics recorder and must run single-threaded
    cargo test --test stress_tests stress_test_longpoll_comparison --features db-metrics -- --ignored --nocapture --test-threads=1
}

case "$1" in
    "longpoll")
        run_longpoll_stress
        ;;
    "continue_as_new"|"can")
        run_continue_as_new_stress
        ;;
    "general")
        run_general_stress
        ;;
    "comparison"|"longpoll_comparison")
        run_longpoll_comparison_stress
        ;;
    "")
        # Run all stress tests
        echo "Running ALL stress tests..."
        echo ""
        run_longpoll_stress
        echo ""
        run_continue_as_new_stress
        echo ""
        run_general_stress
        echo ""
        run_longpoll_comparison_stress
        ;;
    *)
        # Run specific test by name
        echo "Running specific test: $1"
        echo ""
        cargo test --test stress_tests_longpoll "$1" -- --ignored --nocapture 2>/dev/null || \
        cargo test --test continue_as_new_stress_tests "$1" -- --ignored --nocapture 2>/dev/null || \
        cargo test --test stress_tests "$1" -- --ignored --nocapture 2>/dev/null || \
        echo "Test '$1' not found in any stress test file"
        ;;
esac

echo ""
echo "=============================================="
echo "  Stress tests completed"
echo "=============================================="
