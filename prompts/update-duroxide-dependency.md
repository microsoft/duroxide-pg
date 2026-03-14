# Updating duroxide-pg to Latest duroxide Version

This guide describes how to update duroxide-pg when a new version of the duroxide crate is released.

## Important Guidelines for LLM Assistants

> **DO NOT** push to any remote git repository or publish to crates.io unless explicitly asked by the user.
> 
> **DO** ask the user for confirmation before:
> - Pushing commits to remote branches
> - Creating pull requests  
> - Publishing to crates.io
> - Any other action that affects external systems
>
> When in doubt about how to proceed, **ask the user** for guidance.

## Prerequisites

Before starting the update:
1. Ensure you have a clean working directory (`git status`)
2. Create a new branch for the update: `git checkout -b update-duroxide-<version>`

## Step 1: Review duroxide Changes

### 1.1 Read the duroxide CHANGELOG
- Visit https://github.com/microsoft/duroxide/blob/main/CHANGELOG.md
- Identify all changes since the current duroxide version in `Cargo.toml`
- Pay special attention to:
  - **BREAKING** changes that affect the Provider trait
  - New provider trait methods or signature changes
  - New validation tests added to `provider_validation` module
  - Database schema requirements

### 1.2 Read the duroxide README
- Check https://github.com/microsoft/duroxide/blob/main/README.md for any updated integration guidance

### 1.3 Review Provider Implementation Guide
- Read https://github.com/microsoft/duroxide/blob/main/docs/provider-implementation-guide.md
- Note any new requirements for provider implementations
- Check for new trait methods that need to be implemented

### 1.4 Review Provider Testing Guide  
- Read https://github.com/microsoft/duroxide/blob/main/docs/provider-testing-guide.md
- Identify new validation test modules that need to be added
- Check for any test configuration changes (e.g., new config fields)

## Step 2: Check duroxide GitHub Issues

- Visit https://github.com/microsoft/duroxide/issues
- Filter by label `duroxide-pg` to find any provider-specific issues
- Review open and recently closed issues for test fixes or improvements

## Step 3: Update Dependencies

### 3.1 Update Cargo.toml
```toml
[dependencies]
duroxide = { version = "<new-version>", features = ["provider-test"] }
```

### 3.2 Run cargo check
```bash
cargo check
```

Fix any compilation errors from API changes.

## Step 4: Implement API Changes

For each breaking change identified in Step 1:

### 4.1 Update Provider Trait Implementation
- Edit `src/provider.rs` to implement new/changed trait methods
- Follow the patterns established by existing implementations
- Add appropriate logging for new operations

### 4.2 Update Database Migrations (if needed)
- Create new migration file: `migrations/NNNN_<description>.sql`
- Update stored procedures if provider method signatures changed
- Ensure migrations are idempotent (can be run multiple times safely)

### 4.3 Generate Migration Diff File (Required)
**Every migration must have a companion diff file.** Git diffs only show new SQL code, not the semantic delta from previous versions.

#### Option A: Auto-generate with PostgreSQL (preferred)
```bash
./scripts/generate_migration_diff.sh <migration_number>
# Example: ./scripts/generate_migration_diff.sh 9
# Creates: migrations/0009_diff.md
```

The script creates temp schemas, applies migrations before/after, extracts DDL, and diffs them.

#### Option B: Manual extraction (when no PostgreSQL available)
When a live database isn't available, generate diffs by extracting stored procedure bodies from the SQL migration files:

1. **Identify baselines**: For each SP modified in migration N, find the most recent migration before N that contains `CREATE OR REPLACE FUNCTION ... <sp_name>`. Use `grep -n "CREATE OR REPLACE FUNCTION.*<sp_name>" migrations/*.sql` to find them.
2. **Extract SP bodies**: Extract the `CREATE OR REPLACE FUNCTION ... LANGUAGE plpgsql;` block from both the baseline and new migration files.
3. **Normalize before diffing**: Migration files use different SQL quoting mechanisms across versions:
   - Replace schema placeholders (`%I.` or `@SCHEMA@.`) with `SCHEMA.`
   - Replace escaped single quotes (`''`) with `'` (old migrations inside `format('...')` need `''`; newer ones inside `format($fmt$...$fmt$)` don't)
   - Replace escaped percent signs (`%%`) with `%` (same reason: `format()` uses `%` for placeholders)
   - Expand tabs to spaces and strip trailing whitespace
4. **Diff**: Run `diff -u <baseline_normalized> <new_normalized>` to get unified diff output.
5. **Assemble**: Strip the `---`/`+++` header lines and include the hunks in ` ```diff ` code blocks.

#### Diff format requirements
Each changed function must be shown with `+`/`-` diff markers on changed lines inside a ` ```diff ` code block. The diff file should contain:
1. **Table Changes** — New tables (full DDL in ` ```sql ` block), modified tables (mark new columns with `+`)
2. **New Indexes** — Any indexes added by the migration (full DDL in ` ```sql ` block)
3. **Function Changes** — For each changed function: unified diff in a ` ```diff ` block with the baseline migration number noted in the heading (e.g., `### \`func_name\` — body modified (baseline: 0016)`)

> ⚠️ **Do not skip this step.** PRs with migrations but no diff file will be rejected.

See [migrations/0017_diff.md](../migrations/0017_diff.md) for a complete example.

### 4.4 Update src/migrations.rs
- Add new migration to the `MIGRATIONS` array if you created one

## Step 5: Add New Validation Tests

### 5.1 Identify New Tests
From the provider testing guide, identify new test modules such as:
- `provider_validation::new_module::test_name`

### 5.2 Update tests/postgres_provider_test.rs
Add new test modules following the existing pattern:
```rust
mod new_module_tests {
    use super::*;
    
    provider_validation_test!(new_module::test_name_1);
    provider_validation_test!(new_module::test_name_2);
}
```

## Step 6: Sync E2E Tests from Duroxide Main Repo

### 6.1 Sync e2e_samples tests
Check `tests/e2e_samples.rs` in the **duroxide** main repo ([github.com/microsoft/duroxide](https://github.com/microsoft/duroxide/tree/main/tests)) for any new or changed tests. Compare test function names and copy any missing tests, adapting them for PostgreSQL.

Use the GitHub MCP tools to fetch the test file contents from the duroxide repo, then compare with local tests:

```bash
# List tests in this repo
grep -o 'async fn [a-z_0-9]*' tests/e2e_samples.rs | sort
```

Compare against the list from the duroxide repo's `tests/e2e_samples.rs` (fetched via GitHub).

### 6.2 Sync session_e2e tests
Same process for `tests/session_e2e_tests.rs` — compare local test names against the duroxide repo's `tests/session_e2e_tests.rs`.

### 6.3 Adaptation pattern
When copying tests from the duroxide main repo to this provider repo:
- Replace `common::create_sqlite_store_disk()` → `common::create_postgres_store()`
- Replace `create_runtime(activities, orchestrations)` → `Runtime::start_with_store(store.clone(), activities, orchestrations)`
- Replace `create_runtime_with_options(activities, orchestrations, options)` → `Runtime::start_with_options(store.clone(), activities, orchestrations, options)`
- Add `common::cleanup_schema(&schema).await;` after `rt.shutdown(None).await;`
- Increase short timeouts (5s/10s) to 30s for PostgreSQL latency
- Add any new imports (`semver::Version`, `std::sync::atomic::*`, etc.)

> ⚠️ **Do not skip this step.** Provider e2e tests must stay in sync with the main repo to ensure feature parity.

## Step 7: Update Related Files

### 7.1 Update stress tests if needed
- Check `tests/stress_tests.rs` for any config struct changes
- Check `pg-stress/src/lib.rs` for the same

### 7.2 Update basic tests
- Check `tests/basic_tests.rs` for API signature changes

## Step 8: Test Thoroughly

### 8.1 Run all tests locally
```bash
# Ensure DATABASE_URL is set
export DATABASE_URL=postgres://user:pass@localhost:5432/duroxide_test

# Run tests
cargo test

# Run specific failing tests with output
cargo test <test_name> -- --nocapture
```

### 8.2 Run flaky test detection
For tests that were previously flaky, run multiple times:
```bash
for i in {1..10}; do
    echo "Run $i..."
    cargo test <test_name> --test postgres_provider_test
done
```

## Step 9: Update Documentation

### 8.1 Update CHANGELOG.md
Add a new version entry with:
- Version number and date
- Breaking changes section
- Added features section
- Any migration notes

### 8.2 Update README.md
- Update the "Latest Release" section
- Update duroxide version compatibility if mentioned

### 8.3 Update Cargo.toml version
Bump duroxide-pg version appropriately:
- MAJOR: Breaking API changes
- MINOR: New features, backward compatible
- PATCH: Bug fixes only

## Step 10: Create Pull Request

> **STOP**: Ask the user before proceeding with any git push or PR creation.

### 9.1 Commit changes (local only)
```bash
git add .
git commit -m "Update to duroxide <version> with <main feature>"
```

### 9.2 Ask user before pushing
Before pushing or creating a PR, confirm with the user:
- "Ready to push to remote and create a PR?"
- "Would you like to review the changes first?"

### 9.3 Push and create PR (only when user confirms)
```bash
git push -u origin update-duroxide-<version>
gh pr create --title "Update to duroxide <version>" --body "<PR description>"
```

### 9.4 Verify CI passes
- Check GitHub Actions workflow runs
- Address any CI failures

## Step 11: Post-Merge

> **STOP**: Only proceed with publishing when explicitly requested by the user.

### 10.1 Publish to crates.io (user must explicitly request)
Follow `prompts/publish-crate.md` for publishing instructions.
**Do not publish without user confirmation.**

### 10.2 Create GitHub release (user must explicitly request)
Tag the release and create release notes.
**Do not create releases without user confirmation.**

---

## Common Issues

### Compilation Errors
- Check if trait method signatures changed
- Look for new required trait methods
- Verify import statements for new types

### Test Failures
- Verify database schema matches expected structure
- Check if new tables/columns are needed
- Ensure stored procedures return expected formats

### Migration Issues
- Stored procedures may need DROP before CREATE OR REPLACE
- Check for function signature changes requiring explicit DROP
- Verify schema references use dynamic schema names
- **Always create `NNNN_diff.md`** when dropping/recreating functions so reviewers can see the actual delta

## Example: Adding ExecutionState Support

When duroxide adds a new type like `ExecutionState`:

1. **Import the type**: `use duroxide::providers::ExecutionState;`

2. **Update method signatures**: 
   ```rust
   // Before
   async fn fetch_work_item(&self, ...) -> Result<Option<(WorkItem, String, u32)>, ProviderError>
   
   // After  
   async fn fetch_work_item(&self, ...) -> Result<Option<(WorkItem, String, u32, ExecutionState)>, ProviderError>
   ```

3. **Update stored procedure** to return the new data

4. **Parse the new return value** in Rust code

5. **Add validation tests** for the new functionality
