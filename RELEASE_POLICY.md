# Release Policy

`duroxide-pg` is published to crates.io by Microsoft's internal open-source
release infrastructure. The public repository contains the source, tests, and
contributor release preparation; publishing credentials and release execution
remain outside this repository.

## For Open Source Contributors

Contributors can propose version updates through pull requests. The normal
workflow is:

1. Update `Cargo.toml`, `CHANGELOG.md`, and the relevant documentation.
2. Run the build, tests, and documentation checks.
3. Open a pull request for review.

After Microsoft maintainers merge a release change and create the corresponding
version tag, the internal release process builds the tagged source and publishes
the crate to crates.io.

## What Contributors Should Not Do

- Do not run `cargo publish` for the Microsoft release.
- Do not create a manual crates.io release on behalf of the project.
- Do not request or store Microsoft publishing credentials in this repository.

## For Microsoft Maintainers

After a release pull request is approved:

1. Merge the pull request to `main`.
2. Create and push the matching `vX.Y.Z` tag.
3. Allow the internal OSS release pipeline to build and publish the crate.

The internal pipeline validates the source, runs required compliance checks, and
records release metadata and audit information. It publishes using the
`microsoft-oss-releases` crates.io owner identity.

## Crates.io Integrity

crates.io does not provide cryptographic signing for published Rust crates.
Integrity and authenticity rely on HTTPS transport, crates.io account security,
and Cargo's package checksums recorded in `Cargo.lock`.

## Support

For questions about a release, open an issue in this repository or contact the
duroxide maintainers. Do not add credentials or internal pipeline configuration
to a public issue or pull request.