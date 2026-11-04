# Migration System

This project uses a custom migration system for PostgreSQL schema management. Migrations are stored as SQL files in the `migrations/` directory and are automatically applied when the provider is initialized.

## Migration Files

Migrations are numbered sequentially and follow the naming pattern:
```
0001_initial_schema.sql
0002_add_feature.sql
0003_update_indexes.sql
...
```

The version number is extracted from the filename prefix (before the first underscore).

## How It Works

1. **Schema Creation**: When creating a `PostgresProvider`, if a custom schema is specified (not "public"), the schema is created automatically.

2. **Migration Execution**: The migration runner:
   - Loads all `.sql` files from the `migrations/` directory
   - Sorts them by version number
   - Checks which migrations have already been applied (tracked in `_duroxide_migrations` table)
   - Applies pending migrations in order
   - Records each applied migration

3. **Schema Isolation**: Each schema has its own migration tracking table (`{schema}._duroxide_migrations`), allowing independent migration history per schema.

## Migration Tracking

Migrations are tracked in a table called `_duroxide_migrations` located in each schema:

```sql
CREATE TABLE {schema}._duroxide_migrations (
    version BIGINT PRIMARY KEY,
    name TEXT NOT NULL,
    applied_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);
```

## Writing Migrations

When writing migrations:

1. **Use schema-relative names**: Migrations are executed with
   `SET LOCAL search_path TO <schema>, pg_temp`, so use unqualified table names:
   ```sql
   CREATE TABLE instances (...);
   ```
   Not:
   ```sql
   CREATE TABLE public.instances (...);
   ```

2. **Multiple statements**: Migrations can contain multiple SQL statements separated by semicolons:
   ```sql
   CREATE TABLE table1 (...);
   CREATE TABLE table2 (...);
   CREATE INDEX idx1 ON table1(...);
   ```

3. **Idempotent operations**: Use `IF NOT EXISTS` clauses to make migrations idempotent:
   ```sql
   CREATE TABLE IF NOT EXISTS instances (...);
   CREATE INDEX IF NOT EXISTS idx_instances ON instances(...);
   ```

4. **Backward compatibility**: Use `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` for adding columns to existing tables.

## Example Migration

```sql
-- Migration: 0002_add_timestamps.sql
-- Description: Add timestamp columns to worker_queue

ALTER TABLE worker_queue ADD COLUMN IF NOT EXISTS processed_at TIMESTAMPTZ;
CREATE INDEX IF NOT EXISTS idx_worker_processed ON worker_queue(processed_at);
```

## Adding a New Migration

1. Create a new file in `migrations/` with the next sequential number
2. Use a descriptive name: `0002_add_feature.sql`
3. Write idempotent SQL statements
4. Test the migration locally
5. Commit the migration file

## Testing Migrations

Migrations are automatically applied when creating a `PostgresProvider`. Each test schema gets its own migration history, so you can test migrations in isolation.

The `embedded_migration_versions_are_unique_and_increasing` unit test checks
that bundled SQL files have distinct, increasing version numbers. Gaps are
allowed; two migrations must never share a version.

### Continued execution timestamps (0027)

Migration `0027` fixes completion timestamps for `ContinuedAsNew` executions.
It preserves the acknowledgement lease safeguards introduced in migration `0026`.
It also fills existing NULL timestamps from the persisted `created_at` of each
execution's last `OrchestrationContinuedAsNew` history event. This uses the same
provider clock as acknowledgement, not the execution's start time or upgrade
time. Existing non-NULL timestamps and other execution statuses are unchanged.

If a history payload cannot be decoded as JSONB, the migration aborts and reports
its schema, instance, execution, and event key. No timestamp changes or function
replacements from migration `0027` are committed on failure.

If no terminal event is available, the timestamp stays NULL and the migration
warns with the affected row count. These rows remain ineligible for age-based
pruning; inspect their history before choosing an explicit count-based policy.
The migration itself deletes no executions or history. Once timestamps are
recovered, subsequent age-based pruning can remove generations older than the
configured cutoff, while still protecting the current and running executions.

#### Upgrading safely

Migrations `0027` and `0028` require a fully drained upgrade, not a rolling
upgrade with old and new provider binaries running together:

1. Pause all delete and prune jobs.
2. Stop and drain **every process that uses the provider**: workers, clients,
   retention and maintenance jobs, and direct SQL callers. Wait for in-flight
   calls and transactions to finish.
3. Start one process using the new binary with `MigrationPolicy::ApplyAll` first.
   It applies the migrations during provider initialization; keep provider
   traffic stopped until initialization succeeds.
4. Restart the remaining processes using only new binaries containing these fixes
   and updated direct SQL callers.
5. Review retention cutoffs before resuming delete and prune jobs. Expect a
   one-time wave of age-based deletions of old continue-as-new generations whose
   completion timestamps have now been recovered.

Stopping only workers is insufficient. An old client's age-based deletion
selector can select a pending continuation after its completion timestamp has
been filled, deleting the instance and its queued continuation.

Backfill time and WAL volume grow with the number of continue-as-new rows to
repair. Large histories can further increase backfill time. Allow for this work
in the maintenance window and WAL storage budget.

#### Repairing missing continued-execution timestamps

If an upgrade was not fully drained, an acknowledgement already executing the
old function can write a NULL `completed_at` after the backfill. These rows remain
ineligible for age-based pruning. **Draining all callers is the fence**:
re-running the backfill while old calls remain in flight does not close the race.

First pause deletion and pruning, stop and drain all callers as above, and ensure
the migrations have completed. With provider traffic still stopped, replace
`your_provider_schema` below and run this transaction. It fills only missing
`ContinuedAsNew` timestamps using the last terminal history event's original
`created_at`; existing timestamps and other statuses are unchanged. It neither
replaces functions nor changes the migration ledger. Do not delete migration
tracking rows or re-run whole migrations to perform this repair.

```sql
BEGIN;
SET LOCAL search_path TO your_provider_schema, pg_temp;

WITH completion_times AS (
    SELECT e.instance_id, e.execution_id, terminal.created_at
    FROM executions e
    CROSS JOIN LATERAL (
        SELECT h.created_at
        FROM history h
        WHERE h.instance_id = e.instance_id
          AND h.execution_id = e.execution_id
          AND h.event_data::jsonb->>'type' = 'OrchestrationContinuedAsNew'
        ORDER BY h.event_id DESC
        LIMIT 1
    ) terminal
    WHERE e.status = 'ContinuedAsNew' AND e.completed_at IS NULL
)
UPDATE executions e
SET completed_at = c.created_at
FROM completion_times c
WHERE e.instance_id = c.instance_id AND e.execution_id = c.execution_id
  AND e.status = 'ContinuedAsNew' AND e.completed_at IS NULL;

SELECT instance_id, execution_id
FROM executions
WHERE status = 'ContinuedAsNew' AND completed_at IS NULL
ORDER BY instance_id, execution_id;
COMMIT;
```

Rows returned by the final query have no recoverable terminal timestamp; inspect
their history rather than substituting the current time. Invalid history JSONB
aborts the transaction instead of being skipped. After repair, restart only new
binaries and updated SQL callers, then resume retention with reviewed cutoffs.

### Batched execution pruning (0028)

`<schema>.prune_executions_batch(after_instance_id, after_execution_id,
completed_before_ms, limit)` atomically prunes one primary-key-ordered page across
the provider store. Start with `('', 0)`, a nonnegative epoch-millisecond cutoff,
and a limit from 1 through 1,000. It returns `last_instance_id`,
`last_execution_id`, `executions_scanned`, `executions_deleted`, and `events_deleted`.
Use the returned key to resume; both cursor columns are NULL only for an empty
page. A page shorter than the requested limit ends that scan.

The limit applies to **scanned execution keys**, not just eligible matches.
Current/future executions, Running executions, and histories with NULL or recent
completion times are preserved, but still advance the cursor. Missing instance
metadata is not permission to delete history. No instance, queue, lock, or KV
records are removed. The number of history events per execution is not capped.

Batched pruning checks eligibility without locking first, so an instance with no
deletion candidates is not locked. Protected rows still count toward the page.
Both pruning APIs hold acquired instance-row locks until their transactions end.
They check execution eligibility after acquiring the lock, serializing with instance
deletion/recreation and acknowledgement. This protects replacement instances and
executions that become ineligible while a call waits.
Pruning and atomic deletion acquire their instance locks in ascending instance-ID
order to avoid lock-order inversions during multi-instance operations.

Use a maintenance connection with appropriate statement/lock timeouts. A failed
call rolls back its entire batch. Applications that must progress past locked
pages may pre-read the bounded page's last key and retry that page on a later
scan; a successful call's cursor remains authoritative if concurrent work changes
the page. Keep the original `prune_executions` API for callers that intentionally
request a whole-instance historical cleanup.

Public-schema cleanup also removes the batch-pruning function, while leaving the
`public` schema itself intact.

## Worker Queue Progress (Migration 0029)

Migration `0029` orders eligible worker items by visibility time, then queue ID.
Deferred work moves behind items that were already ready, even when its delay
expires before the backlog drains. Session ownership, tag filtering, lock
handling, and attempt accounting are unchanged.

The migration adds `idx_worker_ready_order` on `worker_queue(visible_at, id)`.
`CREATE INDEX IF NOT EXISTS` takes a `SHARE` lock on `worker_queue`, even when
the index already exists, and holds it **until the migration transaction
commits**, not just until index creation finishes. This blocks worker fetch,
acknowledge, abandon, and lock renewal, as well as orchestration acknowledgements
that write worker rows. The migration runner uses bounded lock waits and a
ten-second retry budget; the SQL file itself sets no `lock_timeout` when run
directly. Plan a maintenance window and allow time for the index build.

For a busy or large queue, pre-build the index **before starting the migration**,
outside any transaction, in each target schema. Replace `duroxide` below with
the provider's schema:

```sql
CREATE INDEX CONCURRENTLY idx_worker_ready_order
    ON duroxide.worker_queue (visible_at, id);

SELECT i.indisvalid, pg_get_indexdef(i.indexrelid)
FROM pg_catalog.pg_index i
WHERE i.indexrelid = 'duroxide.idx_worker_ready_order'::regclass;
```

Wait for the build to finish and verify that `indisvalid` is true and the
definition matches the index above. The migration's `CREATE INDEX IF NOT
EXISTS` will then skip the expensive build, but still take the `SHARE` lock
described above. A failed or cancelled concurrent build can leave an invalid
index with the same name; `IF NOT EXISTS` does not repair it, and the migration
rejects it without recording success. Inspect such an index, then drop it with
`DROP INDEX CONCURRENTLY duroxide.idx_worker_ready_order` outside a transaction
before retrying the concurrent build.

The `fetch_work_item` signature is unchanged, so already-running older
providers can keep using it. Older binaries cannot restart against the upgraded
schema because they reject unknown migration versions. Coordinate migration
and provider rollout accordingly.

## Long-Poll Notifications (Migration 0030)

Migration `0030` installs disabled queue-notification triggers. A provider with
`MigrationPolicy::ApplyAll` and `long_poll.enabled = true` activates them for the
schema under the migration lock. `VerifyOnly` requires prior activation. See
[Migration Policy](../docs/migration-policy.md) for compatibility and privilege
requirements.
The six triggers cover both queues and session leases. Claims and renewals send
future lease-expiry hints; releasing an orchestration claim also sends an
immediate hint, even when the abandoned message is delayed. Installation and
activation use bounded transaction retries under lock contention.

The long-poll migration is reserved as `0030`, after `0027`-`0028` from
[#32](https://github.com/microsoft/duroxide-pg/pull/32) and `0029` from
[#51](https://github.com/microsoft/duroxide-pg/pull/51). Its `cleanup_schema`
definition retains migration `0028`'s batched-pruning cleanup.
**Do not release #41 before #32 and #51.** A release containing `0030` must
also contain the preceding `0027`-`0029` migrations; #51 likewise must not be
released before #32. Shipping a later migration first creates a ledger that
the later combined build rejects as a non-prefix. Reserving a migration number
does not make the intervening migrations optional. Do not repair this by editing
the ledger; coordinate the release order before publishing.
Disposable development/test databases that applied an earlier draft of the
long-poll migration as `0027` must be recreated before using this sequence.
Do not delete migration records or apply this draft over such a database that
contains data you need to preserve.

The provider validation harness can exercise either mode:

```sh
cargo nextest run --test postgres_provider_test
DUROXIDE_PG_TEST_LONG_POLL=true cargo nextest run --test postgres_provider_test
cargo nextest run --test long_poll_tests
```

The ignored two-phase-commit test additionally requires a disposable PostgreSQL
server with `max_prepared_transactions >= 1`:

```sh
cargo nextest run --test long_poll_tests --run-ignored only \
  -E 'test(two_phase_commit_is_preserved_until_explicit_notification_activation)'
```

The ignored idle-query benchmark requires `pg_stat_statements` to be preloaded
and installed in a disposable local database, and an optimized PostgreSQL build
without assertions or memory wiping. It measures 65 seconds per mode, including
the normal long-poll timeouts and notifier refresh:

```sh
cargo nextest run --release --test long_poll_tests --run-ignored only \
  -E 'test(idle_query_load_is_reduced_with_long_polling)' --success-output final
```

## Migration Execution Context

- Migrations run inside transactions
- Each migration is executed atomically (all-or-nothing)
- The `search_path` is set to `<target schema>, pg_temp` for each migration
  (`pg_temp` is pinned last so temporary objects cannot shadow the unqualified
  references migrations rely on the `search_path` to resolve)
- Migrations run in the order specified by their version numbers

## Troubleshooting

### Migration not found
- Ensure the migrations directory exists relative to where the code is executed
- The migration runner tries multiple paths: `./migrations`, `../migrations`, and `$CARGO_MANIFEST_DIR/migrations`

### Migration already applied
- Check the `_duroxide_migrations` table in your schema
- Keep applied migration records intact. Use a documented data repair, such as
  the continued-execution timestamp repair above, or a new migration rather than
  deleting ledger rows to force old migrations to run again.

### Schema conflicts
- Each schema maintains its own migration history
- Migrations can be applied independently to different schemas
