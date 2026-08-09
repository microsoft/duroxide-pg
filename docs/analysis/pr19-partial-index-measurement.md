# duroxide-pg PR #19 — Partial Secondary Index Measurement

Measured effect of making three secondary indexes partial (`WHERE ... IS NOT NULL`)
in `duroxide-pg`. Uses the real, unmodified migration files from the repository.

- Repo: `microsoft/duroxide-pg`
- Baseline ref: `main@26fd4a37019a72e2b8ce2905fc97479061131d57` — migrations `0001`
  through `0021`. There is no `0022` on `main`. PR #18's `0022` is not part of this
  test and was not applied.
- PR19 ref: `46a8bf1deb127b6e10fe2d5ae50a068ff8cd4162` — the same `0001`-`0021`, plus
  `migrations/0023_partial_secondary_indexes.sql` taken from that ref.
- Files were extracted with `git show <ref>:migrations/<file>` and applied with
  `psql` in version order, unmodified. `search_path` was set to `public` per schema
  so the migrations' `current_schema()` / `format(%I)` logic worked without edits.

## 1. Environment

| Item | Value |
|---|---|
| PostgreSQL server | 16.14, single instance |
| PostgreSQL server pod | `waldemort-pr19-pg16`, 4 vCPU, 6 GiB memory |
| Storage | Container/ephemeral disk (no PVC attached) |
| Driver pod | `waldemort-pr19-driver`, 4 vCPU, 4 GiB memory |
| Client tools | psql/pgbench 17.10 client against a 16.14 server (version skew is benign) |
| Server config | `shared_buffers=1GB`, `max_wal_size=4GB`, `max_connections=200` |
| Database | `bench`, two schemas: `baseline` (full B-tree indexes) and `pr19` (partial indexes per 0023) |
| Region | westus3 (same pod, same server, used for every condition — no mid-campaign migration) |

Both schemas were built once in the same database from the same migration files,
so baseline and PR19 coexist without re-running migrations between measurements.

## 2. Author's benchmark script — assessment

`scripts/bench-secondary-indexes.sh` (at the PR19 ref) builds two throwaway
databases, `sec_base` (`0021`) and `sec_fix` (`0023` applied), loads `worker_queue`
with about 95% of `worker_queue` rows having a NULL `session_id`/`tag`, and about
90% of `orchestrator_queue` rows having a NULL `lock_token`. It reports secondary
index size and bulk-insert time for each.

**Assessment: the method is sound for what it measures, but it measures only
half the claim.** It checks bulk-insert time and index size at one NULL share
(about 95% NULL).
It does not run any read query, does not vary concurrency, and does not check
the NULL-seeking branch of `fetch_work_item`. This is not enough to validate the
author's "no read-path regression" claim — the regression found below (Section 5)
could not have been caught by this script.

## 3. Sweep parameters

| Parameter | Value |
|---|---|
| `worker_queue` rows | 500,000 |
| `orchestrator_queue` rows | 100,000 |
| NULL share tested | 100%, 99%, 95%, 75%, 50%, 10%, 0% |
| pgbench clients tested | 1, 8, 32 |
| pgbench run duration | 15 s per client count |
| Churn-loop duration | 8 s |
| Read-latency samples | 300 per query per NULL share |

**NULL share** means the share of `worker_queue` rows whose `session_id` and
`tag` are NULL. For `orchestrator_queue` it means the share of rows whose
`lock_token` is NULL. NULL rows are the rows the partial indexes leave out.
A high NULL share means most rows are generic, untagged work. A low NULL share
means most rows are bound to a session or a tag. About 95% NULL is the shape
used in the author's own script.

**Generator caveat:** the data generator used modulo arithmetic to decide which
rows get a value. Because `gcd(7,100)=1`, the two column masks are a fixed
function of each other. This can under-count or over-count the true overlap at
the extreme shares. The problem was found during the threshold work in
Section 5, and that point was re-tested with independent random sampling at 1%
NULL. The seven main shares reported here were run as specified. The write-path
numbers are not affected. Only the fine threshold claim in Section 5 needed a
re-check.

## 4. Write-path results

### 4.1 Bulk load time and WAL

| NULL share | Schema | Load time (s) | WAL for load (MB) |
|---:|---|---:|---:|
| 100% | baseline | 3.994 | 299.7 |
| 100% | pr19 | 3.169 | 228.0 |
| 99% | baseline | 3.953 | 299.6 |
| 99% | pr19 | 3.124 | 229.2 |
| 95% | baseline | 4.134 | 301.4 |
| 95% | pr19 | 3.276 | 232.7 |
| 75% | baseline | 4.544 | 305.4 |
| 75% | pr19 | 3.683 | 252.0 |
| 50% | baseline | 4.797 | 311.4 |
| 50% | pr19 | 4.386 | 275.6 |
| 10% | baseline | 5.667 | 320.8 |
| 10% | pr19 | 5.569 | 316.9 |
| 0% | baseline | 5.980 | 323.4 |
| 0% | pr19 | 5.899 | 323.4 |

Load time and WAL bytes for PR19 are lower at every NULL share above 0%. The
gap shrinks as the NULL share falls. At 0% NULL the two are equal (323.4 MB WAL
for both). This confirms the author's claim on the write side. The author's own
shape, about 95% NULL, is reproduced here directly.

### 4.2 Index size (KB), by index

| NULL share | Schema | session_id idx | tag idx | lock_token idx | Total |
|---:|---|---:|---:|---:|---:|
| 100% | baseline | 3,168 | 3,168 | 648 | 6,984 |
| 100% | pr19 | 8 | 8 | 8 | 24 |
| 99% | baseline | 3,184 | 3,168 | 688 | 7,040 |
| 99% | pr19 | 64 | 48 | 64 | 176 |
| 95% | baseline | 3,264 | 3,168 | 872 | 7,304 |
| 95% | pr19 | 272 | 176 | 264 | 712 |
| 75% | baseline | 3,536 | 3,184 | 1,792 | 8,512 |
| 75% | pr19 | 1,192 | 816 | 1,304 | 3,312 |
| 50% | baseline | 4,016 | 3,200 | 2,952 | 10,168 |
| 50% | pr19 | 2,336 | 1,616 | 2,624 | 6,576 |
| 10% | baseline | 4,424 | 3,232 | 4,768 | 12,424 |
| 10% | pr19 | 4,128 | 2,912 | 4,728 | 11,768 |
| 0% | baseline | 4,520 | 3,232 | 5,408 | 13,160 |
| 0% | pr19 | 4,520 | 3,232 | 5,408 | 13,160 |

At 100% NULL, PR19's total index size is 24 KB against baseline's 6,984 KB.
That is about 290 times smaller. The gap closes steadily as the NULL share
falls. **It becomes exactly zero at 0% NULL**: 13,160 KB for both. This is
expected. A partial index that leaves out NULL rows is the same as a full index
once no row is NULL.

**The index-size benefit disappears at 0% NULL, and only there.** It is present,
at shrinking size, at every NULL share above 0%.

### 4.3 Steady-state single-row INSERT throughput (pgbench, TPS)

| NULL share | Schema | 1 client | 8 clients | 32 clients |
|---:|---|---:|---:|---:|
| 100% | baseline | 1,714 | 12,284 | 21,590 |
| 100% | pr19 | 1,693 | 11,060 | 21,606 |
| 99% | baseline | 1,421 | 11,929 | 21,135 |
| 99% | pr19 | 1,689 | 11,901 | 21,727 |
| 95% | baseline | 2,072 | 11,289 | 20,838 |
| 95% | pr19 | 2,113 | 11,643 | 21,219 |
| 75% | baseline | 1,425 | 11,078 | 20,322 |
| 75% | pr19 | 1,429 | 11,786 | 20,986 |
| 50% | baseline | 1,627 | 11,138 | 18,862 |
| 50% | pr19 | 1,875 | 10,314 | 19,668 |
| 10% | baseline | 1,344 | 11,048 | 17,979 |
| 10% | pr19 | 1,967 | 10,177 | 17,883 |
| 0% | baseline | 1,382 | 10,378 | 18,017 |
| 0% | pr19 | 1,606 | 10,448 | 17,423 |

**No consistent, repeatable TPS difference** between schemas at any NULL share
or client count. Deltas move in both directions by a few percent — this is normal
run-to-run noise, not a signal. The write-path benefit measured here is an
index-size and WAL-volume benefit, not a steady-state single-row INSERT
throughput benefit, for this workload shape (500,000-row table, single-row
INSERT after bulk load).

### 4.4 HOT update ratio — could not be measured

`n_tup_hot_upd` was 0 in every sample, for both schemas, at all 7 NULL shares.
Cause: freshly bulk-loaded pages use the default `fillfactor=100` (no reserved
free space per page), so HOT updates are structurally impossible immediately
after a bulk load — independent of index partiality. This affects both schemas
equally, so it is not a source of bias. But it means the HOT update and
autovacuum effect could not be isolated with this test design.

## 5. Read-path results

### 5.1 Named equality/ANY lookups — no regression

Three named lookup queries were checked:

- `SELECT ... WHERE session_id = <existing id>`
- `SELECT ... WHERE tag = ANY(ARRAY[<tags>])`
- `SELECT/DELETE ... WHERE lock_token = <token>`

| NULL share | Schema | session_id p50/p95 (ms) | tag ANY p50/p95 (ms) | lock_token p50/p95 (ms) |
|---:|---|---|---|---|
| 99% | baseline | 16.545 / 19.760 | 12.695 / 15.735 | 0.383 / 0.467 |
| 99% | pr19 | 1.681 / 2.273 | 13.256 / 17.396 | 0.395 / 0.467 |
| 95% | baseline | 16.278 / 19.428 | 83.813 / 100.386 | 0.467 / 0.527 |
| 95% | pr19 | 15.798 / 19.438 | 14.007 / 19.541 | 0.580 / 0.667 |
| 75% | baseline | 14.702 / 17.905 | 219.496 / 242.185 | 0.481 / 0.542 |
| 75% | pr19 | 16.031 / 18.450 | 226.506 / 249.206 | 0.495 / 0.575 |
| 50% | baseline | 15.126 / 17.135 | 329.344 / 384.379 | 0.370 / 0.457 |
| 50% | pr19 | 15.436 / 18.086 | 326.036 / 352.790 | 0.485 / 0.553 |
| 10% | baseline | 14.714 / 19.085 | 549.274 / 596.568 | 0.489 / 0.562 |
| 10% | pr19 | 15.006 / 17.380 | 552.303 / 624.088 | 0.488 / 0.565 |
| 0% | baseline | 14.264 / 17.710 | 608.074 / 655.073 | 0.590 / 0.673 |
| 0% | pr19 | 14.706 / 18.114 | 607.160 / 648.393 | 0.598 / 0.693 |

The plan node for these queries did not change between schemas at any NULL
share. `session_id` used a Bitmap Heap Scan in both. `tag` used an Index Scan at
99% NULL and a Bitmap Heap Scan at 95% NULL and below, in both. `lock_token`
used an Index Scan in both. Latencies track each other within noise. The `tag`
lookup gets slower as the NULL share falls, because `tag = ANY([...])` then
matches more rows. That rise is the same in both schemas. It is not a PR19
effect.

**No read-path regression for these three named queries at any tested NULL
share.**

### 5.2 The actual regression — `fetch_work_item`'s NULL-seeking branch

The real source was searched for `IS NULL`, anti-join, and `NOT EXISTS` use on
the three affected columns. This is what was found:

`migrations/0016_add_activity_tags.sql`, function `fetch_work_item`, the
non-session branch (used whenever a worker polls with `p_owner_id IS NULL` —
the default/no-session poll):

```sql
-- migrations/0016_add_activity_tags.sql, lines 103-119
ELSE
    -- Non-session fetch with tag filtering
    SELECT q.id, q.session_id INTO v_id, v_session_id
    FROM %I.worker_queue q
    WHERE q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
      AND (q.lock_token IS NULL OR q.locked_until <= p_now_ms)
      AND q.session_id IS NULL
      AND (
        CASE p_tag_mode
            WHEN ''default_only'' THEN q.tag IS NULL
            WHEN ''tags'' THEN q.tag = ANY(p_tag_filter)
            WHEN ''default_and'' THEN (q.tag IS NULL OR q.tag = ANY(p_tag_filter))
            WHEN ''any'' THEN TRUE
            ELSE FALSE
        END
      )
    ORDER BY q.id
    LIMIT 1
    FOR UPDATE OF q SKIP LOCKED;
END IF;
```

Exact citations:

- `0016_add_activity_tags.sql:85` — `q.session_id IS NULL` (session-aware branch, non-matched session case)
- `0016_add_activity_tags.sql:91` — `tag_mode=default_only`: `q.tag IS NULL`
- `0016_add_activity_tags.sql:93` — `tag_mode=default_and`: `q.tag IS NULL OR q.tag = ANY(...)`
- `0016_add_activity_tags.sql:107` — `q.session_id IS NULL` (non-session branch, shown above)
- `0016_add_activity_tags.sql:110,112` — same `tag_mode` cases repeated in the non-session branch
- `0014_add_session_support.sql:94,96,107` — same predicate shape, earlier version of this function, superseded by 0016 but the same pattern

This is the exact inverse of what the partial indexes cover
(`WHERE session_id IS NOT NULL`, `WHERE tag IS NOT NULL`). A partial index that
excludes NULL rows cannot be used to answer "find me a row where this column
IS NULL" — Postgres cannot prove the index covers all such rows.

Other paths checked, confirmed safe: `ack_orchestration_item` /
`abandon_orchestration_item` / the orchestrator DELETE path use plain
`lock_token = <value>` equality, which the partial index serves fine. The Rust
client (`provider.rs`) only issues equality binds — no NULL-seeking SQL
generated from Rust.

### 5.3 Confirmed regression — EXPLAIN evidence

Direct EXPLAIN (ANALYZE, BUFFERS) of the exact non-session predicate from
Section 5.2, at 0% NULL. Every row has a `session_id` and a `tag`, so no row can
match `session_id IS NULL AND tag IS NULL`:

**Baseline** (full B-tree indexes):

```
Index Scan using idx_worker_queue_tag on worker_queue q
  Index Cond: (tag IS NULL)
  Filter: ((session_id IS NULL) AND (visible_at <= ...) AND ((lock_token IS NULL) OR (locked_until <= ...)))
  Rows Removed by Filter: 0
  Buffers: shared hit=6
Planning Time: ...
Execution Time: 0.097 ms
```

Baseline can prove the answer set is empty directly from the B-tree's NULL
entries — 6 buffer hits, 0.097 ms.

**PR19** (partial indexes):

```
Bitmap Heap Scan on worker_queue q
  Recheck Cond: (visible_at <= ...)
  Filter: ((session_id IS NULL) AND (tag IS NULL) AND ((lock_token IS NULL) OR (locked_until <= ...)))
  Rows Removed by Filter: 440254
  Heap Blocks: exact=5052
  ->  Bitmap Index Scan on idx_worker_visible
  Buffers: shared hit=8069
Planning Time: ...
Execution Time: 62.252 ms
```

PR19 cannot use `idx_worker_queue_session_id` or `idx_worker_queue_tag` for
this query (both exclude NULLs), so it falls back to the visible-rows index
and filters every candidate row in the heap.

| Metric | Baseline | PR19 | Ratio |
|---|---:|---:|---:|
| Execution time | 0.097 ms | 62.252 ms | ~642x slower |
| Buffer hits | 6 | 8,069 | ~1,345x more |
| Rows removed by filter | 0 | 440,254 | — |

This is a genuine plan regression: **PR19 produces a seq/bitmap-heap scan
where baseline used an index scan, specifically for this query shape.**

### 5.4 Where the regression appears — threshold investigation

The proxy metric (`dequeue_churn`, which repeatedly calls `fetch_work_item`
with the default no-session poll for 8 seconds) corroborates this directly:

| NULL share | Schema | Iterations in 8s |
|---:|---|---:|
| 0% | baseline | 16,223 |
| 0% | pr19 | 127 |

A ~128x throughput collapse for the identical call pattern.

To find the exact trigger, the 50% and 10% NULL shares from the sweep, and a
separate 1% NULL re-test, were checked with EXPLAIN:

| NULL share | Rows matching `session_id IS NULL AND tag IS NULL` | Plan used (both schemas) | Regression? |
|---:|---:|---|---|
| 10% | about 5,000 | Primary-key index scan by id order, match found quickly | No |
| 1%* | 48 (independent-random re-test) | Primary-key index scan, match found quickly | No |
| 0% | 0 | Baseline: index scan proving the set is empty. PR19: full bitmap heap scan | **Yes — confirmed above** |

\* The modulo generator produced exactly 0 matches at 1% NULL. That is an
artifact of the generator, not a real shape (see Section 3). The data was
reloaded with independent random assignment at 1% NULL. That gave 48 real
matches. Both schemas answered in about 1.3 to 1.5 ms using the primary-key
index. There is no regression when real matches exist.

**Conclusion: the regression does not follow from "the NULL share is low." It
appears when the set of rows with `session_id IS NULL AND tag IS NULL` is
empty.** That is, when the queue holds no generic, untagged work and a worker
still polls for it. This gets more likely as the NULL share falls toward zero.
But the trigger is "no matching row exists", not a fixed percentage.

## 6. Migration window

Migration `0023`'s DDL, unmodified:

```sql
DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    EXECUTE format('DROP INDEX IF EXISTS %1$I.idx_worker_queue_session_id', v_schema_name);
    EXECUTE format('CREATE INDEX idx_worker_queue_session_id ON %1$I.worker_queue (session_id) WHERE session_id IS NOT NULL', v_schema_name);

    EXECUTE format('DROP INDEX IF EXISTS %1$I.idx_worker_queue_tag', v_schema_name);
    EXECUTE format('CREATE INDEX idx_worker_queue_tag ON %1$I.worker_queue (tag) WHERE tag IS NOT NULL', v_schema_name);

    EXECUTE format('DROP INDEX IF EXISTS %1$I.idx_orch_lock', v_schema_name);
    EXECUTE format('CREATE INDEX idx_orch_lock ON %1$I.orchestrator_queue (lock_token) WHERE lock_token IS NOT NULL', v_schema_name);
END $$;
```

All six statements (`DROP INDEX` + `CREATE INDEX`, both non-concurrent, ×3) run
inside one implicit transaction. `DROP INDEX` (non-concurrent) requires an
ACCESS EXCLUSIVE lock on the table; Postgres does not downgrade a lock
mid-transaction, so the whole transaction holds ACCESS EXCLUSIVE until commit.

Test setup: two fresh scratch schemas, each built from the real `0001`-`0021`
migrations, loaded with 500,000 `worker_queue` rows and 100,000
`orchestrator_queue` rows at the author's shape, about 95% NULL.

| Test | Migration wall time | Concurrent client | Baseline op latency (p50/p95) | Observed stall |
|---|---:|---|---:|---:|
| Writer probe | 0.120 s | Single-row INSERT loop against `worker_queue` | 0.557 ms / 0.658 ms | 116.4 ms |
| Reader probe | 0.102 s | `SELECT ... WHERE session_id = ...` loop | 0.561 ms / 0.635 ms | 98.5 ms |

Both the writer and the reader probes recorded exactly one call blocked for
approximately the migration's own wall-clock duration. **Reads are blocked,
not just writes** — this confirms an ACCESS EXCLUSIVE lock, not a weaker one
(a ShareLock, for example, would have let reads through).

### 6.1 Why `CONCURRENTLY` cannot be used as written

`CREATE INDEX CONCURRENTLY` and `DROP INDEX CONCURRENTLY` cannot run inside a
transaction block. The migration wraps all six statements in a `DO $$` block,
which is one transaction. So the concurrent forms cannot be added to this file
as it stands. Removing the stall would need the `DO $$` block to be replaced by
plain top-level statements, and the migration runner would have to run them
outside a transaction. That is a structural change, not a one-word edit.

**Migration window conclusion:** There is no client-visible "unindexed" state.
Clients observe either the pre-migration or post-migration index set — never a
gap. The cost is a hard, brief stall (locked rows and queries alike are
blocked, roughly 100–120 ms at 500,000/100,000 rows) rather than a period of
degraded query plans.

## 7. The live Waldemort control-plane database

The synthetic sweep above uses 500,000 and 100,000 rows. The real database this
cluster runs on does not look like that. It was read directly on 2026-08-09
(HorizonDB `wdmwaldemortchk-hdb`, schema `pilot_duroxide`).

| Item | Value |
|---|---|
| `worker_queue` live rows | 3 |
| `orchestrator_queue` live rows | 26 |
| `sessions` live rows | 11 |
| `worker_queue` rows inserted / deleted, cumulative | 779,972 / 779,972 |
| `orchestrator_queue` rows inserted / deleted, cumulative | 953,045 / 953,059 |
| `idx_worker_queue_session_id` size | 16 kB |
| `idx_worker_queue_tag` size | 16 kB |
| `idx_orch_lock` size | 184 kB |

The queues are shallow and have very high churn. Every row is inserted, worked,
and deleted. The tables never grow. So the two indexes that PR #19 changes on
`worker_queue` are 16 kB each.

**On this workload the measured benefit of PR #19 is close to zero.** The patch
would save about 16 kB of index space, while adding the read-path risk described
in Section 5 to the poll path. The write-path gain measured in Section 4 needs a
large, deep queue to matter. This queue is never deep.

Two unrelated findings came out of the same reading. They look more valuable
than either PR, and are worth separate work:

- `idx_worker_queue_tag` has read 1,088,773,329 tuples across 14,175,847 scans
  and returned only 612,817. That is about 77 tuples read per scan.
- `orchestrator_queue` has a HOT update ratio of 0.0%, 5 out of 1,736,049
  updates. Every update rewrites all index entries. For comparison, `sessions`
  is at 99.4%.

## 8. Verdict

**Write-path claim: confirmed.** Index size drops by about 290 times at a high
NULL share. The benefit shrinks to zero at 0% NULL, because a partial index
becomes a full index once nothing is NULL. WAL volume for bulk load is lower
for PR19 at every NULL share above 0%. Steady-state single-row INSERT
throughput shows no measurable, repeatable difference at any NULL share or
client count in this test. The write-side benefit is index size and WAL volume,
not TPS.

**Read-path claim: partly contradicted.** The three named equality lookup
queries show no regression at any NULL share. But the real hot-path query, the
default poll branch of `fetch_work_item`, seeks NULL rows. The new partial
indexes cannot serve it at all. Confirmed at 0% NULL: execution time rises from
0.097 ms to 62.252 ms, about 642 times slower, with about 1,345 times more
buffer hits. The churn-loop proxy shows the same effect, a drop from 16,223 to
127 iterations in 8 seconds. This is a real risk. The author's benchmark did
not test it and could not have found it.

**Where the write-path benefit disappears:** at 0% NULL, by construction.

**Where the read-path regression appears:** when the set of rows matching
`session_id IS NULL AND tag IS NULL` is empty. This was verified at 0% NULL. At
1% NULL with real matches present, there was no regression. The empty case gets
more likely as the NULL share falls toward zero. But the true trigger is "no
generic work exists", not a fixed percentage.

**Migration window claim:** Migration `0023` itself completes in ~0.10–0.12 s
at 500,000/100,000 rows but blocks all table access (reads and writes) for
that entire duration via an ACCESS EXCLUSIVE lock, since it uses non-concurrent
`DROP INDEX` / `CREATE INDEX`. Observed single-operation stalls: 116.4 ms
(write), 98.5 ms (read). This is a short, all-or-nothing outage — not a window
of degraded plans.

## 9. Measurement limitations

- **HOT update ratio could not be isolated.** `n_tup_hot_upd` was 0 in every
  sample, in both schemas, at all 7 NULL shares. The cause is `fillfactor=100`
  on freshly bulk-loaded pages. It affects both schemas equally, so it is not a
  bias. But this metric gave no usable signal.
- **The churn-loop collapse at 10% and 0% NULL happens in both schemas.** The
  harness always polls with `p_owner_id=NULL` and the default tag mode. At a low
  NULL share, few rows match that by construction, so most calls correctly find
  nothing. The real signal is the difference in cost between the two schemas
  when both hit the same "nothing found" case: 16,223 against 127 iterations in
  8 seconds at 0% NULL. The raw collapse by itself is not the signal.
- **Steady-state TPS differences are within normal run-to-run noise**, a few
  percent in either direction. No firm throughput win or loss was established
  for single-row INSERT in this workload shape.
- **The data generator distorts the extreme shares** (Section 3). The 1% NULL
  point was re-tested with independent random sampling to get a representative
  result.
