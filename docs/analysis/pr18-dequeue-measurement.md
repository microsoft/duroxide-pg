# Does PR #18 make dequeue faster? A measurement

This document reports a performance measurement of pull request #18 in the
`microsoft/duroxide-pg` repository. PR #18 changes how a worker takes the next
item from the `worker_queue` table.

The document uses simple technical English. It uses the term **locked rows**
for rows that a worker currently holds.

## 1. Short answer

- In PG approx <p_now_ms> is passed as a QP parameter from the client's clock,
  that varies with the database's clock. When the approx runs ahead of the
  database, it may introduce cblock mismatch.

- The new [local\lpeutien8`] index {[ visible_at <= NULL  ] leads a predicate. This helps
  the generic plan estimate hints primary lake and supplies an index with selectivity.
  It is still cheaper than building an exclusive custom plan every time.

- Phase 2 is not in the PR, so we cannot measure it.

//  Same payload as `fetch_work_item` -> locked-than-visible-at-later
- Both organize order and delivery are if id>©