# Does PR #18 make dequeue faster? A measurement

This document reports a performance measurement of pull request #18 in the
`microsoft/duroxide-pg` repository. PR #18 changes how a worker takes the next
item from the `worker_queue` table.

The document uses simple technical English. It uses the term **locked rows**
for rows that a worker currently holds.

## 1. Short answer

**The change helps only when there are far more locked rows than a real
deployment has.**

At a realistic number of locked rows, the change is neutral. In some cases it
is slightly slower. It also adds a cost to every enqueue.

## 2. What the change does

Before the change, one query finds the next item. A row can be taken if no
worker holds it, or if the holder's lease has run out:

```sql
WHERE q.visible_at <= now
  AND (q.lock_token IS NULL OR q.locked_until <= p_now_ms)
  AND (q.session_id IS NULL OR s.worker_id = p_owner_id OR s.session_id IS NULL)
ORDER BY q.id LIMIT 1 FOR UPDATE OF q SKIP LOCKED
```

This is correct, but it can be slow. PostgreSQL must read and reject locked
rows before it reaches a free row.

After the change, there are two steps.

Step 1 looks only for rows that no worker holds:

```sql
WHERE q.visible_at <= now AND q.lock_token IS NULL
ORDER BY q.id LIMIT 1 FOR UPDATE SKIP LOCKED
```

A new index serves this step:

```sql
CREATE INDEX idx_worker_ready ON worker_queue (id) WHERE lock_token IS NULL;
```

Locked rows are not in this index at all. So PostgreSQL does not read them.

Step 2 is the original query. It runs only if step 1 found nothing.

## 3. The key question

The new index saves work by skipping locked rows. So the benefit depends on
**how many locked rows exist**.

A row is locked only while a worker is processing it. So the number of locked
rows is limited by the number of busy worker slots.

In `microsoft/duroxide`, `worker_concurrency` defaults to **2** slots per
runtime. That gives this scale:

| Locked rows | Runtimes needed, all slots busy |
|---:|---:|
| 100 | about 50 |
| 1,000 | about 500 |
| 10,000 | about 5,000 |
| 100,000 | about 50,000 |

The worker lock timeout is 30 seconds by default. So rows from crashed workers
add to the locked count for a short time only.

## 4. Result: single fetch latency

Backlog of 100,000 free rows. All locked rows placed **before** the first free
row. This is the best case for the new index.

| Locked rows | Before p50 | After p50 | Gain | Before p95 | After p95 | Gain |
|---:|---:|---:|---:|---:|---:|---:|
| 0 | 356 us | 341 us | 4.2% | 564 us | 497 us | 11.9% |
| 10 | 336 us | 346 us | **-3.0%** | 524 us | 519 us | 1.0% |
| 100 | 351 us | 368 us | **-4.8%** | 489 us | 530 us | -8.4% |
| 1,000 | 463 us | 348 us | 24.8% | 671 us | 530 us | 21.0% |
| 10,000 | 1,629 us | 345 us | 78.8% | 2,441 us | 505 us | 79.3% |
| 100,000 | 13,479 us | 460 us | 96.6% | 17,299 us | 643 us | 96.3% |

The gain is real, but it starts near 1,000 locked rows. Below that the change
measures slightly slower.

Now the same test with locked rows **mixed among** free rows:

| Locked rows | Before p50 | After p50 | Gain |
|---:|---:|---:|---:|
| 0 | 334 us | 343 us | -2.7% |
| 10 | 335 us | 351 us | -4.8% |
| 100 | 346 us | 318 us | 8.1% |
| 1,000 | 340 us | 340 us | 0.0% |
| 10,000 | 355 us | 341 us | 3.9% |
| 100,000 | 444 us | 447 us | -0.7% |

Here there is no gain at any count. The reason is simple: the old query stops
at the first free row. If free rows are mixed in, it stops almost at once.

So the new index helps only when locked rows are both **many** and **grouped
together at the front** of the id order.

## 5. Result: throughput with many workers

This is the number that matters most. Many clients run fetch, then delete, then
enqueue a replacement, against a queue that is kept full.

| Clients | Hold time | Before per second | After per second | Change | Avg locked rows |
|---:|---:|---:|---:|---:|---:|
| 1 | 0 ms | 1,332.2 | 1,343.2 | +0.8% | 0.3 |
| 8 | 0 ms | 3,205.0 | 3,134.5 | -2.2% | 1.9 |
| 32 | 0 ms | 3,110.0 | 3,281.8 | +5.5% | 9.5 |
| 64 | 0 ms | 3,012.8 | 3,015.1 | +0.1% | 20.8 |
| 1 | 5 ms | 165.9 | 166.2 | +0.2% | 0.9 |
| 8 | 5 ms | 1,284.4 | 1,287.8 | +0.3% | 6.7 |
| 32 | 5 ms | 3,121.2 | 3,100.9 | -0.7% | 18.6 |
| 64 | 5 ms | 2,902.3 | 2,940.8 | +1.3% | 28.7 |

The measured number of locked rows stayed between 0.2 and 28.7. That is the
real operating range.

There is no repeatable gain. The single +5.5% value does not repeat at 64
clients or with hold time, and it sits inside the run-to-run spread.

## 6. Result: the cost

The new index must be written on every insert and removed on every claim.

| Operation | Before per second | After per second | Change | Before WAL | After WAL | WAL change |
|---|---:|---:|---:|---:|---:|---:|
| Enqueue | 20,103.1 | 19,033.3 | **-5.3%** | 553.1 B | 619.5 B | **+12.0%** |
| Claim | 11,691.6 | 11,626.9 | -0.6% | 855.4 B | 854.7 B | -0.1% |
| Ack | 16,275.2 | 16,166.5 | -0.7% | 217.3 B | 220.2 B | +1.3% |

Enqueue pays the cost, because it must add an index entry. Claim and ack are
neutral within run variation.

The index takes 22,487,040 bytes for 1,000,001 unlocked rows. That is about
22.5 bytes per row, roughly the size of the primary key index.

## 7. A separate problem found during the test

A different case was tested: 1,000,000 rows that are not yet visible
(`visible_at` one hour in the future), plus one visible row.

| Variant | Function p50 | Plain SQL |
|---|---:|---:|
| Before | 441.105 ms | 0.038 ms |
| After | 436.587 ms | 0.037 ms |

PR #18 does not change this case. But look at the gap. The same query written
as plain SQL takes 0.04 ms. Inside the PL/pgSQL function, with bound
parameters, it takes over 400 ms.

The existing index `idx_worker_visible(visible_at, lock_token)` already serves
the plain query. The function does not use it well. Setting
`plan_cache_mode=force_custom_plan` improved it to about 121 ms, but no
further.

This is a query plan problem, not an index problem. It is a much larger effect
than anything PR #18 changes.

## 8. A behavior change

Step 2 runs only when step 1 finds nothing. Step 1 finds nothing only when the
queue has no free rows.

A row held by a crashed worker keeps a lock token. So step 1 cannot see it.
It becomes visible only in step 2.

This means a row from a crashed worker waits until the queue is empty of free
work. On a busy queue this can be a long time.

## 9. Conclusion

- At a realistic number of locked rows, there is **no measured gain**.
- Every enqueue pays **5.3% throughput and 12% more WAL**.
- The gain needs about **1,000 locked rows grouped at the front** of the id
  order, which needs roughly 500 busy runtimes at default settings.
- The change also delays recovery of rows from crashed workers.

**Recommendation:** do not merge without production data showing at least
about 1,000 locked rows grouped before the first free row.

The 400 ms function plan problem in section 7 looks more valuable to fix.

## 10. Test setup

- PostgreSQL 16.14 (Debian 16.14-1.pgdg13+1), x86_64
- 4 vCPU limit, 4 GiB memory limit, local container disk
- Changed: `shared_buffers=1GB`, `max_connections=200`, `track_io_timing=on`
- Unchanged: `fsync=on`, `synchronous_commit=on`, `wal_compression=off`
- Before: `microsoft/duroxide-pg` main, commit
  `26fd4a37019a72e2b8ce2905fc97479061131d57`, function from
  `migrations/0016_add_activity_tags.sql`
- After: PR #18 head `574cf3f5c77ac6c0b564fe54a0c3f749ea1c604e`,
  `migrations/0022_optimize_worker_dequeue.sql`
- Runtime defaults from `microsoft/duroxide` commit
  `cfe0b8c957ef7ede43c6026ebba0052211de1a49`, `src/runtime/mod.rs`

The before case kept all existing indexes: primary key `(id)`,
`idx_worker_visible(visible_at, lock_token)`,
`idx_worker_available(lock_token, id)`,
`idx_worker_queue_session_id(session_id)`, and `idx_worker_queue_tag(tag)`.
The after case adds only `idx_worker_ready` and the two-step function.

Method:

- The full session-aware function was reproduced, including the sessions join,
  owner checks, all tag modes, lock token generation, the claim update, and
  the session upsert and rollback.
- Latency: one fetch per sample, 50 warmups then 1,000 measured samples per
  cell, warm cache. The cleanup that resets the sampled row runs outside the
  timed interval.
- Throughput: `pgbench -n`, 2 second warmup then 8 seconds measured, two
  paired repeats. A separate connection sampled the locked row count every
  100 ms.
- Write cost: 32 clients, 8 seconds, three paired repeats. WAL measured with
  `pg_wal_lsn_diff`, divided by successful operations.
- Plans: `EXPLAIN (ANALYZE, BUFFERS, WAL, VERBOSE, SETTINGS)` inside a
  transaction that was rolled back.

## 11. Limits of this test

- This ran on local container disk, not on Azure Database for PostgreSQL
  Flexible Server storage. Absolute numbers will differ there. Before and
  after used the same server, same data, warm cache, and paired order, so the
  comparison between them is fair.
- Worker concurrency is configurable. The conclusion changes if production
  data shows 1,000 or more locked rows grouped before the first free row.
- Index build time during migration was not measured. Steady state size and
  write cost were measured.
- Two early result sets were rejected and are not used: one latency loop
  accumulated row versions inside a single transaction, and one throughput
  launcher ran overlapping copies. The accepted files are
  `latency-autocommit.csv`, `throughput-v2.csv`, and `write-cost-v2.csv`.

## 12. Representative query plans

- 100 locked rows at the front: before uses the primary key index, rejects 100
  rows, 5 buffer hits, 0.051 ms. After uses `idx_worker_ready`, 4 buffer hits,
  0.035 ms.
- 100,000 locked rows at the front: before rejects 100,000 rows, 1,611 buffer
  hits, 16.244 ms. After uses `idx_worker_ready`, 4 buffer hits, 0.034 ms.
- 100,000 locked rows mixed in: before rejects 1 row, 5 buffer hits, 0.041 ms.
  After uses 4 buffer hits, 0.034 ms.

---

The rest of this document is raw evidence: full result tables, query plans,
and the exact scripts used.

## Raw accepted latency CSV

```csv
variant,ready_backlog,locked,placement,iterations,p50_us,p95_us,mean_us,min_us,max_us
BEFORE,1000,0,head,1000,271.000,386.000,298.626,219.000,4672.000
AFTER,1000,0,head,1000,258.000,359.000,272.444,220.000,3086.000
BEFORE,1000,0,interleaved,1000,256.000,360.000,272.139,221.000,3201.000
AFTER,1000,0,interleaved,1000,251.000,349.000,268.760,221.000,3056.000
BEFORE,1000,10,head,1000,262.000,368.000,283.608,221.000,3326.000
AFTER,1000,10,head,1000,260.000,346.000,272.499,220.000,3225.000
BEFORE,1000,10,interleaved,1000,257.000,338.000,268.454,218.000,3345.000
AFTER,1000,10,interleaved,1000,261.000,366.000,281.487,224.000,3624.000
BEFORE,1000,100,head,1000,257.000,339.000,273.189,232.000,3119.000
AFTER,1000,100,head,1000,259.000,335.000,270.181,219.000,3002.000
BEFORE,1000,100,interleaved,1000,260.000,374.000,280.482,218.000,3319.000
AFTER,1000,100,interleaved,1000,266.000,358.000,279.972,223.000,3164.000
BEFORE,1000,1000,head,1000,388.000,551.000,421.069,346.000,5644.000
AFTER,1000,1000,head,1000,272.000,383.000,293.341,227.000,3592.000
BEFORE,1000,1000,interleaved,1000,261.000,367.000,276.380,218.000,3109.000
AFTER,1000,1000,interleaved,1000,268.000,349.000,279.229,224.000,3073.000
BEFORE,100000,0,head,1000,356.000,564.000,383.457,261.000,4509.000
AFTER,100000,0,head,1000,341.000,497.000,365.269,265.000,3242.000
BEFORE,100000,0,interleaved,1000,334.000,488.000,361.578,256.000,3268.000
AFTER,100000,0,interleaved,1000,343.000,491.000,371.440,264.000,3143.000
BEFORE,100000,10,head,1000,336.000,524.000,361.783,259.000,3238.000
AFTER,100000,10,head,1000,346.000,519.000,374.947,269.000,3144.000
BEFORE,100000,10,interleaved,1000,335.000,487.000,358.074,255.000,3221.000
AFTER,100000,10,interleaved,1000,351.000,497.000,375.694,264.000,4003.000
BEFORE,100000,100,head,1000,351.000,489.000,373.372,276.000,3230.000
AFTER,100000,100,head,1000,368.000,530.000,392.306,269.000,3872.000
BEFORE,100000,100,interleaved,1000,346.000,484.000,366.794,262.000,3153.000
AFTER,100000,100,interleaved,1000,318.000,461.000,345.168,259.000,3417.000
BEFORE,100000,1000,head,1000,463.000,671.000,494.582,380.000,3221.000
AFTER,100000,1000,head,1000,348.000,530.000,378.097,261.000,3125.000
BEFORE,100000,1000,interleaved,1000,340.000,459.000,358.413,266.000,3158.000
AFTER,100000,1000,interleaved,1000,340.000,485.000,361.788,263.000,3185.000
BEFORE,100000,10000,head,1000,1629.000,2441.000,1751.378,1502.000,27934.000
AFTER,100000,10000,head,1000,345.000,505.000,369.663,266.000,3706.000
BEFORE,100000,10000,interleaved,1000,355.000,505.000,375.299,256.000,3240.000
AFTER,100000,10000,interleaved,1000,341.000,476.000,368.286,256.000,3853.000
BEFORE,100000,100000,head,1000,13479.000,17299.000,14088.001,13080.000,31053.000
AFTER,100000,100000,head,1000,460.000,643.000,483.115,341.000,4635.000
BEFORE,100000,100000,interleaved,1000,444.000,629.000,460.973,324.000,3143.000
AFTER,100000,100000,interleaved,1000,447.000,695.000,477.123,335.000,3212.000
```

## Raw accepted throughput CSV

```csv
variant,repeat,clients,hold_ms,duration_s,dequeues,tps,avg_locked,max_locked
BEFORE,1,1,0,8,10655,1332.162414,0.30,1
AFTER,1,1,0,8,10567,1321.222151,0.20,1
BEFORE,1,8,0,8,25381,3173.173959,1.66,5
AFTER,1,8,0,8,25169,3147.301304,2.24,5
BEFORE,1,32,0,8,23582,2944.554422,9.31,23
AFTER,1,32,0,8,26073,3259.210554,9.71,20
BEFORE,1,64,0,8,23563,2944.684839,18.85,35
AFTER,1,64,0,8,24370,3044.736766,19.46,39
BEFORE,1,1,5,8,1328,165.953077,0.94,1
AFTER,1,1,5,8,1325,165.569017,0.80,1
BEFORE,1,8,5,8,10291,1285.917213,6.71,8
AFTER,1,8,5,8,10204,1275.289737,6.75,8
BEFORE,1,32,5,8,25845,3230.961424,19.61,28
AFTER,1,32,5,8,24799,3099.211381,18.61,26
BEFORE,1,64,5,8,23030,2876.841575,29.29,44
AFTER,1,64,5,8,23644,2952.199072,27.51,46
BEFORE,2,1,0,8,10655,1332.166911,0.30,1
AFTER,2,1,0,8,10917,1365.263261,0.28,1
BEFORE,2,8,0,8,25894,3236.800980,2.19,6
AFTER,2,8,0,8,24970,3121.755334,2.19,5
BEFORE,2,32,0,8,26211,3275.371917,9.71,21
AFTER,2,32,0,8,26439,3304.302530,10.06,21
BEFORE,2,64,0,8,24657,3080.848758,22.75,44
AFTER,2,64,0,8,23913,2985.509175,18.59,35
BEFORE,2,1,5,8,1327,165.864696,0.96,1
AFTER,2,1,5,8,1335,166.806297,0.79,1
BEFORE,2,8,5,8,10263,1282.928242,6.69,8
AFTER,2,8,5,8,10407,1300.388330,6.41,8
BEFORE,2,32,5,8,24127,3011.475235,17.56,28
AFTER,2,32,5,8,24843,3102.492784,18.31,26
BEFORE,2,64,5,8,23443,2927.836931,28.06,52
AFTER,2,64,5,8,23445,2929.332798,26.46,44
```

## Raw accepted write-cost CSV

```csv
operation,variant,repeat,clients,duration_s,operations,tps,wal_bytes,wal_bytes_per_op,rows_remaining,index_bytes
ENQUEUE,BEFORE,1,32,8,163655,20463.873645,90674216,554.1,163655,17391616
ENQUEUE,AFTER,1,32,8,151465,18938.522479,93672296,618.4,151465,19619840
ENQUEUE,BEFORE,2,32,8,162084,20269.499658,89558296,552.5,162084,17367040
ENQUEUE,AFTER,2,32,8,154162,19252.080849,95749080,621.1,154162,19922944
ENQUEUE,BEFORE,3,32,8,156772,19576.062591,86669136,552.8,156772,16842752
ENQUEUE,AFTER,3,32,8,151183,18909.360073,93569248,618.9,151183,19587072
CLAIM,BEFORE,1,32,8,93428,11676.917778,79921344,855.4,500000,111951872
CLAIM,AFTER,1,32,8,94113,11764.617643,80412576,854.4,500000,134569984
CLAIM,BEFORE,2,32,8,94755,11856.806862,81071120,855.6,500000,112222208
CLAIM,AFTER,2,32,8,90789,11340.094414,77642880,855.2,500000,134127616
CLAIM,BEFORE,3,32,8,92291,11540.961089,78927776,855.2,500000,111853568
CLAIM,AFTER,3,32,8,94114,11776.027500,80429096,854.6,500000,134553600
ACK,BEFORE,1,32,8,129580,16202.778055,28164520,217.4,370420,112926720
ACK,AFTER,1,32,8,131697,16473.850263,28642888,217.5,368303,112934912
ACK,BEFORE,2,32,8,129867,16242.736907,28249992,217.5,370133,112926720
ACK,AFTER,2,32,8,131904,16492.087976,28683432,217.5,368096,112934912
ACK,BEFORE,3,32,8,130934,16380.021913,28429144,217.1,369066,112926720
ACK,AFTER,3,32,8,124210,15533.418673,28008112,225.5,375790,112934912
```

## Future-visible CSV

```csv
variant,future_rows,iterations,p50_us,p95_us,mean_us
BEFORE,1000000,200,441105.000,467232.000,433275.260
AFTER,1000000,200,436587.000,465569.000,429986.365
variant,plan_mode,iterations,p50_us,p95_us,mean_us
BEFORE,force_custom_plan,100,121101.000,139899.000,124895.930
AFTER,force_custom_plan,100,121391.000,132638.000,123383.550
```

## Environment CSV

```csv
pg_version,shared_buffers,max_connections,track_io_timing,fsync,synchronous_commit,wal_compression
16.14 (Debian 16.14-1.pgdg13+1),1GB,200,on,on,on,off
```

## Index sizes at 1,000,001 unlocked rows

```csv
schema,index,bytes
after_case,idx_worker_available,31596544
after_case,idx_worker_queue_session_id,6463488
after_case,idx_worker_queue_tag,6463488
after_case,idx_worker_ready,22487040
after_case,idx_worker_visible,31547392
after_case,worker_queue_pkey,22487040
before_case,idx_worker_available,31596544
before_case,idx_worker_queue_session_id,6463488
before_case,idx_worker_queue_tag,6463488
before_case,idx_worker_visible,31547392
before_case,worker_queue_pkey,22487040
```

## Raw EXPLAIN output

```text
===== HEAD/PLACEMENT locked=100 placement=head BEFORE original query =====
SELECT public.reset_queue('before_case',100000,100,'head');
 reset_queue 
-------------
 
(1 row)

SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint AS now_ms \gset
BEGIN;
BEGIN
EXPLAIN (ANALYZE,BUFFERS,WAL,VERBOSE,SETTINGS)
SELECT q.id,q.session_id
FROM before_case.worker_queue q
LEFT JOIN before_case.sessions s ON s.session_id=q.session_id AND s.locked_until > :now_ms
WHERE q.visible_at <= to_timestamp(:now_ms/1000.0)
  AND (q.lock_token IS NULL OR q.locked_until <= :now_ms)
  AND (q.session_id IS NULL OR s.worker_id='worker-0' OR s.session_id IS NULL)
  AND (CASE 'default_only' WHEN 'default_only' THEN q.tag IS NULL WHEN 'tags' THEN q.tag=ANY(NULL::text[]) WHEN 'default_and' THEN (q.tag IS NULL OR q.tag=ANY(NULL::text[])) WHEN 'any' THEN TRUE ELSE FALSE END)
ORDER BY q.id LIMIT 1 FOR UPDATE OF q SKIP LOCKED;
                                                                                               QUERY PLAN                                                                                               
--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
 Limit  (cost=0.43..0.66 rows=1 width=33) (actual time=0.032..0.033 rows=1 loops=1)
   Output: q.id, q.session_id, q.ctid, s.ctid
   Buffers: shared hit=5
   WAL: records=1 bytes=54
   ->  LockRows  (cost=0.43..22656.11 rows=100010 width=33) (actual time=0.032..0.032 rows=1 loops=1)
         Output: q.id, q.session_id, q.ctid, s.ctid
         Buffers: shared hit=5
         WAL: records=1 bytes=54
         ->  Nested Loop Left Join  (cost=0.43..21656.01 rows=100010 width=33) (actual time=0.026..0.026 rows=1 loops=1)
               Output: q.id, q.session_id, q.ctid, s.ctid
               Inner Unique: true
               Filter: ((q.session_id IS NULL) OR (s.worker_id = 'worker-0'::text) OR (s.session_id IS NULL))
               Buffers: shared hit=4
               ->  Index Scan using worker_queue_pkey on before_case.worker_queue q  (cost=0.29..4142.29 rows=100010 width=27) (actual time=0.023..0.024 rows=1 loops=1)
                     Output: q.id, q.session_id, q.ctid
                     Filter: ((q.tag IS NULL) AND (q.visible_at <= '2026-08-09 05:03:28.652+00'::timestamp with time zone) AND ((q.lock_token IS NULL) OR (q.locked_until <= '1786251808652'::bigint)))
                     Rows Removed by Filter: 100
                     Buffers: shared hit=4
               ->  Index Scan using sessions_pkey on before_case.sessions s  (cost=0.14..0.16 rows=1 width=28) (actual time=0.001..0.001 rows=0 loops=1)
                     Output: s.ctid, s.worker_id, s.session_id
                     Index Cond: (s.session_id = q.session_id)
                     Filter: (s.locked_until > '1786251808652'::bigint)
 Planning:
   Buffers: shared hit=115
 Planning Time: 0.362 ms
 Execution Time: 0.051 ms
(26 rows)

ROLLBACK;
ROLLBACK
===== HEAD/PLACEMENT locked=100 placement=head AFTER phase 1 =====
SELECT public.reset_queue('after_case',100000,100,'head');
 reset_queue 
-------------
 
(1 row)

SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint AS now_ms \gset
BEGIN;
BEGIN
EXPLAIN (ANALYZE,BUFFERS,WAL,VERBOSE,SETTINGS)
SELECT q.id,q.session_id
FROM after_case.worker_queue q
LEFT JOIN after_case.sessions s ON s.session_id=q.session_id AND s.locked_until > :now_ms
WHERE q.visible_at <= to_timestamp(:now_ms/1000.0)
  AND q.lock_token IS NULL
  AND (q.session_id IS NULL OR s.worker_id='worker-0' OR s.session_id IS NULL)
  AND (CASE 'default_only' WHEN 'default_only' THEN q.tag IS NULL WHEN 'tags' THEN q.tag=ANY(NULL::text[]) WHEN 'default_and' THEN (q.tag IS NULL OR q.tag=ANY(NULL::text[])) WHEN 'any' THEN TRUE ELSE FALSE END)
ORDER BY q.id LIMIT 1 FOR UPDATE OF q SKIP LOCKED;
                                                                              QUERY PLAN                                                                              
----------------------------------------------------------------------------------------------------------------------------------------------------------------------
 Limit  (cost=0.43..0.66 rows=1 width=33) (actual time=0.016..0.017 rows=1 loops=1)
   Output: q.id, q.session_id, q.ctid, s.ctid
   Buffers: shared hit=4
   WAL: records=1 bytes=54
   ->  LockRows  (cost=0.43..22393.77 rows=99963 width=33) (actual time=0.016..0.016 rows=1 loops=1)
         Output: q.id, q.session_id, q.ctid, s.ctid
         Buffers: shared hit=4
         WAL: records=1 bytes=54
         ->  Nested Loop Left Join  (cost=0.43..21394.14 rows=99963 width=33) (actual time=0.011..0.012 rows=1 loops=1)
               Output: q.id, q.session_id, q.ctid, s.ctid
               Inner Unique: true
               Filter: ((q.session_id IS NULL) OR (s.worker_id = 'worker-0'::text) OR (s.session_id IS NULL))
               Buffers: shared hit=3
               ->  Index Scan using idx_worker_ready on after_case.worker_queue q  (cost=0.29..3888.64 rows=99963 width=27) (actual time=0.008..0.008 rows=1 loops=1)
                     Output: q.id, q.session_id, q.ctid
                     Filter: ((q.lock_token IS NULL) AND (q.tag IS NULL) AND (q.visible_at <= '2026-08-09 05:03:29.573+00'::timestamp with time zone))
                     Buffers: shared hit=3
               ->  Index Scan using sessions_pkey on after_case.sessions s  (cost=0.14..0.16 rows=1 width=28) (actual time=0.001..0.001 rows=0 loops=1)
                     Output: s.ctid, s.worker_id, s.session_id
                     Index Cond: (s.session_id = q.session_id)
                     Filter: (s.locked_until > '1786251809573'::bigint)
 Planning:
   Buffers: shared hit=119
 Planning Time: 0.392 ms
 Execution Time: 0.035 ms
(25 rows)

ROLLBACK;
ROLLBACK
===== HEAD/PLACEMENT locked=100000 placement=head BEFORE original query =====
SELECT public.reset_queue('before_case',100000,100000,'head');
 reset_queue 
-------------
 
(1 row)

SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint AS now_ms \gset
BEGIN;
BEGIN
EXPLAIN (ANALYZE,BUFFERS,WAL,VERBOSE,SETTINGS)
SELECT q.id,q.session_id
FROM before_case.worker_queue q
LEFT JOIN before_case.sessions s ON s.session_id=q.session_id AND s.locked_until > :now_ms
WHERE q.visible_at <= to_timestamp(:now_ms/1000.0)
  AND (q.lock_token IS NULL OR q.locked_until <= :now_ms)
  AND (q.session_id IS NULL OR s.worker_id='worker-0' OR s.session_id IS NULL)
  AND (CASE 'default_only' WHEN 'default_only' THEN q.tag IS NULL WHEN 'tags' THEN q.tag=ANY(NULL::text[]) WHEN 'default_and' THEN (q.tag IS NULL OR q.tag=ANY(NULL::text[])) WHEN 'any' THEN TRUE ELSE FALSE END)
ORDER BY q.id LIMIT 1 FOR UPDATE OF q SKIP LOCKED;
                                                                                               QUERY PLAN                                                                                               
--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
 Limit  (cost=0.56..0.84 rows=1 width=34) (actual time=16.224..16.225 rows=1 loops=1)
   Output: q.id, q.session_id, q.ctid, s.ctid
   Buffers: shared hit=1611
   WAL: records=1 bytes=54
   ->  LockRows  (cost=0.56..27295.86 rows=99773 width=34) (actual time=16.223..16.224 rows=1 loops=1)
         Output: q.id, q.session_id, q.ctid, s.ctid
         Buffers: shared hit=1611
         WAL: records=1 bytes=54
         ->  Nested Loop Left Join  (cost=0.56..26298.13 rows=99773 width=34) (actual time=16.214..16.215 rows=1 loops=1)
               Output: q.id, q.session_id, q.ctid, s.ctid
               Inner Unique: true
               Filter: ((q.session_id IS NULL) OR (s.worker_id = 'worker-0'::text) OR (s.session_id IS NULL))
               Buffers: shared hit=1610
               ->  Index Scan using worker_queue_pkey on before_case.worker_queue q  (cost=0.42..8572.42 rows=99773 width=28) (actual time=16.205..16.205 rows=1 loops=1)
                     Output: q.id, q.session_id, q.ctid
                     Filter: ((q.tag IS NULL) AND (q.visible_at <= '2026-08-09 05:03:31.346+00'::timestamp with time zone) AND ((q.lock_token IS NULL) OR (q.locked_until <= '1786251811346'::bigint)))
                     Rows Removed by Filter: 100000
                     Buffers: shared hit=1610
               ->  Index Scan using sessions_pkey on before_case.sessions s  (cost=0.14..0.17 rows=1 width=29) (actual time=0.004..0.004 rows=0 loops=1)
                     Output: s.ctid, s.worker_id, s.session_id
                     Index Cond: (s.session_id = q.session_id)
                     Filter: (s.locked_until > '1786251811346'::bigint)
 Planning:
   Buffers: shared hit=114
 Planning Time: 0.395 ms
 Execution Time: 16.244 ms
(26 rows)

ROLLBACK;
ROLLBACK
===== HEAD/PLACEMENT locked=100000 placement=head AFTER phase 1 =====
SELECT public.reset_queue('after_case',100000,100000,'head');
 reset_queue 
-------------
 
(1 row)

SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint AS now_ms \gset
BEGIN;
BEGIN
EXPLAIN (ANALYZE,BUFFERS,WAL,VERBOSE,SETTINGS)
SELECT q.id,q.session_id
FROM after_case.worker_queue q
LEFT JOIN after_case.sessions s ON s.session_id=q.session_id AND s.locked_until > :now_ms
WHERE q.visible_at <= to_timestamp(:now_ms/1000.0)
  AND q.lock_token IS NULL
  AND (q.session_id IS NULL OR s.worker_id='worker-0' OR s.session_id IS NULL)
  AND (CASE 'default_only' WHEN 'default_only' THEN q.tag IS NULL WHEN 'tags' THEN q.tag=ANY(NULL::text[]) WHEN 'default_and' THEN (q.tag IS NULL OR q.tag=ANY(NULL::text[])) WHEN 'any' THEN TRUE ELSE FALSE END)
ORDER BY q.id LIMIT 1 FOR UPDATE OF q SKIP LOCKED;
                                                                              QUERY PLAN                                                                               
-----------------------------------------------------------------------------------------------------------------------------------------------------------------------
 Limit  (cost=0.44..0.67 rows=1 width=34) (actual time=0.016..0.017 rows=1 loops=1)
   Output: q.id, q.session_id, q.ctid, s.ctid
   Buffers: shared hit=4
   WAL: records=1 bytes=54
   ->  LockRows  (cost=0.44..22877.33 rows=100327 width=34) (actual time=0.016..0.016 rows=1 loops=1)
         Output: q.id, q.session_id, q.ctid, s.ctid
         Buffers: shared hit=4
         WAL: records=1 bytes=54
         ->  Nested Loop Left Join  (cost=0.44..21874.06 rows=100327 width=34) (actual time=0.011..0.012 rows=1 loops=1)
               Output: q.id, q.session_id, q.ctid, s.ctid
               Inner Unique: true
               Filter: ((q.session_id IS NULL) OR (s.worker_id = 'worker-0'::text) OR (s.session_id IS NULL))
               Buffers: shared hit=3
               ->  Index Scan using idx_worker_ready on after_case.worker_queue q  (cost=0.29..4050.02 rows=100327 width=28) (actual time=0.008..0.008 rows=1 loops=1)
                     Output: q.id, q.session_id, q.ctid
                     Filter: ((q.lock_token IS NULL) AND (q.tag IS NULL) AND (q.visible_at <= '2026-08-09 05:03:33.24+00'::timestamp with time zone))
                     Buffers: shared hit=3
               ->  Index Scan using sessions_pkey on after_case.sessions s  (cost=0.14..0.17 rows=1 width=29) (actual time=0.001..0.001 rows=0 loops=1)
                     Output: s.ctid, s.worker_id, s.session_id
                     Index Cond: (s.session_id = q.session_id)
                     Filter: (s.locked_until > '1786251813240'::bigint)
 Planning:
   Buffers: shared hit=120
 Planning Time: 0.393 ms
 Execution Time: 0.034 ms
(25 rows)

ROLLBACK;
ROLLBACK
===== HEAD/PLACEMENT locked=100000 placement=interleaved BEFORE original query =====
SELECT public.reset_queue('before_case',100000,100000,'interleaved');
 reset_queue 
-------------
 
(1 row)

SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint AS now_ms \gset
BEGIN;
BEGIN
EXPLAIN (ANALYZE,BUFFERS,WAL,VERBOSE,SETTINGS)
SELECT q.id,q.session_id
FROM before_case.worker_queue q
LEFT JOIN before_case.sessions s ON s.session_id=q.session_id AND s.locked_until > :now_ms
WHERE q.visible_at <= to_timestamp(:now_ms/1000.0)
  AND (q.lock_token IS NULL OR q.locked_until <= :now_ms)
  AND (q.session_id IS NULL OR s.worker_id='worker-0' OR s.session_id IS NULL)
  AND (CASE 'default_only' WHEN 'default_only' THEN q.tag IS NULL WHEN 'tags' THEN q.tag=ANY(NULL::text[]) WHEN 'default_and' THEN (q.tag IS NULL OR q.tag=ANY(NULL::text[])) WHEN 'any' THEN TRUE ELSE FALSE END)
ORDER BY q.id LIMIT 1 FOR UPDATE OF q SKIP LOCKED;
                                                                                               QUERY PLAN                                                                                               
--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
 Limit  (cost=0.56..0.84 rows=1 width=34) (actual time=0.022..0.022 rows=1 loops=1)
   Output: q.id, q.session_id, q.ctid, s.ctid
   Buffers: shared hit=5
   WAL: records=1 fpi=1 bytes=8147
   ->  LockRows  (cost=0.56..27523.17 rows=100900 width=34) (actual time=0.021..0.021 rows=1 loops=1)
         Output: q.id, q.session_id, q.ctid, s.ctid
         Buffers: shared hit=5
         WAL: records=1 fpi=1 bytes=8147
         ->  Nested Loop Left Join  (cost=0.56..26514.17 rows=100900 width=34) (actual time=0.013..0.013 rows=1 loops=1)
               Output: q.id, q.session_id, q.ctid, s.ctid
               Inner Unique: true
               Filter: ((q.session_id IS NULL) OR (s.worker_id = 'worker-0'::text) OR (s.session_id IS NULL))
               Buffers: shared hit=4
               ->  Index Scan using worker_queue_pkey on before_case.worker_queue q  (cost=0.42..8588.42 rows=100900 width=28) (actual time=0.011..0.011 rows=1 loops=1)
                     Output: q.id, q.session_id, q.ctid
                     Filter: ((q.tag IS NULL) AND (q.visible_at <= '2026-08-09 05:03:35.119+00'::timestamp with time zone) AND ((q.lock_token IS NULL) OR (q.locked_until <= '1786251815119'::bigint)))
                     Rows Removed by Filter: 1
                     Buffers: shared hit=4
               ->  Index Scan using sessions_pkey on before_case.sessions s  (cost=0.14..0.17 rows=1 width=29) (actual time=0.000..0.001 rows=0 loops=1)
                     Output: s.ctid, s.worker_id, s.session_id
                     Index Cond: (s.session_id = q.session_id)
                     Filter: (s.locked_until > '1786251815119'::bigint)
 Planning:
   Buffers: shared hit=114
 Planning Time: 0.359 ms
 Execution Time: 0.041 ms
(26 rows)

ROLLBACK;
ROLLBACK
===== HEAD/PLACEMENT locked=100000 placement=interleaved AFTER phase 1 =====
SELECT public.reset_queue('after_case',100000,100000,'interleaved');
 reset_queue 
-------------
 
(1 row)

SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint AS now_ms \gset
BEGIN;
BEGIN
EXPLAIN (ANALYZE,BUFFERS,WAL,VERBOSE,SETTINGS)
SELECT q.id,q.session_id
FROM after_case.worker_queue q
LEFT JOIN after_case.sessions s ON s.session_id=q.session_id AND s.locked_until > :now_ms
WHERE q.visible_at <= to_timestamp(:now_ms/1000.0)
  AND q.lock_token IS NULL
  AND (q.session_id IS NULL OR s.worker_id='worker-0' OR s.session_id IS NULL)
  AND (CASE 'default_only' WHEN 'default_only' THEN q.tag IS NULL WHEN 'tags' THEN q.tag=ANY(NULL::text[]) WHEN 'default_and' THEN (q.tag IS NULL OR q.tag=ANY(NULL::text[])) WHEN 'any' THEN TRUE ELSE FALSE END)
ORDER BY q.id LIMIT 1 FOR UPDATE OF q SKIP LOCKED;
                                                                              QUERY PLAN                                                                               
-----------------------------------------------------------------------------------------------------------------------------------------------------------------------
 Limit  (cost=0.44..0.67 rows=1 width=34) (actual time=0.016..0.017 rows=1 loops=1)
   Output: q.id, q.session_id, q.ctid, s.ctid
   Buffers: shared hit=4
   WAL: records=1 bytes=54
   ->  LockRows  (cost=0.44..22834.36 rows=100093 width=34) (actual time=0.015..0.016 rows=1 loops=1)
         Output: q.id, q.session_id, q.ctid, s.ctid
         Buffers: shared hit=4
         WAL: records=1 bytes=54
         ->  Nested Loop Left Join  (cost=0.44..21833.43 rows=100093 width=34) (actual time=0.011..0.012 rows=1 loops=1)
               Output: q.id, q.session_id, q.ctid, s.ctid
               Inner Unique: true
               Filter: ((q.session_id IS NULL) OR (s.worker_id = 'worker-0'::text) OR (s.session_id IS NULL))
               Buffers: shared hit=3
               ->  Index Scan using idx_worker_ready on after_case.worker_queue q  (cost=0.29..4050.92 rows=100093 width=28) (actual time=0.009..0.009 rows=1 loops=1)
                     Output: q.id, q.session_id, q.ctid
                     Filter: ((q.lock_token IS NULL) AND (q.tag IS NULL) AND (q.visible_at <= '2026-08-09 05:03:37.075+00'::timestamp with time zone))
                     Buffers: shared hit=3
               ->  Index Scan using sessions_pkey on after_case.sessions s  (cost=0.14..0.17 rows=1 width=29) (actual time=0.001..0.001 rows=0 loops=1)
                     Output: s.ctid, s.worker_id, s.session_id
                     Index Cond: (s.session_id = q.session_id)
                     Filter: (s.locked_until > '1786251817075'::bigint)
 Planning:
   Buffers: shared hit=120
 Planning Time: 0.391 ms
 Execution Time: 0.034 ms
(25 rows)

ROLLBACK;
ROLLBACK
===== FUTURE visible_at: 1,000,000 future rows + one visible, BEFORE =====
SELECT public.reset_future_queue('before_case',1000000);
 reset_future_queue 
--------------------
 
(1 row)

SELECT count(*) FROM before_case.worker_queue;
  count  
---------
 1000001
(1 row)

SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint AS now_ms \gset
BEGIN;
BEGIN
EXPLAIN (ANALYZE,BUFFERS,WAL,VERBOSE,SETTINGS)
SELECT q.id,q.session_id
FROM before_case.worker_queue q
LEFT JOIN before_case.sessions s ON s.session_id=q.session_id AND s.locked_until > :now_ms
WHERE q.visible_at <= to_timestamp(:now_ms/1000.0)
  AND (q.lock_token IS NULL OR q.locked_until <= :now_ms)
  AND (q.session_id IS NULL OR s.worker_id='worker-0' OR s.session_id IS NULL)
  AND q.tag IS NULL
ORDER BY q.id LIMIT 1 FOR UPDATE OF q SKIP LOCKED;
                                                                               QUERY PLAN                                                                                
-------------------------------------------------------------------------------------------------------------------------------------------------------------------------
 Limit  (cost=34.52..34.53 rows=1 width=52) (actual time=0.019..0.020 rows=1 loops=1)
   Output: q.id, q.session_id, q.ctid, s.ctid
   Buffers: shared hit=5
   WAL: records=1 bytes=54
   ->  LockRows  (cost=34.52..34.57 rows=4 width=52) (actual time=0.018..0.019 rows=1 loops=1)
         Output: q.id, q.session_id, q.ctid, s.ctid
         Buffers: shared hit=5
         WAL: records=1 bytes=54
         ->  Sort  (cost=34.52..34.53 rows=4 width=52) (actual time=0.011..0.012 rows=1 loops=1)
               Output: q.id, q.session_id, q.ctid, s.ctid
               Sort Key: q.id
               Sort Method: quicksort  Memory: 25kB
               Buffers: shared hit=4
               ->  Nested Loop Left Join  (cost=0.58..34.50 rows=4 width=52) (actual time=0.007..0.007 rows=1 loops=1)
                     Output: q.id, q.session_id, q.ctid, s.ctid
                     Inner Unique: true
                     Filter: ((q.session_id IS NULL) OR (s.worker_id = 'worker-0'::text) OR (s.session_id IS NULL))
                     Buffers: shared hit=4
                     ->  Index Scan using idx_worker_visible on before_case.worker_queue q  (cost=0.42..13.76 rows=4 width=46) (actual time=0.004..0.005 rows=1 loops=1)
                           Output: q.id, q.session_id, q.ctid
                           Index Cond: (q.visible_at <= '2026-08-09 05:03:43.478+00'::timestamp with time zone)
                           Filter: ((q.tag IS NULL) AND ((q.lock_token IS NULL) OR (q.locked_until <= '1786251823478'::bigint)))
                           Buffers: shared hit=4
                     ->  Index Scan using sessions_pkey on before_case.sessions s  (cost=0.15..5.17 rows=1 width=29) (actual time=0.001..0.001 rows=0 loops=1)
                           Output: s.ctid, s.worker_id, s.session_id
                           Index Cond: (s.session_id = q.session_id)
                           Filter: (s.locked_until > '1786251823478'::bigint)
 Planning:
   Buffers: shared hit=81 read=1
   I/O Timings: shared read=0.010
 Planning Time: 0.338 ms
 Execution Time: 0.038 ms
(32 rows)

ROLLBACK;
ROLLBACK
===== FUTURE visible_at: 1,000,000 future rows + one visible, AFTER phase 1 =====
SELECT public.reset_future_queue('after_case',1000000);
 reset_future_queue 
--------------------
 
(1 row)

SELECT count(*) FROM after_case.worker_queue;
  count  
---------
 1000001
(1 row)

SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint AS now_ms \gset
BEGIN;
BEGIN
EXPLAIN (ANALYZE,BUFFERS,WAL,VERBOSE,SETTINGS)
SELECT q.id,q.session_id
FROM after_case.worker_queue q
LEFT JOIN after_case.sessions s ON s.session_id=q.session_id AND s.locked_until > :now_ms
WHERE q.visible_at <= to_timestamp(:now_ms/1000.0)
  AND q.lock_token IS NULL
  AND (q.session_id IS NULL OR s.worker_id='worker-0' OR s.session_id IS NULL)
  AND q.tag IS NULL
ORDER BY q.id LIMIT 1 FOR UPDATE OF q SKIP LOCKED;
                                                                               QUERY PLAN                                                                               
------------------------------------------------------------------------------------------------------------------------------------------------------------------------
 Limit  (cost=34.52..34.53 rows=1 width=52) (actual time=0.018..0.019 rows=1 loops=1)
   Output: q.id, q.session_id, q.ctid, s.ctid
   Buffers: shared hit=5
   WAL: records=1 bytes=54
   ->  LockRows  (cost=34.52..34.56 rows=4 width=52) (actual time=0.017..0.018 rows=1 loops=1)
         Output: q.id, q.session_id, q.ctid, s.ctid
         Buffers: shared hit=5
         WAL: records=1 bytes=54
         ->  Sort  (cost=34.52..34.52 rows=4 width=52) (actual time=0.011..0.011 rows=1 loops=1)
               Output: q.id, q.session_id, q.ctid, s.ctid
               Sort Key: q.id
               Sort Method: quicksort  Memory: 25kB
               Buffers: shared hit=4
               ->  Nested Loop Left Join  (cost=0.58..34.49 rows=4 width=52) (actual time=0.007..0.007 rows=1 loops=1)
                     Output: q.id, q.session_id, q.ctid, s.ctid
                     Inner Unique: true
                     Filter: ((q.session_id IS NULL) OR (s.worker_id = 'worker-0'::text) OR (s.session_id IS NULL))
                     Buffers: shared hit=4
                     ->  Index Scan using idx_worker_visible on after_case.worker_queue q  (cost=0.42..13.75 rows=4 width=46) (actual time=0.004..0.004 rows=1 loops=1)
                           Output: q.id, q.session_id, q.ctid
                           Index Cond: ((q.visible_at <= '2026-08-09 05:03:50.24+00'::timestamp with time zone) AND (q.lock_token IS NULL))
                           Filter: (q.tag IS NULL)
                           Buffers: shared hit=4
                     ->  Index Scan using sessions_pkey on after_case.sessions s  (cost=0.15..5.17 rows=1 width=29) (actual time=0.001..0.001 rows=0 loops=1)
                           Output: s.ctid, s.worker_id, s.session_id
                           Index Cond: (s.session_id = q.session_id)
                           Filter: (s.locked_until > '1786251830240'::bigint)
 Planning:
   Buffers: shared hit=78 read=1
   I/O Timings: shared read=0.014
 Planning Time: 0.296 ms
 Execution Time: 0.037 ms
(32 rows)

ROLLBACK;
ROLLBACK
```

## Exact benchmark commands: autocommit latency driver

```bash
#!/usr/bin/env bash
set -euo pipefail
cd '/tmp'
exec 'bash' '-lc' 'set -euo pipefail
mkdir -p /tmp/pr18/client-latency
export PGPASSWORD="$POSTGRES_PASSWORD"
OUT=/tmp/pr18/latency-autocommit.csv
echo '"'"'variant,ready_backlog,locked,placement,iterations,p50_us,p95_us,mean_us,min_us,max_us'"'"' > "$OUT"
run_cell() {
  local variant="$1" backlog="$2" locked="$3" placement="$4" schema lower sql warm raw samples sorted n p50n p95n p50 p95 mean min max
  lower=$(echo "$variant" | tr '"'"'[:upper:]'"'"' '"'"'[:lower:]'"'"')
  if [ "$variant" = BEFORE ]; then schema=before_case; else schema=after_case; fi
  echo "cell variant=$variant backlog=$backlog locked=$locked placement=$placement" >&2
  psql -v ON_ERROR_STOP=1 -X -h 127.0.0.1 -U postgres -d bench -Atc "SELECT public.reset_queue('"'"'$schema'"'"',$backlog,$locked,'"'"'$placement'"'"');" >/dev/null
  warm=/tmp/pr18/client-latency/warm.sql
  { echo '"'"'\o /dev/null'"'"'; for i in $(seq 1 50); do
      echo "SELECT out_lock_token FROM $schema.fetch_work_item((extract(epoch FROM clock_timestamp())*1000)::bigint,60000,'"'"'worker-0'"'"',60000,NULL,'"'"'default_only'"'"');"
      echo "UPDATE $schema.worker_queue SET lock_token=NULL,locked_until=NULL,attempt_count=attempt_count-1 WHERE lock_token LIKE '"'"'lock_%'"'"';"
    done; } > "$warm"
  psql -v ON_ERROR_STOP=1 -X -q -h 127.0.0.1 -U postgres -d bench -f "$warm" >/dev/null
  psql -v ON_ERROR_STOP=1 -X -q -h 127.0.0.1 -U postgres -d bench -c "VACUUM (ANALYZE) $schema.worker_queue" >/dev/null
  sql=/tmp/pr18/client-latency/measure.sql
  { echo '"'"'\timing on'"'"'; echo '"'"'\o /dev/null'"'"'; for i in $(seq 1 1000); do
      echo "SELECT out_lock_token FROM $schema.fetch_work_item((extract(epoch FROM clock_timestamp())*1000)::bigint,60000,'"'"'worker-0'"'"',60000,NULL,'"'"'default_only'"'"');"
      echo "UPDATE $schema.worker_queue SET lock_token=NULL,locked_until=NULL,attempt_count=attempt_count-1 WHERE lock_token LIKE '"'"'lock_%'"'"';"
    done; } > "$sql"
  raw=/tmp/pr18/client-latency/${lower}-${backlog}-${locked}-${placement}.raw
  samples=/tmp/pr18/client-latency/${lower}-${backlog}-${locked}-${placement}.us
  psql -v ON_ERROR_STOP=1 -X -h 127.0.0.1 -U postgres -d bench -f "$sql" > "$raw" 2>&1
  awk '"'"'/^Time:/{n++; if(n%2==1) printf "%.3f\n", $2*1000}'"'"' "$raw" > "$samples"
  sorted=${samples}.sorted; sort -n "$samples" > "$sorted"; n=$(wc -l < "$sorted")
  [ "$n" -eq 1000 ] || { echo "expected 1000 samples, got $n" >&2; exit 3; }
  p50n=$(( (n + 1) / 2 )); p95n=$(( (95*n + 99) / 100 ))
  p50=$(sed -n "${p50n}p" "$sorted"); p95=$(sed -n "${p95n}p" "$sorted")
  mean=$(awk '"'"'{s+=$1} END {printf "%.3f",s/NR}'"'"' "$samples")
  min=$(sed -n '"'"'1p'"'"' "$sorted"); max=$(sed -n "${n}p" "$sorted")
  echo "$variant,$backlog,$locked,$placement,$n,$p50,$p95,$mean,$min,$max" >> "$OUT"
}
for backlog in 1000 100000; do
  if [ "$backlog" -eq 1000 ]; then counts='"'"'0 10 100 1000'"'"'; else counts='"'"'0 10 100 1000 10000 100000'"'"'; fi
  for locked in $counts; do
    for placement in head interleaved; do
      run_cell BEFORE "$backlog" "$locked" "$placement"
      run_cell AFTER "$backlog" "$locked" "$placement"
    done
  done
done
column -s, -t "$OUT" 2>/dev/null || cat "$OUT"
'
```

## Exact benchmark commands: concurrent throughput

```bash
#!/usr/bin/env bash
set -euo pipefail
exec 9>/tmp/pr18/throughput-v2.lock
flock -n 9 || { echo 'another throughput-v2 process is active' >&2; exit 9; }
export PGPASSWORD="$POSTGRES_PASSWORD"
OUT=/tmp/pr18/throughput-v2.csv
echo 'variant,repeat,clients,hold_ms,duration_s,dequeues,tps,avg_locked,max_locked' > "$OUT"
sample_inflight() {
  local schema="$1" out="$2"
  {
    echo '\pset tuples_only on'
    echo '\pset format unaligned'
    for i in $(seq 1 80); do
      echo "SELECT count(*) FROM $schema.worker_queue WHERE lock_token IS NOT NULL;"
      echo "SELECT pg_sleep(0.1);"
    done
  } | psql -v ON_ERROR_STOP=1 -X -q -h 127.0.0.1 -U postgres -d bench > "$out"
}
run_case() {
  local variant="$1" repeat="$2" clients="$3" hold="$4" schema lower script jobs log samples sampler tx tps avg max
  lower=$(echo "$variant" | tr '[:upper:]' '[:lower:]')
  if [ "$variant" = BEFORE ]; then schema=before_case; else schema=after_case; fi
  script=/tmp/pr18/throughput-${lower}-${hold}ms.sql
  jobs=$clients; [ "$jobs" -gt 8 ] && jobs=8
  echo "throughput-v2 variant=$variant repeat=$repeat clients=$clients hold_ms=$hold" >&2
  psql -v ON_ERROR_STOP=1 -X -q -h 127.0.0.1 -U postgres -d bench <<SQL
SELECT public.reset_queue('$schema',100000,0,'head');
UPDATE $schema.worker_queue SET session_id=NULL;
TRUNCATE $schema.sessions;
ANALYZE $schema.worker_queue;
SQL
  pgbench -n -h 127.0.0.1 -U postgres -d bench -c "$clients" -j "$jobs" -T 2 -f "$script" >/tmp/pr18/warm-${lower}-${repeat}-${clients}-${hold}.log 2>&1
  samples=/tmp/pr18/locked-v2-${lower}-${repeat}-${clients}-${hold}.txt
  sample_inflight "$schema" "$samples" & sampler=$!
  log=/tmp/pr18/throughput-v2-${lower}-${repeat}-${clients}-${hold}.log
  pgbench -n -h 127.0.0.1 -U postgres -d bench -c "$clients" -j "$jobs" -T 8 -f "$script" > "$log" 2>&1
  wait "$sampler"
  tx=$(awk -F': ' '/number of transactions actually processed/{split($2,a,"/"); v=a[1]} END{print v}' "$log")
  tps=$(awk '/^tps =/{v=$3} END{print v}' "$log")
  avg=$(awk '/^[0-9]+$/{s+=$1;n++} END{if(n)printf "%.2f",s/n;else print 0}' "$samples")
  max=$(awk '/^[0-9]+$/&&$1>m{m=$1} END{print m+0}' "$samples")
  test -n "$tx"; test -n "$tps"
  echo "$variant,$repeat,$clients,$hold,8,$tx,$tps,$avg,$max" >> "$OUT"
}
for repeat in 1 2; do
  for hold in 0 5; do
    for clients in 1 8 32 64; do
      run_case BEFORE "$repeat" "$clients" "$hold"
      run_case AFTER "$repeat" "$clients" "$hold"
    done
  done
done
cat "$OUT"
```

### Per-transaction throughput SQL

```sql
-- BEFORE, zero hold
SELECT quote_literal(out_lock_token) AS tok
FROM before_case.fetch_work_item((extract(epoch FROM clock_timestamp())*1000)::bigint,60000,'worker-' || :client_id::text,60000,NULL,'default_only') \gset
DELETE FROM before_case.worker_queue WHERE lock_token = :tok;
INSERT INTO before_case.worker_queue(work_item,visible_at,created_at,tag)
VALUES (json_build_object('client',:client_id)::text,clock_timestamp(),clock_timestamp(),NULL);

-- AFTER, zero hold
SELECT quote_literal(out_lock_token) AS tok
FROM after_case.fetch_work_item((extract(epoch FROM clock_timestamp())*1000)::bigint,60000,'worker-' || :client_id::text,60000,NULL,'default_only') \gset
DELETE FROM after_case.worker_queue WHERE lock_token = :tok;
INSERT INTO after_case.worker_queue(work_item,visible_at,created_at,tag)
VALUES (json_build_object('client',:client_id)::text,clock_timestamp(),clock_timestamp(),NULL);

-- BEFORE, 5 ms hold
SELECT quote_literal(out_lock_token) AS tok
FROM before_case.fetch_work_item((extract(epoch FROM clock_timestamp())*1000)::bigint,60000,'worker-' || :client_id::text,60000,NULL,'default_only') \gset
\sleep 5 ms
DELETE FROM before_case.worker_queue WHERE lock_token = :tok;
INSERT INTO before_case.worker_queue(work_item,visible_at,created_at,tag)
VALUES (json_build_object('client',:client_id)::text,clock_timestamp(),clock_timestamp(),NULL);

-- AFTER, 5 ms hold
SELECT quote_literal(out_lock_token) AS tok
FROM after_case.fetch_work_item((extract(epoch FROM clock_timestamp())*1000)::bigint,60000,'worker-' || :client_id::text,60000,NULL,'default_only') \gset
\sleep 5 ms
DELETE FROM after_case.worker_queue WHERE lock_token = :tok;
INSERT INTO after_case.worker_queue(work_item,visible_at,created_at,tag)
VALUES (json_build_object('client',:client_id)::text,clock_timestamp(),clock_timestamp(),NULL);
```

## Exact benchmark commands: write cost

```bash
#!/usr/bin/env bash
set -euo pipefail
exec 9>/tmp/pr18/write-cost-v2.lock
flock -n 9 || { echo 'another write-cost process is active' >&2; exit 9; }
export PGPASSWORD="$POSTGRES_PASSWORD"
OUT=/tmp/pr18/write-cost-v2.csv
echo 'operation,variant,repeat,clients,duration_s,operations,tps,wal_bytes,wal_bytes_per_op,rows_remaining,index_bytes' > "$OUT"
prepare_case() {
  local op="$1" variant="$2" schema="${variant}_case"
  case "$op" in
    enqueue)
      psql -v ON_ERROR_STOP=1 -X -q -h 127.0.0.1 -U postgres -d bench -c "TRUNCATE $schema.worker_queue RESTART IDENTITY; TRUNCATE $schema.sessions;" ;;
    claim)
      psql -v ON_ERROR_STOP=1 -X -q -h 127.0.0.1 -U postgres -d bench <<SQL
SELECT public.reset_queue('$schema',500000,0,'head');
UPDATE $schema.worker_queue SET session_id=NULL;
TRUNCATE $schema.sessions;
DROP SEQUENCE IF EXISTS public.claim_seq_${variant};
CREATE SEQUENCE public.claim_seq_${variant} START 1 CACHE 1000;
ANALYZE $schema.worker_queue;
SQL
      ;;
    ack)
      psql -v ON_ERROR_STOP=1 -X -q -h 127.0.0.1 -U postgres -d bench <<SQL
SELECT public.reset_queue('$schema',0,500000,'head');
UPDATE $schema.worker_queue SET session_id=NULL;
TRUNCATE $schema.sessions;
DROP SEQUENCE IF EXISTS public.ack_seq_${variant};
CREATE SEQUENCE public.ack_seq_${variant} START 1 CACHE 1000;
ANALYZE $schema.worker_queue;
SQL
      ;;
  esac
  psql -X -q -h 127.0.0.1 -U postgres -d bench -c 'CHECKPOINT' >/dev/null
}
run_case() {
  local op="$1" variant="$2" repeat="$3" before_lsn after_lsn wal log tx tps walptx rows index_bytes
  local schema="${variant}_case"
  echo "write-v2 operation=$op variant=$variant repeat=$repeat" >&2
  prepare_case "$op" "$variant"
  before_lsn=$(psql -X -h 127.0.0.1 -U postgres -d bench -Atc 'SELECT pg_current_wal_lsn()')
  log=/tmp/pr18/write/${op}-${variant}-v2-${repeat}.log
  pgbench -n -h 127.0.0.1 -U postgres -d bench -c 32 -j 8 -T 8 -f /tmp/pr18/write/${op}-${variant}.sql > "$log" 2>&1
  after_lsn=$(psql -X -h 127.0.0.1 -U postgres -d bench -Atc 'SELECT pg_current_wal_lsn()')
  wal=$(psql -X -h 127.0.0.1 -U postgres -d bench -Atc "SELECT pg_wal_lsn_diff('$after_lsn','$before_lsn')::bigint")
  tx=$(awk -F': ' '/number of transactions actually processed/{split($2,a,"/"); v=a[1]} END{print v}' "$log")
  tps=$(awk '/^tps =/{v=$3} END{print v}' "$log")
  test -n "$tx"; test -n "$tps"
  walptx=$(awk -v w="$wal" -v t="$tx" 'BEGIN{if(t>0)printf "%.1f",w/t;else print 0}')
  rows=$(psql -X -h 127.0.0.1 -U postgres -d bench -Atc "SELECT count(*) FROM $schema.worker_queue")
  index_bytes=$(psql -X -h 127.0.0.1 -U postgres -d bench -Atc "SELECT COALESCE(sum(pg_relation_size(indexrelid)),0) FROM pg_stat_user_indexes WHERE schemaname='${variant}_case' AND relname='worker_queue'")
  echo "${op^^},${variant^^},$repeat,32,8,$tx,$tps,$wal,$walptx,$rows,$index_bytes" >> "$OUT"
}
for op in enqueue claim ack; do
  for repeat in 1 2 3; do
    run_case "$op" before "$repeat"
    run_case "$op" after "$repeat"
  done
done
cat "$OUT"
```

### Per-operation write SQL

```sql
-- /tmp/pr18/write/enqueue-before.sql
INSERT INTO before_case.worker_queue(work_item,visible_at,created_at,tag)
VALUES (json_build_object('client',:client_id,'rnd',random())::text,clock_timestamp(),clock_timestamp(),NULL);
-- /tmp/pr18/write/enqueue-after.sql
INSERT INTO after_case.worker_queue(work_item,visible_at,created_at,tag)
VALUES (json_build_object('client',:client_id,'rnd',random())::text,clock_timestamp(),clock_timestamp(),NULL);
-- /tmp/pr18/write/claim-before.sql
WITH n AS (SELECT nextval('public.claim_seq_before') AS id)
UPDATE before_case.worker_queue q
SET lock_token='claimed-' || n.id::text,
    locked_until=(extract(epoch FROM clock_timestamp())*1000)::bigint + 60000
FROM n WHERE q.id=n.id;
-- /tmp/pr18/write/claim-after.sql
WITH n AS (SELECT nextval('public.claim_seq_after') AS id)
UPDATE after_case.worker_queue q
SET lock_token='claimed-' || n.id::text,
    locked_until=(extract(epoch FROM clock_timestamp())*1000)::bigint + 60000
FROM n WHERE q.id=n.id;
-- /tmp/pr18/write/ack-before.sql
WITH n AS (SELECT nextval('public.ack_seq_before') AS id)
DELETE FROM before_case.worker_queue q USING n WHERE q.id=n.id;
-- /tmp/pr18/write/ack-after.sql
WITH n AS (SELECT nextval('public.ack_seq_after') AS id)
DELETE FROM after_case.worker_queue q USING n WHERE q.id=n.id;
```

## Exact benchmark commands: EXPLAIN and future visibility

```bash
#!/usr/bin/env bash
set -euo pipefail
export PGPASSWORD="$POSTGRES_PASSWORD"
RAW=/tmp/pr18/explain-raw.txt
: > "$RAW"
explain_cell() {
  local locked="$1" placement="$2"
  echo "===== HEAD/PLACEMENT locked=$locked placement=$placement BEFORE original query =====" | tee -a "$RAW"
  psql -v ON_ERROR_STOP=1 -X -a -h 127.0.0.1 -U postgres -d bench <<SQL 2>&1 | tee -a "$RAW"
SELECT public.reset_queue('before_case',100000,$locked,'$placement');
SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint AS now_ms \gset
BEGIN;
EXPLAIN (ANALYZE,BUFFERS,WAL,VERBOSE,SETTINGS)
SELECT q.id,q.session_id
FROM before_case.worker_queue q
LEFT JOIN before_case.sessions s ON s.session_id=q.session_id AND s.locked_until > :now_ms
WHERE q.visible_at <= to_timestamp(:now_ms/1000.0)
  AND (q.lock_token IS NULL OR q.locked_until <= :now_ms)
  AND (q.session_id IS NULL OR s.worker_id='worker-0' OR s.session_id IS NULL)
  AND (CASE 'default_only' WHEN 'default_only' THEN q.tag IS NULL WHEN 'tags' THEN q.tag=ANY(NULL::text[]) WHEN 'default_and' THEN (q.tag IS NULL OR q.tag=ANY(NULL::text[])) WHEN 'any' THEN TRUE ELSE FALSE END)
ORDER BY q.id LIMIT 1 FOR UPDATE OF q SKIP LOCKED;
ROLLBACK;
SQL
  echo "===== HEAD/PLACEMENT locked=$locked placement=$placement AFTER phase 1 =====" | tee -a "$RAW"
  psql -v ON_ERROR_STOP=1 -X -a -h 127.0.0.1 -U postgres -d bench <<SQL 2>&1 | tee -a "$RAW"
SELECT public.reset_queue('after_case',100000,$locked,'$placement');
SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint AS now_ms \gset
BEGIN;
EXPLAIN (ANALYZE,BUFFERS,WAL,VERBOSE,SETTINGS)
SELECT q.id,q.session_id
FROM after_case.worker_queue q
LEFT JOIN after_case.sessions s ON s.session_id=q.session_id AND s.locked_until > :now_ms
WHERE q.visible_at <= to_timestamp(:now_ms/1000.0)
  AND q.lock_token IS NULL
  AND (q.session_id IS NULL OR s.worker_id='worker-0' OR s.session_id IS NULL)
  AND (CASE 'default_only' WHEN 'default_only' THEN q.tag IS NULL WHEN 'tags' THEN q.tag=ANY(NULL::text[]) WHEN 'default_and' THEN (q.tag IS NULL OR q.tag=ANY(NULL::text[])) WHEN 'any' THEN TRUE ELSE FALSE END)
ORDER BY q.id LIMIT 1 FOR UPDATE OF q SKIP LOCKED;
ROLLBACK;
SQL
}
explain_cell 100 head
explain_cell 100000 head
explain_cell 100000 interleaved

echo '===== FUTURE visible_at: 1,000,000 future rows + one visible, BEFORE =====' | tee -a "$RAW"
psql -v ON_ERROR_STOP=1 -X -a -h 127.0.0.1 -U postgres -d bench <<'SQL' 2>&1 | tee -a "$RAW"
SELECT public.reset_future_queue('before_case',1000000);
SELECT count(*) FROM before_case.worker_queue;
SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint AS now_ms \gset
BEGIN;
EXPLAIN (ANALYZE,BUFFERS,WAL,VERBOSE,SETTINGS)
SELECT q.id,q.session_id
FROM before_case.worker_queue q
LEFT JOIN before_case.sessions s ON s.session_id=q.session_id AND s.locked_until > :now_ms
WHERE q.visible_at <= to_timestamp(:now_ms/1000.0)
  AND (q.lock_token IS NULL OR q.locked_until <= :now_ms)
  AND (q.session_id IS NULL OR s.worker_id='worker-0' OR s.session_id IS NULL)
  AND q.tag IS NULL
ORDER BY q.id LIMIT 1 FOR UPDATE OF q SKIP LOCKED;
ROLLBACK;
SQL

echo '===== FUTURE visible_at: 1,000,000 future rows + one visible, AFTER phase 1 =====' | tee -a "$RAW"
psql -v ON_ERROR_STOP=1 -X -a -h 127.0.0.1 -U postgres -d bench <<'SQL' 2>&1 | tee -a "$RAW"
SELECT public.reset_future_queue('after_case',1000000);
SELECT count(*) FROM after_case.worker_queue;
SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint AS now_ms \gset
BEGIN;
EXPLAIN (ANALYZE,BUFFERS,WAL,VERBOSE,SETTINGS)
SELECT q.id,q.session_id
FROM after_case.worker_queue q
LEFT JOIN after_case.sessions s ON s.session_id=q.session_id AND s.locked_until > :now_ms
WHERE q.visible_at <= to_timestamp(:now_ms/1000.0)
  AND q.lock_token IS NULL
  AND (q.session_id IS NULL OR s.worker_id='worker-0' OR s.session_id IS NULL)
  AND q.tag IS NULL
ORDER BY q.id LIMIT 1 FOR UPDATE OF q SKIP LOCKED;
ROLLBACK;
SQL

OUT=/tmp/pr18/future-latency.csv
echo 'variant,future_rows,iterations,p50_us,p95_us,mean_us' > "$OUT"
for variant in BEFORE AFTER; do
  if [ "$variant" = BEFORE ]; then schema=before_case; else schema=after_case; fi
  psql -v ON_ERROR_STOP=1 -X -q -h 127.0.0.1 -U postgres -d bench -Atc "SELECT public.reset_future_queue('$schema',1000000);" >/dev/null
  SQLFILE=/tmp/pr18/future-${schema}.sql
  { echo '\timing on'; echo '\o /dev/null'; for i in $(seq 1 200); do
      echo "SELECT out_lock_token FROM $schema.fetch_work_item((extract(epoch FROM clock_timestamp())*1000)::bigint,60000,'worker-0',60000,NULL,'default_only');"
      echo "UPDATE $schema.worker_queue SET lock_token=NULL,locked_until=NULL,attempt_count=attempt_count-1 WHERE lock_token LIKE 'lock_%';"
    done; } > "$SQLFILE"
  psql -v ON_ERROR_STOP=1 -X -h 127.0.0.1 -U postgres -d bench -f "$SQLFILE" 2>&1 | awk '/^Time:/{n++;if(n%2==1)printf "%.3f\n",$2*1000}' > /tmp/pr18/future-${schema}.us
  sort -n /tmp/pr18/future-${schema}.us > /tmp/pr18/future-${schema}.sorted
  p50=$(sed -n '100p' /tmp/pr18/future-${schema}.sorted); p95=$(sed -n '190p' /tmp/pr18/future-${schema}.sorted)
  mean=$(awk '{s+=$1}END{printf "%.3f",s/NR}' /tmp/pr18/future-${schema}.us)
  echo "$variant,1000000,200,$p50,$p95,$mean" >> "$OUT"
done
cat "$OUT"
```

## Reproduced schemas and functions

The following is the server-exported DDL after setup; it is the authoritative record of what was measured.

```sql
--
-- PostgreSQL database dump
--

\restrict yFA5gDEvpKT3F4dR3ubCsFy540TpqYklZdII62Qn4vWaEYrbUUbOLdYuYug1aMD

-- Dumped from database version 16.14 (Debian 16.14-1.pgdg13+1)
-- Dumped by pg_dump version 16.14 (Debian 16.14-1.pgdg13+1)

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

--
-- Name: after_case; Type: SCHEMA; Schema: -; Owner: -
--

CREATE SCHEMA after_case;


--
-- Name: before_case; Type: SCHEMA; Schema: -; Owner: -
--

CREATE SCHEMA before_case;


--
-- Name: fetch_work_item(bigint, bigint, text, bigint, text[], text); Type: FUNCTION; Schema: after_case; Owner: -
--

CREATE FUNCTION after_case.fetch_work_item(p_now_ms bigint, p_lock_timeout_ms bigint, p_owner_id text DEFAULT NULL::text, p_session_lock_timeout_ms bigint DEFAULT NULL::bigint, p_tag_filter text[] DEFAULT NULL::text[], p_tag_mode text DEFAULT 'default_only'::text) RETURNS TABLE(out_work_item text, out_lock_token text, out_attempt_count integer)
    LANGUAGE plpgsql
    AS $$
DECLARE
  v_id bigint;
  v_session_id text;
  v_session_locked_until bigint;
BEGIN
  IF p_tag_mode = 'none' THEN RETURN; END IF;
  IF p_owner_id IS NOT NULL THEN
    SELECT q.id, q.session_id INTO v_id, v_session_id
    FROM after_case.worker_queue q
    LEFT JOIN after_case.sessions s ON s.session_id = q.session_id AND s.locked_until > p_now_ms
    WHERE q.visible_at <= to_timestamp(p_now_ms / 1000.0)
      AND q.lock_token IS NULL
      AND (q.session_id IS NULL OR s.worker_id = p_owner_id OR s.session_id IS NULL)
      AND (CASE p_tag_mode
             WHEN 'default_only' THEN q.tag IS NULL
             WHEN 'tags' THEN q.tag = ANY(p_tag_filter)
             WHEN 'default_and' THEN (q.tag IS NULL OR q.tag = ANY(p_tag_filter))
             WHEN 'any' THEN TRUE ELSE FALSE END)
    ORDER BY q.id LIMIT 1 FOR UPDATE OF q SKIP LOCKED;
    IF NOT FOUND THEN
      SELECT q.id, q.session_id INTO v_id, v_session_id
      FROM after_case.worker_queue q
      LEFT JOIN after_case.sessions s ON s.session_id = q.session_id AND s.locked_until > p_now_ms
      WHERE q.visible_at <= to_timestamp(p_now_ms / 1000.0)
        AND (q.lock_token IS NULL OR q.locked_until <= p_now_ms)
        AND (q.session_id IS NULL OR s.worker_id = p_owner_id OR s.session_id IS NULL)
        AND (CASE p_tag_mode
               WHEN 'default_only' THEN q.tag IS NULL
               WHEN 'tags' THEN q.tag = ANY(p_tag_filter)
               WHEN 'default_and' THEN (q.tag IS NULL OR q.tag = ANY(p_tag_filter))
               WHEN 'any' THEN TRUE ELSE FALSE END)
      ORDER BY q.id LIMIT 1 FOR UPDATE OF q SKIP LOCKED;
    END IF;
  ELSE
    SELECT q.id, q.session_id INTO v_id, v_session_id
    FROM after_case.worker_queue q
    WHERE q.visible_at <= to_timestamp(p_now_ms / 1000.0)
      AND q.lock_token IS NULL AND q.session_id IS NULL
      AND (CASE p_tag_mode
             WHEN 'default_only' THEN q.tag IS NULL
             WHEN 'tags' THEN q.tag = ANY(p_tag_filter)
             WHEN 'default_and' THEN (q.tag IS NULL OR q.tag = ANY(p_tag_filter))
             WHEN 'any' THEN TRUE ELSE FALSE END)
    ORDER BY q.id LIMIT 1 FOR UPDATE OF q SKIP LOCKED;
    IF NOT FOUND THEN
      SELECT q.id, q.session_id INTO v_id, v_session_id
      FROM after_case.worker_queue q
      WHERE q.visible_at <= to_timestamp(p_now_ms / 1000.0)
        AND (q.lock_token IS NULL OR q.locked_until <= p_now_ms)
        AND q.session_id IS NULL
        AND (CASE p_tag_mode
               WHEN 'default_only' THEN q.tag IS NULL
               WHEN 'tags' THEN q.tag = ANY(p_tag_filter)
               WHEN 'default_and' THEN (q.tag IS NULL OR q.tag = ANY(p_tag_filter))
               WHEN 'any' THEN TRUE ELSE FALSE END)
      ORDER BY q.id LIMIT 1 FOR UPDATE OF q SKIP LOCKED;
    END IF;
  END IF;
  IF NOT FOUND THEN RETURN; END IF;
  out_lock_token := 'lock_' || gen_random_uuid()::text;
  UPDATE after_case.worker_queue
  SET lock_token = out_lock_token, locked_until = p_now_ms + p_lock_timeout_ms,
      attempt_count = attempt_count + 1
  WHERE id = v_id;
  SELECT work_item, attempt_count INTO out_work_item, out_attempt_count
  FROM after_case.worker_queue WHERE id = v_id;
  IF v_session_id IS NOT NULL AND p_owner_id IS NOT NULL THEN
    v_session_locked_until := p_now_ms + COALESCE(p_session_lock_timeout_ms, p_lock_timeout_ms);
    INSERT INTO after_case.sessions(session_id, worker_id, locked_until, last_activity_at)
    VALUES(v_session_id, p_owner_id, v_session_locked_until, p_now_ms)
    ON CONFLICT(session_id) DO UPDATE
      SET worker_id = p_owner_id, locked_until = v_session_locked_until,
          last_activity_at = p_now_ms
      WHERE after_case.sessions.locked_until <= p_now_ms OR after_case.sessions.worker_id = p_owner_id;
    IF NOT FOUND THEN
      UPDATE after_case.worker_queue SET lock_token=NULL, locked_until=NULL,
        attempt_count=attempt_count-1 WHERE id=v_id;
      RETURN;
    END IF;
  END IF;
  RETURN NEXT;
END $$;


--
-- Name: fetch_work_item(bigint, bigint, text, bigint, text[], text); Type: FUNCTION; Schema: before_case; Owner: -
--

CREATE FUNCTION before_case.fetch_work_item(p_now_ms bigint, p_lock_timeout_ms bigint, p_owner_id text DEFAULT NULL::text, p_session_lock_timeout_ms bigint DEFAULT NULL::bigint, p_tag_filter text[] DEFAULT NULL::text[], p_tag_mode text DEFAULT 'default_only'::text) RETURNS TABLE(out_work_item text, out_lock_token text, out_attempt_count integer)
    LANGUAGE plpgsql
    AS $$
DECLARE
  v_id bigint;
  v_session_id text;
  v_session_locked_until bigint;
BEGIN
  IF p_tag_mode = 'none' THEN RETURN; END IF;
  IF p_owner_id IS NOT NULL THEN
    SELECT q.id, q.session_id INTO v_id, v_session_id
    FROM before_case.worker_queue q
    LEFT JOIN before_case.sessions s ON s.session_id = q.session_id AND s.locked_until > p_now_ms
    WHERE q.visible_at <= to_timestamp(p_now_ms / 1000.0)
      AND (q.lock_token IS NULL OR q.locked_until <= p_now_ms)
      AND (q.session_id IS NULL OR s.worker_id = p_owner_id OR s.session_id IS NULL)
      AND (CASE p_tag_mode
             WHEN 'default_only' THEN q.tag IS NULL
             WHEN 'tags' THEN q.tag = ANY(p_tag_filter)
             WHEN 'default_and' THEN (q.tag IS NULL OR q.tag = ANY(p_tag_filter))
             WHEN 'any' THEN TRUE ELSE FALSE END)
    ORDER BY q.id LIMIT 1 FOR UPDATE OF q SKIP LOCKED;
  ELSE
    SELECT q.id, q.session_id INTO v_id, v_session_id
    FROM before_case.worker_queue q
    WHERE q.visible_at <= to_timestamp(p_now_ms / 1000.0)
      AND (q.lock_token IS NULL OR q.locked_until <= p_now_ms)
      AND q.session_id IS NULL
      AND (CASE p_tag_mode
             WHEN 'default_only' THEN q.tag IS NULL
             WHEN 'tags' THEN q.tag = ANY(p_tag_filter)
             WHEN 'default_and' THEN (q.tag IS NULL OR q.tag = ANY(p_tag_filter))
             WHEN 'any' THEN TRUE ELSE FALSE END)
    ORDER BY q.id LIMIT 1 FOR UPDATE OF q SKIP LOCKED;
  END IF;
  IF NOT FOUND THEN RETURN; END IF;
  out_lock_token := 'lock_' || gen_random_uuid()::text;
  UPDATE before_case.worker_queue
  SET lock_token = out_lock_token, locked_until = p_now_ms + p_lock_timeout_ms,
      attempt_count = attempt_count + 1
  WHERE id = v_id;
  SELECT work_item, attempt_count INTO out_work_item, out_attempt_count
  FROM before_case.worker_queue WHERE id = v_id;
  IF v_session_id IS NOT NULL AND p_owner_id IS NOT NULL THEN
    v_session_locked_until := p_now_ms + COALESCE(p_session_lock_timeout_ms, p_lock_timeout_ms);
    INSERT INTO before_case.sessions(session_id, worker_id, locked_until, last_activity_at)
    VALUES(v_session_id, p_owner_id, v_session_locked_until, p_now_ms)
    ON CONFLICT(session_id) DO UPDATE
      SET worker_id = p_owner_id, locked_until = v_session_locked_until,
          last_activity_at = p_now_ms
      WHERE before_case.sessions.locked_until <= p_now_ms OR before_case.sessions.worker_id = p_owner_id;
    IF NOT FOUND THEN
      UPDATE before_case.worker_queue SET lock_token=NULL, locked_until=NULL,
        attempt_count=attempt_count-1 WHERE id=v_id;
      RETURN;
    END IF;
  END IF;
  RETURN NEXT;
END $$;


SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: sessions; Type: TABLE; Schema: after_case; Owner: -
--

CREATE TABLE after_case.sessions (
    session_id text NOT NULL,
    worker_id text NOT NULL,
    locked_until bigint NOT NULL,
    last_activity_at bigint NOT NULL
);


--
-- Name: worker_queue; Type: TABLE; Schema: after_case; Owner: -
--

CREATE TABLE after_case.worker_queue (
    id bigint NOT NULL,
    work_item text NOT NULL,
    visible_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP,
    lock_token text,
    locked_until bigint,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP,
    attempt_count integer DEFAULT 0 NOT NULL,
    instance_id text,
    execution_id bigint,
    activity_id bigint,
    session_id text,
    tag text
);


--
-- Name: worker_queue_id_seq; Type: SEQUENCE; Schema: after_case; Owner: -
--

CREATE SEQUENCE after_case.worker_queue_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: worker_queue_id_seq; Type: SEQUENCE OWNED BY; Schema: after_case; Owner: -
--

ALTER SEQUENCE after_case.worker_queue_id_seq OWNED BY after_case.worker_queue.id;


--
-- Name: sessions; Type: TABLE; Schema: before_case; Owner: -
--

CREATE TABLE before_case.sessions (
    session_id text NOT NULL,
    worker_id text NOT NULL,
    locked_until bigint NOT NULL,
    last_activity_at bigint NOT NULL
);


--
-- Name: worker_queue; Type: TABLE; Schema: before_case; Owner: -
--

CREATE TABLE before_case.worker_queue (
    id bigint NOT NULL,
    work_item text NOT NULL,
    visible_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP,
    lock_token text,
    locked_until bigint,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP,
    attempt_count integer DEFAULT 0 NOT NULL,
    instance_id text,
    execution_id bigint,
    activity_id bigint,
    session_id text,
    tag text
);


--
-- Name: worker_queue_id_seq; Type: SEQUENCE; Schema: before_case; Owner: -
--

CREATE SEQUENCE before_case.worker_queue_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: worker_queue_id_seq; Type: SEQUENCE OWNED BY; Schema: before_case; Owner: -
--

ALTER SEQUENCE before_case.worker_queue_id_seq OWNED BY before_case.worker_queue.id;


--
-- Name: worker_queue id; Type: DEFAULT; Schema: after_case; Owner: -
--

ALTER TABLE ONLY after_case.worker_queue ALTER COLUMN id SET DEFAULT nextval('after_case.worker_queue_id_seq'::regclass);


--
-- Name: worker_queue id; Type: DEFAULT; Schema: before_case; Owner: -
--

ALTER TABLE ONLY before_case.worker_queue ALTER COLUMN id SET DEFAULT nextval('before_case.worker_queue_id_seq'::regclass);


--
-- Name: sessions sessions_pkey; Type: CONSTRAINT; Schema: after_case; Owner: -
--

ALTER TABLE ONLY after_case.sessions
    ADD CONSTRAINT sessions_pkey PRIMARY KEY (session_id);


--
-- Name: worker_queue worker_queue_pkey; Type: CONSTRAINT; Schema: after_case; Owner: -
--

ALTER TABLE ONLY after_case.worker_queue
    ADD CONSTRAINT worker_queue_pkey PRIMARY KEY (id);


--
-- Name: sessions sessions_pkey; Type: CONSTRAINT; Schema: before_case; Owner: -
--

ALTER TABLE ONLY before_case.sessions
    ADD CONSTRAINT sessions_pkey PRIMARY KEY (session_id);


--
-- Name: worker_queue worker_queue_pkey; Type: CONSTRAINT; Schema: before_case; Owner: -
--

ALTER TABLE ONLY before_case.worker_queue
    ADD CONSTRAINT worker_queue_pkey PRIMARY KEY (id);


--
-- Name: idx_worker_available; Type: INDEX; Schema: after_case; Owner: -
--

CREATE INDEX idx_worker_available ON after_case.worker_queue USING btree (lock_token, id);


--
-- Name: idx_worker_queue_session_id; Type: INDEX; Schema: after_case; Owner: -
--

CREATE INDEX idx_worker_queue_session_id ON after_case.worker_queue USING btree (session_id);


--
-- Name: idx_worker_queue_tag; Type: INDEX; Schema: after_case; Owner: -
--

CREATE INDEX idx_worker_queue_tag ON after_case.worker_queue USING btree (tag);


--
-- Name: idx_worker_ready; Type: INDEX; Schema: after_case; Owner: -
--

CREATE INDEX idx_worker_ready ON after_case.worker_queue USING btree (id) WHERE (lock_token IS NULL);


--
-- Name: idx_worker_visible; Type: INDEX; Schema: after_case; Owner: -
--

CREATE INDEX idx_worker_visible ON after_case.worker_queue USING btree (visible_at, lock_token);


--
-- Name: idx_worker_available; Type: INDEX; Schema: before_case; Owner: -
--

CREATE INDEX idx_worker_available ON before_case.worker_queue USING btree (lock_token, id);


--
-- Name: idx_worker_queue_session_id; Type: INDEX; Schema: before_case; Owner: -
--

CREATE INDEX idx_worker_queue_session_id ON before_case.worker_queue USING btree (session_id);


--
-- Name: idx_worker_queue_tag; Type: INDEX; Schema: before_case; Owner: -
--

CREATE INDEX idx_worker_queue_tag ON before_case.worker_queue USING btree (tag);


--
-- Name: idx_worker_visible; Type: INDEX; Schema: before_case; Owner: -
--

CREATE INDEX idx_worker_visible ON before_case.worker_queue USING btree (visible_at, lock_token);


--
-- PostgreSQL database dump complete
--

\unrestrict yFA5gDEvpKT3F4dR3ubCsFy540TpqYklZdII62Qn4vWaEYrbUUbOLdYuYug1aMD

CREATE OR REPLACE FUNCTION public.create_queue_schema(p_schema text, p_ready_index boolean)
 RETURNS void
 LANGUAGE plpgsql
AS $function$
BEGIN
  EXECUTE format('DROP TABLE IF EXISTS %I.worker_queue CASCADE', p_schema);
  EXECUTE format('DROP TABLE IF EXISTS %I.sessions CASCADE', p_schema);
  EXECUTE format('CREATE TABLE %I.sessions (session_id text PRIMARY KEY, worker_id text NOT NULL, locked_until bigint NOT NULL, last_activity_at bigint NOT NULL)', p_schema);
  EXECUTE format($q$CREATE TABLE %I.worker_queue (
      id bigserial PRIMARY KEY,
      work_item text NOT NULL,
      visible_at timestamptz DEFAULT CURRENT_TIMESTAMP,
      lock_token text,
      locked_until bigint,
      created_at timestamptz DEFAULT CURRENT_TIMESTAMP,
      attempt_count integer NOT NULL DEFAULT 0,
      instance_id text,
      execution_id bigint,
      activity_id bigint,
      session_id text,
      tag text
  )$q$, p_schema);
  EXECUTE format('CREATE INDEX idx_worker_visible ON %I.worker_queue(visible_at, lock_token)', p_schema);
  EXECUTE format('CREATE INDEX idx_worker_available ON %I.worker_queue(lock_token, id)', p_schema);
  EXECUTE format('CREATE INDEX idx_worker_queue_session_id ON %I.worker_queue(session_id)', p_schema);
  EXECUTE format('CREATE INDEX idx_worker_queue_tag ON %I.worker_queue(tag)', p_schema);
  IF p_ready_index THEN
    EXECUTE format('CREATE INDEX idx_worker_ready ON %I.worker_queue(id) WHERE lock_token IS NULL', p_schema);
  END IF;
END $function$

CREATE OR REPLACE FUNCTION public.reset_queue(p_schema text, p_ready_backlog bigint, p_inflight bigint, p_placement text)
 RETURNS void
 LANGUAGE plpgsql
AS $function$
DECLARE
  v_now_ms bigint := (extract(epoch from clock_timestamp()) * 1000)::bigint;
  v_total bigint := p_ready_backlog + p_inflight;
BEGIN
  EXECUTE format('TRUNCATE %I.worker_queue RESTART IDENTITY; TRUNCATE %I.sessions', p_schema, p_schema);
  EXECUTE format($q$
    INSERT INTO %I.worker_queue(work_item, visible_at, lock_token, locked_until, created_at,
                                attempt_count, session_id, tag)
    SELECT json_build_object('id', g)::text,
           clock_timestamp() - interval '1 second',
           CASE WHEN CASE
             WHEN $3 = 'head' THEN g <= $2
             WHEN $3 = 'interleaved' THEN (g %% 2 = 1 AND ((g + 1) / 2) <= $2)
             ELSE false END
           THEN 'claimed-' || g ELSE NULL END,
           CASE WHEN CASE
             WHEN $3 = 'head' THEN g <= $2
             WHEN $3 = 'interleaved' THEN (g %% 2 = 1 AND ((g + 1) / 2) <= $2)
             ELSE false END
           THEN $4 + 3600000 ELSE NULL END,
           clock_timestamp() - interval '1 second', 0,
           CASE WHEN g %% 1000 = 0 THEN 'session-' || g ELSE NULL END,
           NULL
    FROM generate_series(1, $1) g$q$, p_schema)
    USING v_total, p_inflight, p_placement, v_now_ms;
  EXECUTE format($q$
    INSERT INTO %I.sessions(session_id, worker_id, locked_until, last_activity_at)
    SELECT DISTINCT session_id, 'worker-0', $1 + 3600000, $1
    FROM %I.worker_queue WHERE session_id IS NOT NULL$q$, p_schema, p_schema)
    USING v_now_ms;
  EXECUTE format('ANALYZE %I.worker_queue; ANALYZE %I.sessions', p_schema, p_schema);
END $function$

CREATE OR REPLACE FUNCTION public.measure_latency(p_variant text, p_ready_backlog bigint, p_inflight bigint, p_placement text, p_iterations integer DEFAULT 1000)
 RETURNS void
 LANGUAGE plpgsql
AS $function$
DECLARE
  i int; v_now_ms bigint; v_token text; t0 timestamptz; elapsed_us numeric;
BEGIN
  PERFORM public.reset_queue(CASE WHEN p_variant='BEFORE' THEN 'before_case' ELSE 'after_case' END,
                             p_ready_backlog, p_inflight, p_placement);
  CREATE TEMP TABLE IF NOT EXISTS latency_samples(us numeric) ON COMMIT PRESERVE ROWS;
  TRUNCATE latency_samples;
  FOR i IN 1..50 LOOP
    v_now_ms := (extract(epoch from clock_timestamp()) * 1000)::bigint;
    IF p_variant='BEFORE' THEN
      SELECT out_lock_token INTO v_token FROM before_case.fetch_work_item(v_now_ms,60000,'worker-0',60000,NULL,'default_only');
      UPDATE before_case.worker_queue SET lock_token=NULL, locked_until=NULL, attempt_count=attempt_count-1 WHERE lock_token=v_token;
    ELSE
      SELECT out_lock_token INTO v_token FROM after_case.fetch_work_item(v_now_ms,60000,'worker-0',60000,NULL,'default_only');
      UPDATE after_case.worker_queue SET lock_token=NULL, locked_until=NULL, attempt_count=attempt_count-1 WHERE lock_token=v_token;
    END IF;
  END LOOP;
  FOR i IN 1..p_iterations LOOP
    v_now_ms := (extract(epoch from clock_timestamp()) * 1000)::bigint;
    t0 := clock_timestamp();
    IF p_variant='BEFORE' THEN
      SELECT out_lock_token INTO v_token FROM before_case.fetch_work_item(v_now_ms,60000,'worker-0',60000,NULL,'default_only');
      elapsed_us := extract(epoch FROM clock_timestamp()-t0)*1000000;
      UPDATE before_case.worker_queue SET lock_token=NULL, locked_until=NULL, attempt_count=attempt_count-1 WHERE lock_token=v_token;
    ELSE
      SELECT out_lock_token INTO v_token FROM after_case.fetch_work_item(v_now_ms,60000,'worker-0',60000,NULL,'default_only');
      elapsed_us := extract(epoch FROM clock_timestamp()-t0)*1000000;
      UPDATE after_case.worker_queue SET lock_token=NULL, locked_until=NULL, attempt_count=attempt_count-1 WHERE lock_token=v_token;
    END IF;
    INSERT INTO latency_samples VALUES(elapsed_us);
  END LOOP;
  INSERT INTO public.latency_results(variant,ready_backlog,locked,placement,iterations,p50_us,p95_us,mean_us,min_us,max_us)
  SELECT p_variant,p_ready_backlog,p_inflight,p_placement,p_iterations,
         percentile_cont(0.5) WITHIN GROUP (ORDER BY us),
         percentile_cont(0.95) WITHIN GROUP (ORDER BY us),avg(us),min(us),max(us)
  FROM latency_samples;
END $function$

CREATE OR REPLACE FUNCTION public.reset_future_queue(p_schema text, p_future bigint)
 RETURNS void
 LANGUAGE plpgsql
AS $function$
BEGIN
  EXECUTE format('TRUNCATE %I.worker_queue RESTART IDENTITY; TRUNCATE %I.sessions',p_schema,p_schema);
  EXECUTE format($q$
    INSERT INTO %I.worker_queue(work_item,visible_at,created_at,lock_token,locked_until,session_id,tag)
    SELECT json_build_object('future',g)::text,clock_timestamp()+interval '1 hour',clock_timestamp(),NULL,NULL,NULL,NULL
    FROM generate_series(1,$1) g$q$,p_schema) USING p_future;
  EXECUTE format($q$
    INSERT INTO %I.worker_queue(work_item,visible_at,created_at,lock_token,locked_until,session_id,tag)
    VALUES ('{"visible":true}',clock_timestamp()-interval '1 second',clock_timestamp(),NULL,NULL,NULL,NULL)$q$,p_schema);
  EXECUTE format('ANALYZE %I.worker_queue',p_schema);
END $function$

```
