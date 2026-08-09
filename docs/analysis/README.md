# Waldemort analysis documents

This folder holds performance analysis and measurement reports produced by
agent-driven investigation of this repository.

These documents are for sharing findings. They are not product documentation
and they do not describe intended behavior. Each one reports what was measured,
how, and where the measurement's limits are.

All documents use simple technical English.

## Contents

| Document | Subject |
|---|---|
| `pr18-dequeue-measurement.md` | Does the partial index and two-phase dequeue in PR \#18 improve performance? |
| `fetch-work-item-generic-plan.md` | `fetch_work_item` uses a generic plan and scans the whole future-visible backlog |
| `custom-plan-cost.md` | What it costs to force a custom plan, and a cheaper alternative |

## Reading order

Start with `pr18-dequeue-measurement.md`. It reviews the proposed change and
concludes that the gain does not appear at realistic scale.

The other two documents came out of that work. While measuring PR \#18, a
separate and larger problem appeared in the same function.
`fetch-work-item-generic-plan.md` describes it, and `custom-plan-cost.md`
measures the cost of fixing it.

## Conventions

- **Locked rows** are rows a worker currently holds, where `lock_token` is not
  NULL.
- **Future-visible rows** are rows whose `visible_at` is still in the future.
- Every measurement names the PostgreSQL version, the settings changed, and
  the source commit it was taken against.
- Every document ends with a section stating the limits of its test.

## A note on where these ran

The measurements were taken on PostgreSQL in a container with local disk, not
on Azure Database for PostgreSQL Flexible Server storage. Absolute numbers
will differ on production storage.

Comparisons between variants are still sound: each pair ran on the same
server, with the same data, the same warm cache, and in paired order.
