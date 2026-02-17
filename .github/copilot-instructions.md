# Duroxide-PG Copilot Instructions

## Project Overview
This is a **PostgreSQL provider** for [Duroxide](https://github.com/affandar/duroxide), a durable task orchestration framework for Rust. It implements the `Provider` and `ProviderAdmin` traits, storing orchestration state, history, and work queues in PostgreSQL using stored procedures.

## Architecture

### Core Components
- **[src/provider.rs](../src/provider.rs)** - Main `PostgresProvider` implementing duroxide traits (`Provider`, `ProviderAdmin`)
- **[src/migrations.rs](../src/migrations.rs)** - Embedded SQL migration runner using `include_dir!`
- **[migrations/](../migrations/)** - Sequential SQL migrations (schema + stored procedures)
- **[pg-stress/](../pg-stress/)** - Stress testing binary for performance validation

### Key Design Decisions
1. **Stored Procedures over inline SQL** - All database operations use schema-qualified stored procedures in `0002_create_stored_procedures.sql` for atomic transactions and deadlock prevention
2. **Rust-provided timestamps** - All procedures receive `p_now_ms` from Rust (not database `NOW()`) to avoid clock skew (see migration `0006`)
3. **Schema isolation** - Multi-tenant support via custom PostgreSQL schemas (`new_with_schema()`)
4. **Two-phase locking in fetch operations** - Prevents B-tree index deadlocks (peek query → advisory lock → FOR UPDATE verification)

### Data Flow
```
Duroxide Runtime → PostgresProvider → Stored Procedures → PostgreSQL Tables
                                                         (instances, executions, history,
                                                          orchestrator_queue, worker_queue)
```

## Development Workflow

### Prerequisites
```bash
# PostgreSQL running locally (or use Docker)
docker run -d --name duroxide-pg -p 5432:5432 -e POSTGRES_PASSWORD=postgres postgres:15

# Create .env file
echo "DATABASE_URL=postgres://postgres:postgres@localhost:5432/duroxide_test" > .env
```

### Running Tests

**Prefer `cargo-nextest`** if installed (faster, better output). Fall back to `cargo test` otherwise.

```bash
# Check if nextest is available
command -v cargo-nextest >/dev/null 2>&1 && echo "nextest available" || echo "use cargo test"

# All tests with nextest (preferred)
cargo nextest run

# All tests with cargo test (fallback)
cargo test

# Specific test with output
cargo nextest run test_provider_creation --nocapture
# or: cargo test test_provider_creation -- --nocapture

# Provider validation tests only (99 tests from duroxide)
cargo nextest run --test postgres_provider_test
# or: cargo test --test postgres_provider_test

# Stress tests (long-running, marked #[ignore])
cargo nextest run --test stress_tests --run-ignored only
# or: cargo test --test stress_tests -- --ignored
```

### Connection Exhaustion Under High Parallelism

Each test runtime creates a connection pool (default 10 max connections). At high parallelism (e.g., 14 cores), peak PostgreSQL connections can reach **~104**. If PostgreSQL `max_connections` is set to the default 100, tests will fail with timeouts due to connection exhaustion.

**Fix:** Increase PostgreSQL `max_connections` to at least 300:
```bash
docker exec <container> psql -U postgres -c "ALTER SYSTEM SET max_connections = 500;"
docker restart <container>
```

### Stress Testing
```bash
# Quick stress test (10 seconds)
cargo run --release --package duroxide-pg-stress --bin pg-stress -- --duration 10

# Custom configuration
cargo run --release --package duroxide-pg-stress --bin pg-stress -- \
  --duration 30 --orch-concurrency 4 --worker-concurrency 4
```

## Conventions

### Error Handling
Provider errors are classified as **retryable** or **permanent** via `ProviderError`:
```rust
// Retryable: deadlocks (40P01), pool timeouts, I/O errors
ProviderError::retryable(operation, message)

// Permanent: constraint violations (23505, 23503), invalid tokens
ProviderError::permanent(operation, message)
```

### Schema Naming for Tests
Tests create unique schemas with GUID suffix to allow parallel execution:
```rust
fn next_schema_name() -> String {
    let guid = uuid::Uuid::new_v4().to_string();
    format!("test_{}", &guid[guid.len()-8..])
}
```

### Adding Migrations
1. Create `NNNN_description.sql` in `migrations/`
2. Use unqualified table names (search_path is set by runner)
3. Make idempotent with `IF NOT EXISTS` / `IF EXISTS`
4. Update stored procedures with new parameters if needed
5. **REQUIRED: Generate a diff markdown file** (see below)

### Migration Diff Files (Required)
Every migration that modifies schema or stored procedures **must** have a companion `NNNN_diff.md` file. This is required because git diffs for SQL migrations only show the new code, not the delta from the previous version.

```bash
# Auto-generate diff by comparing schema before/after migration
./scripts/generate_migration_diff.sh <migration_number>

# Example:
./scripts/generate_migration_diff.sh 9
# Creates: migrations/0009_diff.md
```

The script:
1. Creates two temp schemas (before/after the target migration)
2. Extracts DDL for tables, indexes, and functions
3. Diffs them and generates `migrations/NNNN_diff.md`
4. Cleans up temp schemas

**Diff format requirements:** Each changed function must be shown **in full** with `+`/`-` diff markers on changed lines. This ensures the reader always knows which function a change belongs to (the `CREATE OR REPLACE FUNCTION` line is always visible at the top of each block). Do NOT use standard unified diff with small context windows — those lose function boundaries in large stored procedures.

The diff file should contain:
1. **Table Changes** — New tables (full column list), modified tables (mark new columns with `+`)
2. **New Indexes** — Any indexes added by the migration
3. **Function Changes** — For each changed function: full function body in a `diff` code block with `+`/`-` markers. New functions shown in full in a `sql` code block. Signature changes called out separately.

Example output: See [migrations/0014_diff.md](../migrations/0014_diff.md)

### Updating duroxide Dependency
Follow the detailed guide in [prompts/update-duroxide-dependency.md](../prompts/update-duroxide-dependency.md). Key steps:
1. **Review changes**: Read duroxide CHANGELOG, README, and provider guides at the duroxide repo
2. **Update Cargo.toml**: Change version, run `cargo check`, fix compilation errors
3. **Implement API changes**: Update `src/provider.rs`, add migrations if needed
4. **Add validation tests**: New tests go in `tests/postgres_provider_test.rs` using the `provider_validation_test!` macro
5. **Test thoroughly**: `cargo test`, run flaky tests 10x
6. **Update docs**: CHANGELOG.md, README.md, bump version

> ⚠️ **Never push to remote or publish to crates.io without explicit user confirmation**

## Key Files Reference
| File | Purpose |
|------|---------|
| [provider.rs](../src/provider.rs) | Provider trait implementations |
| [0002_create_stored_procedures.sql](../migrations/0002_create_stored_procedures.sql) | All stored procedures |
| [tests/common/mod.rs](../tests/common/mod.rs) | Test utilities (`create_postgres_store`, `test_create_execution`) |
| [postgres_provider_test.rs](../tests/postgres_provider_test.rs) | Provider validation test harness |

## Debugging Tips
- Enable tracing: `RUST_LOG=duroxide::providers::postgres=debug cargo test`
- Check schema cleanup: Run `scripts/cleanup_test_schemas.sh` after failed tests
- Deadlock issues: Review two-phase locking in `fetch_orchestration_item` stored procedure
