# Duroxide Upstream Blockers & Dependencies

**Purpose:** Track duroxide issues/limitations that require workarounds in duroxide-pg-opt.

**Last Updated:** 2024-12-22

---

## How to Check for Fixes

1. **Check duroxide releases:**
   ```bash
   gh release list --repo affandar/duroxide --limit 10
   ```

2. **Check specific issue status:**
   ```bash
   gh issue view <ISSUE_NUMBER> --repo affandar/duroxide
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

### 1. Configurable `wait_for_orchestration` Timeout

| Field | Value |
|-------|-------|
| **Issue** | [GitHub #31](https://github.com/affandar/duroxide/issues/31) |
| **Status** | 🔴 Open |
| **Fixed In** | TBD |
| **Workaround Location** | `pg-stress/src/lib.rs` - `run_large_payload_suite()` |

**Problem:**
The stress test framework has a hardcoded 60-second timeout for `wait_for_orchestration` in `duroxide/src/provider_stress_test/core.rs:177`. This causes large payload tests to fail on remote databases with high latency (200-300ms per query).

**Current Workaround:**
- `is_localhost_db()` function detects local vs remote databases
- Remote databases use reduced test intensity:
  - Smaller payloads (5/20/50 KB instead of 10/50/100 KB)
  - Fewer activities (8 instead of 20)
  - Fewer sub-orchestrations (2 instead of 5)

**When Fixed - Cleanup Steps:**
1. Update duroxide dependency in `Cargo.toml`
2. Remove `is_localhost_db()` function from `pg-stress/src/lib.rs`
3. Remove conditional config in `run_large_payload_suite()`
4. Use full intensity config for all databases with increased timeout
5. Remove STOPGAP comments
6. Remove entry from `TODO.md`

**Files to Update:**
- [ ] `pg-stress/src/lib.rs` (search for `STOPGAP`)
- [ ] `TODO.md` (remove BLOCKED entry)

---

### 2. Validation Test Timing Race with Connection Latency

| Field | Value |
|-------|-------|
| **Issue** | [GitHub #32](https://github.com/affandar/duroxide/issues/32) |
| **Status** | 🔴 Open |
| **Fixed In** | TBD |
| **Workaround Location** | `src/provider.rs` - `new_with_options()` and `new_with_fault_injection()` |

**Problem:**
The `test_multi_threaded_lock_expiration_recovery` validation test in `duroxide/src/provider_validation/instance_locking.rs` has a timing race condition. The test spawns 3 threads that start their sleep timers at spawn time, but thread 1 may have connection establishment latency before acquiring the lock. This causes thread 2 and thread 3 to wake up at the wrong time relative to when the lock was actually acquired.

**Root Cause:**
- Thread 1: Fetches lock (may have 50-200ms connection latency)
- Thread 2: Sleeps 200ms from spawn (may wake BEFORE thread 1 acquires lock)
- Thread 3: Sleeps `lock_timeout+100ms` from spawn (may wake before lock actually expires)

**Proposed Fix:**
Use a oneshot channel to signal when thread 1 has acquired the lock, then have thread 2 and thread 3 start their timers from that point.

**Current Workaround:**
- Pre-warm 4 additional connections when long-poll is enabled
- Connections are acquired and immediately dropped to warm the pool
- This ensures the connection pool is ready before tests run

**When Fixed - Cleanup Steps:**
1. Update duroxide dependency in `Cargo.toml`
2. Verify the validation test passes without pre-warming
3. Remove pre-warming code blocks in both constructors
4. Remove TODO comments

**Files to Update:**
- [ ] `src/provider.rs` (search for "pre-warming")

---

## Resolved Blockers

_None yet. Move items here when fixed._

<!--
Template for resolved blocker:

### [RESOLVED] Issue Title

| Field | Value |
|-------|-------|
| **Issue** | [GitHub #XX](https://github.com/affandar/duroxide/issues/XX) |
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
4. [ ] Run full test suite: `cargo test`
5. [ ] Run stress tests: `./scripts/run-stress-tests.sh`
6. [ ] Update this document with new status

---

## Version Compatibility Matrix

| duroxide-pg-opt | duroxide | Notes |
|-----------------|----------|-------|
| 0.1.1 | 0.1.6 | Current - has workarounds for #31 |

