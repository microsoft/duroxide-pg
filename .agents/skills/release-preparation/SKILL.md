---
name: release-preparation
description: Prepare a duroxide-pg release, create its pull request, and create and push the version tag after the pull request is merged. Never publish directly.
---

# Release Preparation

Use this skill when preparing a `duroxide-pg` release or creating its version
tag after the release pull request has merged.

## Non-Negotiable Boundaries

- Never run `cargo publish` or publish directly to crates.io.
- Never request, use, or store publishing credentials.
- Require explicit user approval before pushing a release branch, creating a
  pull request, or creating and pushing a version tag.
- Publishing remains a separate Microsoft internal release process. Creating or
  pushing a tag does not publish the crate.
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

### 5. Create the Release Pull Request

Review the diff and summarize:

- Target version
- Release notes added
- Validation results
- Files changed

Ask for explicit user approval before performing remote operations. After
approval:

1. Commit the release preparation changes.
2. Push the release branch.
3. Create a pull request targeting `main`.
4. Report the pull request URL and stop.

Do not create a tag while the pull request is open. Run Phase 2 in a later
invocation after the pull request is merged.

## Phase 2: Create the Post-Merge Tag

Run this phase only after the release pull request has merged into `main`.

### 1. Verify the Merge State

Before tagging, confirm:

- The release pull request is merged and its base branch is `main`.
- The merged commit is present in the latest `origin/main`.
- `Cargo.toml`, `CHANGELOG.md`, and `README.md` at the merged commit all contain
  the target version.
- The tag `vX.Y.Z` does not already exist.

If any condition is not met, stop without creating a tag. Never tag a feature
branch or an unmerged release commit.

### 2. Request Tag Approval

Show the user:

- The merged pull request URL
- The exact merged commit to tag
- The proposed tag name `vX.Y.Z`

Ask for explicit approval to create and push the tag. Approval from the pull
request phase does not carry over; obtain fresh approval immediately before
tagging.

### 3. Create, Push, and Verify the Tag

After approval, create an annotated tag on the verified merged commit and push
only that tag:

```bash
git tag -a vX.Y.Z <merged-commit> -m "Release vX.Y.Z"
git push origin vX.Y.Z
git show --no-patch --decorate vX.Y.Z
```

Confirm that the remote tag resolves to the same commit. The skill ends after
tag verification. Do not run `cargo publish`; publication is handled separately
by Microsoft's internal release process.
