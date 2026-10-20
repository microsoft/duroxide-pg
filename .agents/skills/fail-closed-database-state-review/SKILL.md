---
name: fail-closed-database-state-review
description: Review database-backed provider and migration changes for errors that are converted into absent, empty, zero, or success-shaped state and can permit writes, replay, or destructive repair.
---

# Fail-Closed Database State Review

Use this skill for changes that read PostgreSQL catalog state, migration
history, stored-procedure results, durable history, queue state, or serialized
provider payloads.

## Required Invariant

An error is not evidence that state is absent.

Do not convert a failed database query or decode into:

- `false`, `None`, an empty collection, or a zero count;
- a missing migration, table, queue item, history entry, or KV entry;
- permission to create, delete, re-apply, repair, acknowledge, prune, or replay;
- a successful provider result.

Return the original error with operation and state context. If the database
result is structurally invalid, return a permanent provider error that
identifies the invalid field or key.

## Review Procedure

1. Search production Rust and SQL for success-shaped fallbacks:

   ```bash
   rg -n 'unwrap_or\(false\)|unwrap_or_default\(\)|unwrap_or\(0|unwrap_or\(\(0|filter_map\(|\.ok\(\)|let _ = ' src migrations
   rg -n 'fetch_optional|row\.and_then|history_error|check_.*await|DELETE FROM .*_duroxide_migrations|re-?apply' src migrations
   ```

2. Trace each fallback to its next side effect. Flag it when the fallback can:
   - enter a create or repair path;
   - delete or rewrite migration records;
   - execute DDL or replace stored procedures;
   - acknowledge, delete, or prune durable state;
   - return replay input after dropping malformed data.

3. For migrations, require all of these:
   - catalog query errors propagate;
   - applied versions are an ordered prefix of bundled migrations;
   - unknown versions, interior holes, and missing core tables stop before DDL;
   - required objects are verified as base-table relation kinds, not by name alone;
   - the runner never deletes applied migration rows to trigger a re-run;
   - repair is explicit and separate from normal startup.

4. For serialized provider state, require all of these:
   - the root JSON type is checked;
   - every entry is decoded strictly;
   - required fields and their types are enforced;
   - one malformed entry fails the whole operation;
   - errors identify the operation and affected key or field.

5. Inspect cleanup and lock-release code separately. A deliberately
   best-effort action must explain why suppressing its error cannot retain a
   lock, return a poisoned connection to a pool, or report a successful write.

## Regression Proof

Require the same test code to show red before the fix and green after it.
Each affected path needs:

- a positive control with valid state;
- a malformed, missing, or error-producing input;
- an assertion on the returned error;
- a before/after snapshot of every ledger or durable object that must not
  change;
- a behavior assertion, not only a log or string check.

For migration safety, compare `_duroxide_migrations` rows and
`pg_get_functiondef()` before and after the rejected startup. For provider
payloads, prove malformed input does not become empty successful state.

Use a fresh `CARGO_TARGET_DIR` keyed by the source and diff digest so embedded
migrations and SQL-only changes cannot reuse a stale binary. Run the targeted
regressions before the full repository suite.

## Review Output

Report each finding with:

| Field | Required content |
|---|---|
| Location | File and line |
| Fallback | The exact error-to-state conversion |
| Side effect | The write, replay, or success result it enables |
| Severity | Blocker, high, medium, or low |
| Proof | Red test and immutable before/after state |
| Fix | Error propagation or explicit validation |

Do not approve while a database or decode error can become absent state on a
write, migration, acknowledgement, pruning, or replay path introduced or
changed by the candidate. Record pre-existing matches as follow-up debt with
the same evidence, but do not silently expand a scoped patch unless they share
the candidate's execution path or make its safety claim false.
