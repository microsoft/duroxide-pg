# Duroxide Upstream Update Sync

**Purpose:** Prompt for syncing duroxide-pg-opt with upstream duroxide changes.

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

### Step 2: Update Dependency

```bash
cargo update duroxide && cargo fetch
```

### Step 3: Locate New Source

```bash
find ~/.cargo/registry/src -type d -name "duroxide-*" | sort -V | tail -1
```

### Step 4: Read CHANGELOG.md

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
- Memory optimizations or API changes

### Step 5: Read Provider Implementation Guide

```bash
cat $(find ~/.cargo/registry/src -type d -name "duroxide-*" | sort -V | tail -1)/docs/provider-implementation-guide.md
```

**Key things to compare:**
- New required trait methods
- Updated signatures for existing methods
- New error handling requirements
- Schema changes needed
- Instance creation or locking behavior changes

### Step 6: Read Provider Testing Guide

```bash
cat $(find ~/.cargo/registry/src -type d -name "duroxide-*" | sort -V | tail -1)/docs/provider-testing-guide.md
```

**Key things to check:**
- New validation tests to add
- Updated test counts (e.g., "62 tests" → "64 tests")
- Changes to `ProviderFactory` or `ProviderStressFactory` traits
- New stress test scenarios

### Step 7: Verify Build

```bash
cargo build
```

Check for:
- Compilation errors indicating breaking changes
- Missing trait implementations
- Type mismatches

If code changes are needed, implement them. If migrations are needed:
1. Create new migration file `migrations/NNNN_description.sql`
2. Update `0001_initial_schema.sql` with the complete schema (this is the single source of truth)
3. **Note:** For future releases, consider adding migration delta scripts similar to duroxide-pg to support incremental upgrades

### Step 8: Run Validation Tests

```bash
cargo test --test postgres_provider_test -- --nocapture
```

Check for:
- New tests that need wrappers added
- Failing tests indicating behavioral changes
- Removed tests that should be cleaned up

### Step 9: Generate Summary Report

After completing the above, provide a summary with:

1. **Version Change:** `X.Y.Z` → `A.B.C`
2. **Breaking Changes:** List any breaking changes affecting this provider
3. **New Features:** Features that could be leveraged
4. **New Tests:** New validation tests to add (with count)
5. **Required Code Changes:** Specific changes needed in duroxide-pg-opt
6. **Action Items:** Prioritized list of tasks

---

## Example Summary Template

```markdown
## Duroxide Update Summary

**Version Change:** 0.1.5 → 0.1.6

### Breaking Changes
- None

### New Features
- Large payload stress test
- `HistoryManager` memory optimizations (full_history_len, is_full_history_empty, full_history_iter)

### New Validation Tests
- Count: 62 (unchanged from previous)
- New tests: None

### Required Code Changes
- None required for compatibility

### Action Items
1. ✅ Update Cargo.lock (done)
2. ⏳ Consider adding large payload stress test to pg-stress
3. ⏳ Review memory optimization patterns for applicability
```

---

## Quick One-Liner

For quick version check without full sync:

```bash
cargo update duroxide --dry-run 2>&1 | grep duroxide
```
