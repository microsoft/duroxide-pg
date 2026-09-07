# Release Policy

`duroxide-pg` is published to crates.io by Microsoft's internal open-source
release infrastructure. Publishing credentials and release execution remain
outside this repository.

## Contributor Workflow

Contributors can prepare a release through a pull request:

1. Update the version in `Cargo.toml`.
2. Update `CHANGELOG.md` and relevant documentation.
3. Run the build, tests, and documentation checks.
4. Open a pull request for review.

After the release change is merged, the repository's `release-preparation` skill
can create the matching annotated `vX.Y.Z` tag locally on the merged `main`
commit. The skill never pushes the tag. A Microsoft maintainer handles the
remote tag operation and uses the internal release pipeline to build and publish
the crate to crates.io.

## Publishing Boundary

- Never run `cargo publish`; direct publication is not permitted.
- Do not create a GitHub Release manually.
- Do not request or store Microsoft publishing credentials in this repository.

For release questions, open a GitHub issue without including credentials or
internal pipeline configuration.
