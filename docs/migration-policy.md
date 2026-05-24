# Migration Policy

`duroxide-pg` supports separating schema migration from normal runtime database
operations. This matters in production because DDL usually belongs to a
controlled deploy or upgrade step, while application backends and workers often
run with lower-privilege DML-only roles.

## Policies

`MigrationPolicy::ApplyAll` is the default. It preserves the original provider
behavior: provider construction creates the target schema when needed, creates
the `_duroxide_migrations` tracking table, rejects schemas that contain unknown
future migration versions, and applies any bundled migrations that have not yet
been recorded.

`MigrationPolicy::VerifyOnly` executes no DDL. It verifies that the migration
tracking table exists, that the schema has no unknown future migration versions,
that every bundled migration has already been applied, and that core provider
tables still exist. This is intended for client/backend/worker processes where a
separate privileged process has already run migrations.

## Behavior Matrix

| Scenario | `ApplyAll` | `VerifyOnly` |
|---|---|---|
| Schema does not exist | Creates it | Error |
| Schema exists but tracking table is missing | Creates tracking table and runs migrations | Error |
| Schema is behind bundled migrations | Applies pending migrations | Error |
| Schema has unknown future migrations | Error before DDL | Error |
| Migrations are recorded but core tables are missing | Re-applies migrations | Error |
| Schema is fully migrated | No-op | No-op |

`ApplyAll` takes a PostgreSQL session advisory lock scoped to the target schema
before checking or applying migrations. That serializes concurrent provider
startup so multiple nodes do not race to apply the same migration.

## Error Messages

`VerifyOnly` fails with messages in these shapes:

- Missing tracking table: `duroxide migrations not initialized: schema "..." does not contain _duroxide_migrations...`
- Missing bundled migration versions: `duroxide migrations not up to date in schema "...": missing versions [...]...`
- Unknown future migration versions: `schema "..." has migrations not recognized by this version of the code: [...]...`
- Missing core tables after complete migration records: `duroxide migrations recorded as complete in schema "...", but core tables are missing...`

`ApplyAll` also rejects unknown future migration versions before it runs DDL, so
an older binary will not mutate a schema that appears ahead of its code.

## Example

```rust
use duroxide_pg::{MigrationPolicy, PostgresProvider, ProviderConfig};

# async fn example(database_url: &str) -> anyhow::Result<()> {
let mut config = ProviderConfig::url(database_url);
config.schema_name = Some("duroxide".to_string());
config.migration_policy = MigrationPolicy::VerifyOnly;

let provider = PostgresProvider::new_with_config(config).await?;
# Ok(())
# }
```