# TODO

- fetch_orchestration_item review as listed below
- remove all dead code masked by allow dead code
- **BLOCKED on duroxide #51**: Short-poll validation tests use warmup workaround due to hardcoded 100ms threshold in `duroxide/src/provider_validation/long_polling.rs`. See [GitHub issue #51](https://github.com/microsoft/duroxide/issues/51). Once fixed, remove warmup queries from `tests/postgres_provider_test.rs` and configure `short_poll_threshold()`.

## fetch_orchestration_item Stored Procedure Efficiency Review

The `fetch_orchestration_item` stored procedure in `migrations/0002_create_stored_procedures.sql` 
was modified to fix a deadlock issue (reported by pg_durable team, Dec 2025).

**Current approach: Two-phase locking**
1. Phase 1: Peek query (no locks) to find candidate instance
2. Phase 2: Acquire advisory lock on that instance
3. Phase 3: Re-verify with FOR UPDATE

**Review needed:**
- [ ] The two-phase approach adds an extra SELECT query per fetch
- [ ] Consider if the peek query can be optimized (fewer columns, simpler WHERE)
- [ ] Evaluate if Phase 3 re-verification can be combined with subsequent operations
- [ ] Benchmark under load to measure actual overhead vs the old single-query approach
- [ ] Consider adding an index hint or query plan analysis

**Context:**
- The fix prevents B-tree index deadlocks that occurred with INSERT ... ON CONFLICT
- Instance-level advisory locks preserve parallelism across different instances
- Retry logic was also added in the Rust provider as a safety net

