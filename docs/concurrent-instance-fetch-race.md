# Concurrent instance fetch race in `duroxide-pg`

## Summary

`duroxide-pg` has a race in `fetch_orchestration_item` under concurrent dispatchers.

Symptom:
- `fetch_orchestration_item` can return `None` even though eligible work still exists.
- This causes the provider validation `instance_locking_tests::test_concurrent_instance_fetching` to fail intermittently.

Observed reproduction:
- Ran `cargo nextest run instance_locking_tests::test_concurrent_instance_fetching` repeatedly in `providers/duroxide-pg`
- It reproduced on run 5/10
- Failure shape:
  - expected 10 unique instances fetched
  - got 8

## Failing validation

The failure comes from the shared validation in `duroxide`:
- `instance_locking_tests::test_concurrent_instance_fetching`

What the test does:
1. Enqueue 10 distinct orchestration start items
2. Spawn 10 concurrent fetchers
3. Expect each instance to be fetched exactly once

Intermittent failure means one or more fetchers returned `None` too early instead of claiming a different ready instance.

## Root cause

The stored procedure `fetch_orchestration_item` currently has this shape:

1. Select one candidate instance from `orchestrator_queue`
2. Acquire an instance-level lock for that candidate
3. Re-verify the candidate is still eligible
4. If re-check fails, `RETURN`
5. If lock upsert affects 0 rows, `RETURN`

That is incorrect under contention.

### Race window

Two or more fetchers can do this concurrently:

- Fetcher A selects instance `inst-0`
- Fetcher B also selects `inst-0`
- Fetcher A wins the lock
- Fetcher B loses during the re-check or lock acquisition step
- Fetcher B returns `NULL`

But at that point there may still be many other eligible instances (`inst-1` through `inst-9`).

So `NULL` does **not** mean "no work available".
It only means "the candidate I chose was taken by someone else".

## Why this matters

Returning `None` on lost-candidate contention causes:
- missed work in short-poll paths
- flaky validation failures
- unfairness under load
- unnecessary idle cycles in dispatchers

## Affected implementation

The problematic logic is in the PostgreSQL migrations that define `fetch_orchestration_item`, especially the current version inherited through:
- `migrations/0013_add_capability_filtering.sql`
- later migrations that preserve the same fetch semantics

The critical pattern is:

- candidate select
- re-check with `FOR UPDATE`
- `IF NOT FOUND THEN RETURN;`
- instance lock upsert
- `IF v_lock_acquired = 0 THEN RETURN;`

Those `RETURN`s should not terminate the fetch in the contention case.

## Proposed fix

Update `fetch_orchestration_item` to retry candidate selection **inside the stored procedure** when contention invalidates the chosen candidate.

### Correct behavior

Wrap the candidate-selection / lock-acquisition sequence in a `LOOP`:

- select candidate
- if no candidate exists, `RETURN`
- try to lock that candidate
- if the candidate was lost during re-check, `CONTINUE`
- if instance lock acquisition affected 0 rows, `CONTINUE`
- if lock succeeds, return the locked batch

### Important distinction

The loop is **not** long-polling.
It only retries when:
- the chosen candidate was lost to another concurrent fetcher, or
- the instance lock was not acquired because another fetcher won the race

It exits when:
- there is truly no candidate left, or
- a candidate is successfully locked and returned

## Why this is the right fix

This preserves the intended meaning of `None`:
- `None` should mean **no eligible work exists now**
- not **I lost a race on one candidate**

It also matches the behavior a dispatcher expects under contention:
- if one ready instance is lost, keep searching for another ready instance in the same fetch call

## Suggested implementation plan

1. Add a new delta migration in `duroxide-pg`
   - do **not** modify the baseline migration
   - create a new migration, likely `0017_...sql`
2. Replace the current `fetch_orchestration_item` body with a looping version
3. Add the required companion diff doc
4. Re-run:
   - `cargo nextest run instance_locking_tests::test_concurrent_instance_fetching`
   - repeated loop, e.g. 10x
   - then full `cargo nextest run`

## Reference behavior

A working version of this fix was already applied in `duroxide-pg-opt` as a dedicated migration that changes `fetch_orchestration_item` to:
- `LOOP`
- `CONTINUE` on lost candidate
- `CONTINUE` on lock acquisition failure
- `RETURN` only on true empty queue or success

## Non-goals

This document is only about the concurrent orchestration-fetch race.
It does **not** require:
- changing worker fetch logic
- changing runtime polling behavior
- changing validation tests

## Acceptance criteria

The fix is complete when:
- repeated runs of `instance_locking_tests::test_concurrent_instance_fetching` stop failing
- full parallel `cargo nextest run` is stable
- `fetch_orchestration_item` only returns `None` when no eligible orchestration work exists
