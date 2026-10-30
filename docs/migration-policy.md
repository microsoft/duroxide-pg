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
Each migration transaction uses a 250 ms `lock_timeout`. Deadlocks (`40P01`) and
lock timeouts (`55P03`) roll back and retry the entire transaction with jitter,
within a ten-second contention budget per migration. Other errors return
immediately. A failed attempt does not advance the migration ledger. The
migration connection is closed on exit or cancellation so a failed unlock
cannot leave a session advisory lock in the pool.

The contention budget is currently fixed, not a provider configuration option.
If it runs out, inspect the logged schema, SQLSTATE, and error chain, stop or
drain competing workers and admin writers, and allow transactions holding the
blocking locks to finish. Then retry `ApplyAll` from the process with DDL
privileges. Earlier successful migrations remain recorded; the failed
transaction is rolled back and will be retried on startup. Do not delete or
edit migration ledger rows to force a retry. Backfill migrations must use their
drained-upgrade procedure: a transaction retry repeats that backfill's work.

## Long-Poll Notification Activation

Migration `0030` installs disabled notification triggers. With
`long_poll.enabled = true`, `ApplyAll` also enables the queue and session triggers in a
transaction while holding the migration lock. `VerifyOnly` requires that the
triggers are already active; otherwise construction returns an error explaining
that an `ApplyAll` provider must activate them first. It never performs DDL.
Activation follows acknowledgments' worker-before-orchestration lock order.
Every lock acquisition, including the first queue, uses the same 250 ms
`lock_timeout` and ten-second retry budget as migration transactions. Each
failed attempt rolls back before the jittered delay. This bounds interference
with writers and allows activation to outlast short bursts of reverse-order
admin-delete contention. Exhausting the budget returns the database error
without partially activating triggers.

Activation is schema-wide and persists after the initializing provider exits.
It allows short-polling clients to notify long-polling workers. Constructing a
provider with long-polling disabled does not deactivate these shared triggers.
Schemas that have not opted in do not emit notifications.

PostgreSQL cannot prepare a transaction that emits a notification, so
`PREPARE TRANSACTION` is not supported for queue or session-lease writes after activation.
Reverting to two-phase-commit use requires stopping all long-polling workers
and explicitly disabling all six notification triggers:

- `worker_queue`: `trg_notify_worker_work`, `trg_notify_worker_work_released`
- `orchestrator_queue`: `trg_notify_orch_work`, `trg_notify_orch_work_released`,
  `trg_notify_orch_work_consumed`
- `sessions`: `trg_notify_session_lease`

These are verified and enabled from a single trigger inventory;
changing a process's configuration alone is not sufficient.

Notification-emitting commits also participate in PostgreSQL's server-wide
notification-writer serialization. WAL or synchronous-replication delays can
block other notification senders, including in different databases. Activation
adds this cost to every successful queue claim, lease-changing orchestration
and activity renewal, and session-lease write, not just enqueue and acknowledgment.
A normal activity therefore has three notifying commits and an orchestration
turn two; renewal transactions also notify. Empty fetches do not.
Evaluate the loaded-write and renewal costs before opting a schema in;
activation affects short-polling callers too. Disabling a process's listener
does not remove the commit-lock cost of the schema's triggers. See the
[synchronous-standby benchmark](../README.md#reproducing-long-poll-performance)
for a higher-concurrency measurement that records notification-lock wait time.

This is an additive migration within the `duroxide-pg` schema lineage, not a
conversion of `duroxide-pg-opt` state. Older binaries still reject schemas with
unknown migration versions, even when notifications remain disabled.

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