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
that write worker rows. The migration sets no `lock_timeout`, so acquiring the
lock can also wait indefinitely. Plan a maintenance window or set an appropriate
deployment-session `lock_timeout` through PostgreSQL connection options.

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

