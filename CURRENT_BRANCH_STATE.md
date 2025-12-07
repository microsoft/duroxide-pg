# Current Branch State

**Last Updated:** December 2024  
**Status:** Code Review in Progress  
**Branch Purpose:** Optimized Polling via PostgreSQL LISTEN/NOTIFY

---

## Where We Are

You are **code reviewing** this branch. All implementation is complete and tests pass locally.

---

## Key Deltas from `main`

### 1. New Long-Poll Infrastructure (`src/long_poll.rs`)

A complete push-based work detection system using PostgreSQL LISTEN/NOTIFY:

- **`LongPollConfig`** - Configuration struct (enabled, fallback_poll_interval, etc.)
- **`LongPollState`** - Shared state with wake channels, atomic timers, cancellation token
- **`WakeToken`** - Token sent through competing consumer channels
- **Listener Task** - Background task that receives NOTIFY and distributes tokens
- **Aggressive Mode** - State machine for fast-polling after wake events

### 2. Provider Changes (`src/provider.rs`)

- `fetch_orchestration_item()` and `fetch_work_item()` now block in long-poll mode
- Immediate fetch → check aggressive mode → long-poll wait → fetch after wake
- Cancellation support for graceful shutdown

### 3. Database Triggers (`migrations/0003_listen_notify.sql`)

- `notify_orch_work()` - Fires NOTIFY with `visible_at` timestamp on orchestrator queue insert
- `notify_worker_work()` - Fires NOTIFY on worker queue insert
- Schema-aware channel names (`{schema}_orch_work`, `{schema}_worker_work`)

### 4. New Dependencies (`Cargo.toml`)

- `async-channel = "2.3"` - Competing consumer channels
- `tokio-util = "0.7"` - CancellationToken for graceful shutdown

### 5. Test Updates

- `tests/long_poll_tests.rs` - 16 new tests for long-poll behavior
- `tests/e2e_samples.rs` - Now uses long-poll by default
- `tests/e2e_samples_long_poll_disabled.rs` - Sanity tests with short-poll
- `tests/common/mod.rs` - New helpers (`create_provider_with_schema_no_long_poll`, etc.)

---

## Documentation to Read

| Document | Path | Description |
|----------|------|-------------|
| **Architecture** | `docs/long_poll_architecture.md` | Diagrams and component overview |
| **Full Design** | `docs/OPTIMIZED_POLLING.md` | Comprehensive design doc |
| **Validation Issue** | `docs/PROVIDER_VALIDATION_LONG_POLL.md` | Why validation tests use short-poll |

---

## Known Issues / TODO

### 1. Provider Validation Tests Use Short-Poll

**Issue:** The duroxide framework's provider validation tests (`postgres_provider_test.rs`) expect `fetch_*` methods to return immediately when no work is available. With long-poll enabled, these tests timeout.

**Current Workaround:** Validation tests run with `enabled: false` (short-poll).

**Proposed Fix:** See `docs/PROVIDER_VALIDATION_LONG_POLL.md` for 4 approaches. Recommended: Add `max_wait` parameter to `Provider` trait in duroxide core.

**Action Required:** After duroxide core fix, update `postgres_provider_test.rs` to use long-poll.

### 2. All Tests Should Run in Long-Poll Mode

After the duroxide core fix for validation tests, **all** tests should default to long-poll:

- [ ] `postgres_provider_test.rs` - Currently uses short-poll (blocked by above issue)
- [x] `e2e_samples.rs` - Uses long-poll ✓
- [x] `long_poll_tests.rs` - Uses long-poll ✓
- [x] `basic_tests.rs` - Uses long-poll via `create_postgres_store()` ✓

### 3. Stress & Performance Testing Needed

**Goal:** Measure the actual improvement in database query load.

**Metrics to capture:**
- Poll frequency before (short-poll) vs after (long-poll)
- Queries per second at idle
- Latency from work insertion to detection
- Behavior under load (many orchestrations)

**Suggested approach:**
1. Run `pg-stress` tests with short-poll, capture PG query logs
2. Run same tests with long-poll, capture PG query logs
3. Compare idle polling frequency (should be ~99% reduction)
4. Compare work detection latency (should be similar or better)

---

## Test Status

```
cargo test
```

| Test Suite | Status | Mode |
|------------|--------|------|
| `basic_tests.rs` | ✅ 9 pass | Long-poll |
| `e2e_samples.rs` | ✅ 25 pass | Long-poll |
| `e2e_samples_long_poll_disabled.rs` | ✅ 25 pass | Short-poll |
| `long_poll_tests.rs` | ✅ 16 pass | Mixed |
| `postgres_provider_test.rs` | ✅ 50 pass | Short-poll* |
| Stress tests | ⏭️ Ignored | - |
| Doc tests | ✅ 5 pass | - |

*Blocked by duroxide core issue

---

## Quick Start for Review

```bash
# Run all tests
cargo test

# Run just long-poll tests
cargo test --test long_poll_tests

# Check compilation
cargo check

# Read the architecture
cat docs/long_poll_architecture.md
```

---

## Files Changed (Summary)

```
src/
├── lib.rs              # Exports LongPollConfig
├── long_poll.rs        # NEW: All long-poll infrastructure
└── provider.rs         # Modified fetch_* methods

migrations/
└── 0003_listen_notify.sql  # NEW: NOTIFY triggers

tests/
├── common/mod.rs       # New helpers
├── long_poll_tests.rs  # NEW: Long-poll specific tests
├── e2e_samples.rs      # Uses long-poll now
├── e2e_samples_long_poll_disabled.rs  # NEW: Short-poll sanity
└── postgres_provider_test.rs  # Uses short-poll (blocked)

docs/
├── long_poll_architecture.md       # NEW: Architecture diagrams
├── OPTIMIZED_POLLING.md            # NEW: Full design doc
└── PROVIDER_VALIDATION_LONG_POLL.md # NEW: Validation test issue

Cargo.toml              # New deps: async-channel, tokio-util
```

