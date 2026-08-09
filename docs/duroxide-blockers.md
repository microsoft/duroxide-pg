# Duroxide Upstream Blockers & Dependencies

**Purpose:** Track duroxide issues/limitations that require workarounds in duroxide-pg.

**Last Updated:** 2025-01-03

---

## How to Check for Fixes

1. **Check duroxide releases:**
   ```bash
   gh release list --repo microsoft/duroxide --limit 10
   ```

2. **Check specific issue status:**
   ```bash
   gh issue view <ISSUE_NUMBER> --repo microsoft/duroxide
   ```

3. **Check current duroxide version in use:**
   ```bash
   grep 'duroxide = ' Cargo.toml
   ```

4. **After duroxide update, search for STOPGAP markers:**
   ```bash
   grep -rn "STOPGAP\|BLOCKED on duroxide\|TODO.*duroxide fix" --include="*.rs" .
   ```

---

## Active Blockers

### 1. Configurable Short-Poll Timeout Threshold

| Field | Value |
|-------|-------|
| **Issue** | [GitHub #51](https://github.com/microsoft/duroxide/issues/51) |
| **Status** | 🔴 Open |
| **Fixed In** | TBD |
| **Workaround Location** | `tests/postgres_provider_test.rs` - `long_polling_tests` module |

**Problem:**
The short polling validation tests in `duroxide/src/provider_validation/long_polling.rs` have a hardcoded 100ms threshold. This works for local/low-latency backends like SQLite, but causes flaky test failures for network-based providers like PostgreSQL where:

- Each stored procedure call takes ~70-80ms over network
- Query plan compilation on first execution adds overhead
- Connection pool establishment adds latency

**Current Workaround:**
- Added TWO warmup queries before each short-poll test
- First query primes the connection pool
- Second query warms the query plan cache
- Tests are implemented manually instead of using `provider_validation_test!` macro

**When Fixed - Cleanup Steps:**
1. Update duroxide dependency in `Cargo.toml`
2. Configure `short_poll_threshold()` to ~200ms in `PostgresProviderFactory`
3. Remove warmup queries from both test functions
4. Convert tests to use `provider_validation_test!` macro
5. Remove STOPGAP comments
6. Remove entry from `TODO.md`

**Files to Update:**
- [ ] `tests/postgres_provider_test.rs` (search for `STOPGAP` and `duroxide #51`)
- [ ] `TODO.md` (remove BLOCKED entry)

---

## Resolved Blockers

_None yet. Move items here when fixed._

<!--
Template for resolved blocker:

### [RESOLVED] Issue Title

| Field | Value |
|-------|-------|
| **Issue** | [GitHub #XX](https://github.com/microsoft/duroxide/issues/XX) |
| **Status** | ✅ Resolved |
| **Fixed In** | v0.1.X |
| **Cleanup PR** | [#YY](link) |

**Resolution Date:** YYYY-MM-DD
-->

---

## Checklist After Duroxide Update

When updating the duroxide dependency, run through this checklist:

1. [ ] Check if any issues in "Active Blockers" are fixed in the new version
2. [ ] Run `grep -rn "STOPGAP\|BLOCKED on duroxide" --include="*.rs" .` to find workarounds
3. [ ] For each fixed issue:
   - [ ] Remove the workaround code
   - [ ] Run the affected tests to confirm fix
   - [ ] Move the blocker to "Resolved Blockers" section
   - [ ] Update `TODO.md`
4. [ ] Run full test suite: `cargo nextest run`
5. [ ] Run stress tests if applicable
6. [ ] Update this document with new status

---

## Version Compatibility Matrix

| duroxide-pg | duroxide | Notes |
|-------------|----------|-------|
| 0.1.8 | 0.1.11 | Current - has workaround for #51 |
