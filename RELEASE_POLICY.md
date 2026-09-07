# Release Policy

`duroxide-pg` is published to crates.io by Microsoft's internal open-source
release infrastructure. Publishing credentials and release execution remain
outside this repository.

## Contributor Workflow

Contributors can prepare a release through a pull request:

1. Update the version in `Cargo.toml`.
2. Update `CHANGELOG.md` and relevant documentation.
3. Run `cargo package --allow-dirty` to validate the release package.
4. Open a pull request for review.

The repository's `release-preparation` skill can perform these steps and create
the pull request after explicit user approval. Build and test validation is
handled by the pull request checks.

After the release pull request is merged into `main`, the skill verifies the
merged commit and asks for fresh user approval before creating and pushing the
matching annotated `vX.Y.Z` tag. Tagging does not itself publish the crate or
trigger the internal release pipeline. Publication is a separate Microsoft
internal process.

## Publishing Boundary

- Never run `cargo publish`; direct publication is not permitted.
- Do not create a GitHub Release manually.
- Do not request or store Microsoft publishing credentials in this repository.

For release questions, open a GitHub issue without including credentials or
internal pipeline configuration.
