# Duroxide Upstream Update Sync

**Purpose:** Prompt for syncing duroxide-pg with upstream duroxide changes.

---

## When to Use

Use this prompt when:
- A new version of duroxide has been released
- You need to check for breaking changes or new features affecting the provider
- You want to ensure compatibility with the latest duroxide version

---

## Instructions for AI Agent

Perform the following steps in order:

### Step 1: Check Current Version

```bash
grep -A2 'duroxide' Cargo.toml
```

Note the current version specification.

### Step 2: Check for New Releases

```bash
gh release list --repo microsoft/duroxide --limit 5
```

### Step 3: Update Dependency

```bash
cargo update duroxide && cargo fetch
```

### Step 4: Locate New Source

```bash
find ~/.cargo/registry/src -type d -name "duroxide-*" | sort -V | tail -1
```

### Step 5: Read CHANGELOG.md

Read the CHANGELOG.md from the duroxide source to understand what changed:

```bash
cat $(find ~/.cargo/registry/src -type d -name "duroxide-*" | sort -V | tail -1)/CHANGELOG.md
```

**Key things to look for:**
- Breaking changes to `Provider` or `ProviderAdmin` traits
- New trait methods that need implementation
- Changes to `WorkItem`, `Event`, or `ProviderError` types
- New validation tests in `provider_validations`
- Changes to stress test infrastructure

### Step 6: Check for Blocker Fixes

Check if any issues in `prompts/duroxide-blockers.md` have been fixed:

```bash
# Check each issue status
gh issue view 51 --repo microsoft/duroxide
```

### Step 7: Run Tests

```bash
cargo nextest run
```

### Step 8: Search for STOPGAP Markers

```bash
grep -rn "STOPGAP\|BLOCKED on duroxide" --include="*.rs" .
```

For each marker, check if the related issue is now fixed and if cleanup can be performed.

### Step 9: Update Documentation

If any blockers are resolved:
1. Update `prompts/duroxide-blockers.md` - move resolved items
2. Update `TODO.md` - remove BLOCKED entries
3. Update `CHANGELOG.md` - note the cleanup

---

## Provider Trait Changes Checklist

When duroxide updates the Provider trait:

1. **New methods added:**
   - [ ] Implement in `src/provider.rs`
   - [ ] Add corresponding stored procedure in `migrations/` if needed
   - [ ] Generate migration diff: `./scripts/generate_migration_diff.sh <N>`

2. **Method signature changes:**
   - [ ] Update implementation in `src/provider.rs`
   - [ ] Update stored procedure if return type changed
   - [ ] Check all usages in tests

3. **New validation tests:**
   - [ ] Add to `tests/postgres_provider_test.rs`
   - [ ] Use `provider_validation_test!` macro if possible

---

## Quick Reference

```bash
# Check duroxide version in Cargo.lock
grep -A1 'name = "duroxide"' Cargo.lock | head -2

# Find all duroxide trait implementations
grep -n "impl.*Provider.*for PostgresProvider" src/provider.rs

# Count validation tests
grep -c "provider_validation_test!" tests/postgres_provider_test.rs

# Run specific test module
cargo nextest run atomicity_tests
```
