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
- Visit https://github.com/affandar/duroxide/blob/main/CHANGELOG.md
- Identify all changes since the current duroxide version in `Cargo.toml`
- Pay special attention to:
  - **BREAKING** changes that affect the Provider trait
  - New provider trait methods or signature changes
  - New validation tests added to `provider_validation` module
  - Database schema requirements

### 1.2 Read the duroxide README
- Check https://github.com/affandar/duroxide/blob/main/README.md for any updated integration guidance

### 1.3 Review Provider Implementation Guide
- Read https://github.com/affandar/duroxide/blob/main/docs/provider-implementation-guide.md
- Note any new requirements for provider implementations
- Check for new trait methods that need to be implemented

### 1.4 Review Provider Testing Guide  
- Read https://github.com/affandar/duroxide/blob/main/docs/provider-testing-guide.md
- Identify new validation test modules that need to be added
- Check for any test configuration changes (e.g., new config fields)

## Step 2: Check duroxide GitHub Issues

- Visit https://github.com/affandar/duroxide/issues
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
**Every migration must have a companion diff file.** Git diffs only show new SQL code, not the semantic delta from previous versions. Generate the diff:

```bash
./scripts/generate_migration_diff.sh <migration_number>
# Example: ./scripts/generate_migration_diff.sh 9
# Creates: migrations/0009_diff.md
```

The script:
1. Creates temp schemas (before/after the target migration)
2. Applies all migrations up to N-1 and N respectively
3. Extracts DDL for tables, indexes, and functions
4. Generates unified diff in `migrations/NNNN_diff.md`
5. Cleans up temp schemas

> ⚠️ **Do not skip this step.** PRs with migrations but no diff file will be rejected.

See [migrations/0009_diff.md](../migrations/0009_diff.md) for a complete example.

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

## Step 6: Update Related Files

### 6.1 Update stress tests if needed
- Check `tests/stress_tests.rs` for any config struct changes
- Check `pg-stress/src/lib.rs` for the same

### 6.2 Update basic tests
- Check `tests/basic_tests.rs` for API signature changes

## Step 7: Test Thoroughly

### 7.1 Run all tests locally
```bash
# Ensure DATABASE_URL is set
export DATABASE_URL=postgres://user:pass@localhost:5432/duroxide_test

# Run tests
cargo test

# Run specific failing tests with output
cargo test <test_name> -- --nocapture
```

### 7.2 Run flaky test detection
For tests that were previously flaky, run multiple times:
```bash
for i in {1..10}; do
    echo "Run $i..."
    cargo test <test_name> --test postgres_provider_test
done
```

## Step 8: Update Documentation

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

## Step 9: Create Pull Request

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

## Step 10: Post-Merge

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
