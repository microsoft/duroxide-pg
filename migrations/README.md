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

### Continued execution timestamps (0027)

Migration `0027` fixes completion timestamps for `ContinuedAsNew` executions.
It preserves the acknowledgement lease safeguards introduced in migration `0026`.
It also fills existing NULL timestamps from the persisted `created_at` of each
execution's last `OrchestrationContinuedAsNew` history event. This uses the same
provider clock as acknowledgement, not the execution's start time or upgrade
time. Existing non-NULL timestamps and other execution statuses are unchanged.

If no terminal event is available, the timestamp stays NULL and the migration
warns with the affected row count. These rows remain ineligible for age-based
pruning; inspect their history before choosing an explicit count-based policy.
The migration itself deletes no executions or history. Once timestamps are
recovered, subsequent age-based pruning can remove generations older than the
configured cutoff, while still protecting the current and running executions.

Stop worker runtimes before upgrading and resume them after migrations complete,
so acknowledgements using the old function cannot race the one-time backfill.
The backfill can take time on a large history store. The migration is idempotent;
reapplying it does not change already populated timestamps.

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

Batches hold instance-row locks until their transaction ends and recheck
execution eligibility after any lock wait. This serializes pruning with instance
deletion/recreation and acknowledgement, protecting replacement instances and
executions that become ineligible while the batch waits.

Use a maintenance connection with appropriate statement/lock timeouts. A failed
call rolls back its entire batch. Applications that must progress past locked
pages may pre-read the bounded page's last key and retry that page on a later
scan; a successful call's cursor remains authoritative if concurrent work changes
the page. Keep the original `prune_executions` API for callers that intentionally
request a whole-instance historical cleanup.

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
- If you need to re-run a migration, manually delete its entry from the tracking table (use with caution!)

### Schema conflicts
- Each schema maintains its own migration history
- Migrations can be applied independently to different schemas
