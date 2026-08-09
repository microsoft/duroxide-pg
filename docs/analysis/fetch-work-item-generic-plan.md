# `fetch_work_item` uses a generic plan and scans the whole future-visible backlog

This document reports a verified performance defect in `fetch_work_item` in
`microsoft/duroxide-pg`.

It uses simple technical English. It uses the term **locked rows** for rows a
worker currently holds, and **future-visible rows** for rows whose
`visible_at` is still in the future.

## 1. Short answer

`fetch_work_item` gets slower in direct proportion to the number of
future-visible rows in the queue, even though those rows can never be
returned.

The cost is not caused by a missing index. It is caused by PostgreSQL choosing
a generic query plan inside the PL/pgSQL function.

This is **not** related to PR #18 or PR #19. It reproduces on main. Neither PR
changes it.

## 2. The measurement

One row is visible. Everything else has `visible_at` one hour in the future.
Measured time to fetch that one visible row:

| Future-visible rows | p50 | p95 |
|---:|---:|---:|
| 0 | 0.083 ms | 0.118 ms |
| 100 | 0.099 ms | 0.131 ms |
| 1,000 | 0.489 ms | 0.608 ms |
| 10,000 | 4.407 ms | 4.758 ms |
| 100,000 | 43.659 ms | 45.167 ms |
| 1,000,000 | 438.962 ms | 470.135 ms |

The growth is exactly linear. The fit is r-squared = 1.00, at about 0.000439 ms
per future-visible row.

The same query written with literal values takes **0.006 ms** and uses the
existing index `idx_worker_visible(visible_at, lock_token)`.

A mixed case is also slow. With 10,000 future-visible rows and 100 visible
rows, the fetch took 4.413 ms.

The cost passes 10 ms at about **25,000 future-visible rows**. This is not an
extreme scale.

## 3. The cause

PL/pgSQL caches query plans. After five executions it may switch from a custom
plan, built for the actual parameter values, to a generic plan built once for
all values.

The generic plan does not know how selective `visible_at <= now` is. It sees
`ORDER BY id LIMIT 1` and expects to find a matching row early. So it scans
`worker_queue_pkey` in id order and filters as it goes.

When future-visible rows come first in id order, this filters the entire
backlog before it reaches the visible row. At 1,000,000 rows this touched
13,046 buffers and rejected 1,000,000 rows.

Setting the plan mode proves it:

| Plan mode | p50 |
|---|---:|
| `auto` (current behavior) | 439.729 ms |
| `force_generic_plan` | 445.848 ms |
| `force_custom_plan` | **0.332 ms** |

The custom plan picks `idx_worker_visible` and executes in 0.042 ms.

## 4. What is not the cause

Several plausible explanations were tested and ruled out.

- **The `TO_TIMESTAMP(p_now_ms / 1000.0)` expression.** This is indexable. A
  custom plan uses the index with this expression present. The expression does
  add per-row CPU cost once the generic scan is chosen, but it is not what
  blocks the index.
- **A missing index.** Adding `(visible_at, id)` had no effect at all
  (439.806 ms). The generic plan still prefers the primary key scan.
- **The `LEFT JOIN sessions`.** With the join, 434.22 ms. No material
  difference.
- **The tag filter branches.** The no-tag case was 451.569 ms. No material
  difference.
- **`FOR UPDATE OF q SKIP LOCKED`.** Removing it gave 444.321 ms. No material
  difference.

Every factor except plan caching was ruled out by measurement.

## 5. Effect of the open pull requests

Neither open PR changes this.

| Variant | p50 | p95 |
|---|---:|---:|
| main | 439.729 ms | 467.093 ms |
| PR #18 (`574cf3f5`) | 439.880 ms | 450.947 ms |
| PR #19 (`46a8bf1d`) | 438.556 ms | 466.076 ms |

PR #18 does not help because its phase 1 scans the new `idx_worker_ready`
index and still filters all 1,000,000 rows. PR #19 only makes the session,
tag, and orchestrator indexes partial; it does not touch `idx_worker_visible`
or `fetch_work_item`.

## 6. Possible fixes, measured

At 1,000,000 future-visible rows:

| Approach | p50 | Works? |
|---|---:|---|
| `EXECUTE ... USING` for the dequeue SELECT | 0.378 ms | Yes |
| `SET plan_cache_mode = force_custom_plan` | 0.332 ms | Yes |
| Compute the timestamp into a local variable first | 119.520 ms | Partial |
| Add a `(visible_at, id)` index | 439.806 ms | No |

Forcing a custom plan works. Both routes to it give roughly the same result.

**Important caution.** Forcing a custom plan means PostgreSQL re-plans the
query on every call. Planning is not free. If the future-visible backlog is
normally small or zero, this fix may cost more than it saves. That trade is
measured separately in `custom-plan-cost.md`.

## 7. Test setup

- PostgreSQL 16.14, container with 4 vCPU and 8 GiB limits
- All 21 migrations from `microsoft/duroxide-pg` main were applied
  **unmodified**, `0001_initial_schema.sql` through
  `0021_add_get_instance_stats.sql`. The shipped `fetch_work_item` was used,
  not a reproduction.
- Source commit: `26fd4a37019a72e2b8ce2905fc97479061131d57`
- PR #18 head: `574cf3f5c77ac6c0b564fe54a0c3f749ea1c604e`
- PR #19 head: `46a8bf1deb127b6e10fe2d5ae50a068ff8cd4162`
- Warm cache, single client, 7 warmup calls then 25 to 30 measured samples.
  The function was called more than five times before measuring, so the
  generic plan was in effect. This matches steady-state production behavior.

## 8. Limits of this test

- **Worst-case row order.** All future-visible rows were given lower ids than
  the single visible row. This is the worst arrangement for the generic plan.
  A real queue where visible rows appear early in id order will suffer less.
  The linear growth still applies to whatever portion of the backlog precedes
  the first visible row.
- Timings were taken server side with `clock_timestamp`, so they exclude
  client and network time.
- `EXPLAIN` instrumentation itself is measurable at these speeds: the literal
  query measured 0.114 ms under `EXPLAIN` versus 0.006 ms without it.
- The headline case passes `p_owner_id` as NULL, so the function takes its
  non-session branch. A separate probe with an owner and the sessions join
  showed no material difference.
- This ran on local container disk, not on Azure Database for PostgreSQL
  Flexible Server storage. Absolute values will differ there. The comparison
  between variants used the same server, data, and warm cache, so the relative
  results hold.

## 9. Recommendation

This is worth fixing, and it is independent of both open pull requests.

The effect starts at a realistic backlog size, around 25,000 future-visible
rows for a 10 ms cost, and grows without limit from there. Any workload that
schedules work ahead in time, such as timers, retries with backoff, or delayed
messages, will accumulate exactly this kind of backlog.

Before adopting `EXECUTE ... USING`, see `custom-plan-cost.md` for the cost of
re-planning on a queue with little or no future-visible backlog, since that is
the common case.
