# Publish Crate to crates.io

This prompt guides the process of publishing a new version of duroxide-pg to crates.io.

## Steps

### 1. Version Bump

Update the version in `Cargo.toml`:

```toml
[package]
name = "duroxide-pg"
version = "0.1.X"  # Increment X only
```

**Version increment rules:**
- **AI is authorized to bump the patch version only: `0.1.X` → `0.1.(X+1)`**
- Breaking changes are acceptable while version is below 1.0.0 (pre-stable)
- Bumping minor (0.X.0) or major (X.0.0) requires explicit user approval

### 2. Update CHANGELOG.md

Add a new section at the top of CHANGELOG.md (after the header), following this format:

```markdown
## [X.Y.Z] - YYYY-MM-DD

### Added
- New feature descriptions

### Changed
- Modifications to existing features
- API changes (note if breaking)

### Fixed
- Bug fixes

### Notes
- Any additional context (e.g., test counts, compatibility notes)
```

**Guidelines:**
- Use present tense ("Add feature" not "Added feature")
- Be specific about what changed and why
- Note breaking changes prominently with **BREAKING:**
- Include relevant test counts or compatibility notes

### 3. Update README.md - Latest Release Section

Replace the "Latest Release" section in README.md with a summary of the new version:

```markdown
## Latest Release (X.Y.Z)

- Brief bullet points of the most notable changes
- Focus on user-facing changes (not internal refactoring)
- Keep to 3-5 bullet points maximum
- See [CHANGELOG.md](CHANGELOG.md) for full version history
```

**Location in README.md:** Look for the existing `## Latest Release (X.Y.Z)` section and replace it entirely.

### 4. Verify Build and Tests

```bash
cargo build
cargo test
cargo doc --no-deps
```

Ensure all tests pass and documentation builds without errors.

### 5. Commit Changes

```bash
git add Cargo.toml CHANGELOG.md README.md
git commit -m "chore: bump version to X.Y.Z"
```

### 6. Create Git Tag

```bash
git tag vX.Y.Z
git push origin main --tags
```

### 7. Publish to crates.io

```bash
cargo publish
```

**Prerequisites:**
- Must be logged in: `cargo login`
- Must have publish permissions for the crate

### 8. Verify Publication

- Check https://crates.io/crates/duroxide-pg
- Verify version appears and documentation is correct

## Example

For reference, here's how 0.1.3 was released (poison message handling):

**Cargo.toml:**
```toml
version = "0.1.3"
```

**CHANGELOG.md:**
```markdown
## [0.1.3] - 2024-12-14

### Changed
- **BREAKING:** Updated to duroxide 0.1.2 API with poison message handling
- `fetch_orchestration_item` now returns `(OrchestrationItem, String, u32)` tuple
- `fetch_work_item` now returns `(WorkItem, String, u32)` tuple (added attempt_count)

### Added
- New migration `0003_add_attempt_count.sql` - adds `attempt_count` column to queue tables
- `abandon_work_item()` method - explicit work item lock release
- `renew_orchestration_item_lock()` method - extends orchestration lock timeout
- 8 new poison message validation tests

### Notes
- Total validation tests: 58 (up from 50)
```

**README.md Latest Release section:**
```markdown
## Latest Release (0.1.3)

- Updated to duroxide 0.1.2 with poison message handling
- Added `abandon_work_item()` and `renew_orchestration_item_lock()` methods
- 8 new validation tests for poison message detection
- See [CHANGELOG.md](CHANGELOG.md) for full version history
```
