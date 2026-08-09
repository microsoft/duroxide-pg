# The cost of forcing a custom query plan

This document measures what it costs to fix the generic-plan problem described
in `fetch-work-item-generic-plan.md`.

It uses simple technical English. **Future-visible rows** are rows whose
`visible_at` is still in the future.

## 1. Short answer

The obvious fix is to force PostgreSQL to build a fresh plan on every call,
using `EXECUTE ... USING`. This removes the slow scan.

But it makes the common case much slower. On a queue with no future-visible
backlog it adds **60 microseconds** to a call that took 10 microseconds, and
it reduces dequeue throughput by **16.5% to 20.7%**.

So `EXECUTE ... USING` is **not** a safe unconditional fix.

A better fix was found. Adding a redundant `statement_timestamp()` bound keeps
the cached plan and stays fast at every backlog size, with no measured cost.

## 2. Why forcing a custom plan is not free

PostgreSQL normally caches a query plan and reuses it. Reusing a cached plan
costs about **2 microseconds**. Building a fresh plan costs about
**34 microseconds**.

`EXECUTE ... USING` builds a fresh plan every single time. On a fast call that
overhead dominates.

## 3. Latency by backlog size

Variant A is the shipped function today. Variant B uses `EXECUTE ... USING`.

| Future-visible rows | A: current | B: forced custom | Difference |
|---:|---:|---:|---|
| 0 | 10 us | 70 us | **+60 us (+600%)** |
| 1 | 11 us | 67 us | +56 us (+509%) |
| 5 | 13 us | 71 us | +58 us (+446%) |
| 10 | 15 us | 72 us | +57 us (+380%) |
| 50 | 31 us | 75 us | +44 us (+142%) |
| 100 | 52 us | 80 us | +28 us (+54%) |
| 500 | 221 us | 79 us | -142 us (-64%) |
| 1,000 | 434 us | 79 us | -355 us (-82%) |
| 10,000 | 4,273 us | 80 us | -4,193 us (-98%) |
| 100,000 | 43,380 us | 80 us | -43,300 us (-99.8%) |

Variant B is nearly flat at about 80 microseconds at every size. Variant A is
faster when the backlog is small and much slower when it is large.

## 4. The crossover point

**190 future-visible rows.**

The two are tied at 185 rows, at 88 microseconds each. At 190 rows variant B
becomes faster, 88 versus 90 microseconds.

Below 190 future-visible rows, the fix costs more than it saves. Above it, the
fix wins, and the margin grows without limit.

## 5. Throughput, which matters more

Single-call latency understates the problem, because planning burns CPU.

With **no** future-visible backlog, the worst case for the fix:

| Clients | A: current | B: forced custom | Change |
|---:|---:|---:|---|
| 1 | 2,425.2/s | 1,922.5/s | **-20.7%** |
| 8 | 6,566.5/s | 5,391.1/s | **-17.9%** |
| 32 | 5,883.4/s | 4,912.6/s | **-16.5%** |
| 64 | 4,591.5/s | 4,283.0/s | -6.7% |

With a small backlog of 100 future-visible rows:

| Clients | A: current | B: forced custom | Change |
|---:|---:|---:|---|
| 1 | 2,126.0/s | 1,916.7/s | -9.8% |
| 8 | 5,935.5/s | 5,481.9/s | -7.6% |
| 32 | 5,256.3/s | 4,968.3/s | -5.5% |
| 64 | 4,203.2/s | 4,164.9/s | -0.9% |

A healthy queue would lose roughly a sixth to a fifth of its dequeue rate.
That is a large price to pay for protection against a backlog many deployments
will never build up.

## 6. A better fix

Keep the cached plan. Add a second, redundant bound to the same predicate:

```sql
AND q.visible_at <= statement_timestamp()
```

The caller's timestamp predicate stays. This adds a bound the generic planner
can estimate, so the planner can use the existing
`idx_worker_visible(visible_at, lock_token)` index without re-planning.

Measured latency:

| Future-visible rows | p50 |
|---:|---:|
| 0 | 10 us |
| 1 | 11 us |
| 5 | 11 us |
| 10 | 12 us |
| 50 | 14 us |
| 100 | 12 us |
| 500 | 12 us |
| 1,000 | 12 us |
| 10,000 | 12 us |
| 100,000 | 12 us |

The curve is flat. It matches the current fast path at zero backlog and does
not degrade at 100,000 rows. The generic plan executed in 0.022 ms at 100,000
future rows.

Throughput was at or above the current behavior: 2,368.8/s at 1 client and
7,033.1/s at 8 clients with no backlog; 2,306.6/s and 6,712.7/s with a backlog
of 100.

**This fix has a condition.** It is correct only if the caller's `p_now_ms` is
never later than the server's `statement_timestamp()`. The worker supplies
`p_now_ms` from its own clock. A worker whose clock runs ahead of the database
would have rows wrongly excluded from the result.

This must be proven or guarded before the fix is adopted. It is the same
worker-clock-versus-database-clock question that appears elsewhere in this
code.

## 7. Other approaches that did not work

| Approach | Result |
|---|---|
| Index on `(visible_at, id)` | Generic plan still scanned 100,000 rows, about 45.8 ms |
| Partial index on id | Same, no improvement |
| Remove `ORDER BY`, probe for any visible row | Chose a sequential scan, about 38.1 ms |
| `ORDER BY visible_at, id` | Fast at about 0.028 ms, but changes FIFO ordering |

The last one is worth noting. It is fast, but it changes delivery order, so it
is a semantic change rather than a pure optimisation.

## 8. Test setup

- PostgreSQL 16.14 (Debian 16.14-1.pgdg13+1), 64-bit
- Container with 8 CPU quota, 4 visible processors, 8 GiB memory
- `shared_buffers=128MB`, `effective_cache_size=4GB`,
  `random_page_cost=4`, `cpu_tuple_cost=0.01`, JIT on,
  `max_connections=100`
- Source: `microsoft/duroxide-pg` commit
  `26fd4a37019a72e2b8ce2905fc97479061131d57`, migrations 0001 through 0021
  applied unmodified in order
- Plan mode `auto` for variants A and B; `force_custom_plan` used as a
  cross-check
- The function was called more than five times before measuring, so variant A
  was genuinely on its cached generic plan
- Throughput measured with `pgbench`, two 15-second replicates

## 9. Limits of this test

- Latency was measured server side, so it excludes network and client driver
  time.
- PostgreSQL and `pgbench` shared one container, so reported CPU covers both.
- Throughput used two 15-second replicates. Variance at 64 clients was higher
  than at lower client counts.
- The `EXECUTE` variant must replace the shipped `FOUND` check with an
  explicit `v_id IS NULL` test, because dynamic `EXECUTE` does not set the
  PL/pgSQL `FOUND` variable. This is a required change, not an optional one.
- The `statement_timestamp()` fix needs its clock assumption proven before
  adoption. See section 6.
- This ran on local container disk, not on Azure Database for PostgreSQL
  Flexible Server storage. Absolute values will differ there. Comparisons
  between variants used the same server, data, and warm cache.

## 10. Recommendation

Do not adopt `EXECUTE ... USING` unconditionally. It protects against a large
future-visible backlog but taxes every healthy queue by 16% to 20% of its
dequeue rate.

The `statement_timestamp()` bound is the better candidate. It is fast at every
backlog size and costs nothing measurable. Its clock assumption must be proven
first.
