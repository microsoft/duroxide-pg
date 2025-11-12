# Quick Start: Performance Analysis

One-page guide to collecting performance data for AI analysis.

## Prerequisites

```bash
# Ensure database is configured
cat .env | grep DATABASE_URL

# Create logs directory
mkdir -p logs
```

## Data Collection (Copy-Paste These Commands)

```bash
# 1. Clean environment
./scripts/cleanup_test_schemas.sh

# 2. Start measurement
./scripts/start-measurement.sh

# 3. Run all tests with debug logging (15-20 minutes)
RUST_LOG=debug cargo test --lib --test basic_tests -- --nocapture 2>&1 | tee logs/basic_tests_debug.log

RUST_LOG=debug cargo test --test postgres_provider_test -- --nocapture 2>&1 | tee logs/provider_validation_debug.log

RUST_LOG=debug cargo test --test e2e_samples -- --nocapture 2>&1 | tee logs/e2e_samples_debug.log

RUST_LOG=debug cargo run --release --package duroxide-pg-stress --bin pg-stress -- --duration 10 2>&1 | tee logs/stress_test_debug.log

# 4. Collect server-side stats
./scripts/stop-measurement.sh | tee logs/pg_stat_statements.log

# 5. Verify log files
ls -lh logs/
```

## Expected Files

After running above commands, you should have:

```
logs/
├── basic_tests_debug.log         (~500 KB)
├── provider_validation_debug.log (~2 MB)
├── e2e_samples_debug.log         (~5 MB)
├── stress_test_debug.log         (~1 MB)
└── pg_stat_statements.log        (~2 KB)
```

## Using with AI

```bash
# Option 1: Attach to AI conversation
# Upload all logs/ files + ai_prompts/performance-analysis.md

# Option 2: Inline (for small logs)
cat ai_prompts/performance-analysis.md
echo "---"
echo "Debug logs:"
cat logs/*.log
```

## Quick Analysis (Without AI)

```bash
# Network RTT samples
grep "elapsed=" logs/stress_test_debug.log | head -20

# Server-side timing
cat logs/pg_stat_statements.log

# Test duration
grep "test result:" logs/*_debug.log
```

## Cleanup

```bash
# Remove old logs
rm -rf logs/

# Remove test schemas
./scripts/cleanup_test_schemas.sh
```

## Troubleshooting

**Problem**: No data in pg_stat_statements

**Solution**: 
```bash
# Check if extension is enabled
psql $DATABASE_URL -c "SELECT COUNT(*) FROM pg_stat_statements;"

# Check tracking setting
psql $DATABASE_URL -c "SHOW pg_stat_statements.track;"

# Should be 'all', not 'none'
```

**Problem**: Debug logs are huge (>10 MB)

**Solution**: Use grep to extract relevant portions:
```bash
# Extract only timing lines
grep -E "(elapsed=|duration_ms=)" logs/stress_test_debug.log > logs/timing_only.log
```

## Time Estimates

| Task | Time |
|------|------|
| Setup | 2 min |
| Basic tests | 1 min |
| Validation tests | 7 min |
| E2E tests | 3 min |
| Stress tests | 3 min |
| Collect stats | 1 min |
| **Total** | **~17 min** |

## Next Steps

After collecting data:
1. Open `ai_prompts/performance-analysis.md`
2. Follow the "Analysis Instructions" section
3. Provide logs to AI assistant
4. Review generated analysis report
5. Implement top recommendations
6. Re-run to measure improvement

