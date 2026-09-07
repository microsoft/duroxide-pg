---
name: release-preparation
description: Prepare a duroxide-pg release locally by updating version and release notes, validating the crate, and creating a local tag after the release PR is merged. Never push or publish.
---

# Release Preparation

Use this skill when preparing a `duroxide-pg` release or creating its local tag
after the release pull request has merged.

## Non-Negotiable Boundaries

- Never push commits or tags to a remote.
- Never create a pull request.
- Never run `cargo publish` or publish directly to crates.io.
- Never request, use, or store publishing credentials.
- Leave all remote operations and publication to Microsoft maintainers and the
  internal release pipeline.
- Follow [RELEASE_POLICY.md](../../../RELEASE_POLICY.md).

## Phase 1: Prepare the Release Change

Complete this phase before the release pull request is reviewed.

### 1. Determine the Version

- Require an explicit target version.
- Confirm it is a valid semantic version greater than the current version.
- Update only the root package version in `Cargo.toml`.
- Do not change the `pg-stress` package version or dependency versions unless
  they are part of the requested release.

### 2. Update the Changelog

In `CHANGELOG.md`:

1. Keep an empty `## [Unreleased]` section at the top.
2. Move the pending entries into `## [X.Y.Z] - YYYY-MM-DD`.
3. Summarize all user-visible changes included since the previous release.
4. Preserve the existing Keep a Changelog structure and section names.

Do not invent changes. Use the commits and diff since the previous version tag
to confirm the release notes are complete.

### 3. Update the README

In `README.md`:

1. Change `Latest Release` to the target version and summarize its most notable
   user-visible changes in three to five bullets.
2. Move the former latest release summary into `Previous Release`.
3. Keep the link to `CHANGELOG.md` and the release policy notice.

### 4. Validate Locally

Run the repository's existing release checks:

```bash
cargo build
cargo test
cargo doc --no-deps
cargo package --allow-dirty
```

Report any unavailable prerequisite or failing check. Do not bypass failures or
add new tooling solely for release preparation.

### 5. Leave a Local Handoff

Review the diff and summarize:

- Target version
- Release notes added
- Validation results
- Files changed

A local commit may be created, but never push it or create a pull request.

## Phase 2: Create the Post-Merge Local Tag

Run this phase only after the release pull request has merged.

### 1. Verify the Merge State

Before tagging, confirm:

- The current branch is `main`.
- `HEAD` is the merged release commit.
- The working tree is clean.
- `Cargo.toml`, `CHANGELOG.md`, and `README.md` all contain the target version.
- The tag `vX.Y.Z` does not already exist.

If any condition is not met, stop without creating a tag. Never tag a feature
branch or an unmerged release commit.

### 2. Create and Verify the Local Tag

Create an annotated tag:

```bash
git tag -a vX.Y.Z -m "Release vX.Y.Z"
git show --no-patch --decorate vX.Y.Z
```

The skill ends after verifying the local tag. Never push the tag. A Microsoft
maintainer is responsible for all remote tag operations and the internal release
pipeline.
