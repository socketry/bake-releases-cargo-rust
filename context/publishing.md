# Publishing workflows

This crate supports tag-based publishing workflows for downstream Cargo workspaces. Its repository uses the standard Socketry Cargo release workflow from `bake-cargo`; the generated workflow described here is a separate feature for consumers that opt into `releases:cargo:release`.

This repository's own `publish.yml` follows the standard workflow from [`bake-cargo`](https://github.com/socketry/bake-cargo-rust/blob/main/readme.md#github-workflow-and-repository-settings). It validates release changes and runs formatting and Clippy on pull requests; `test.yml` supplies pull request tests and coverage. On pushes, the publish check also runs ordinary workspace tests before a release can start.

The task packages the workspace and pushes an annotated `vVERSION` tag. The generated `.github/workflows/publish.yml` runs checks, then asks `releases:cargo:publish:pending` to verify the tag version against every publishable workspace package and report which package versions are already on crates.io. After GitHub Actions obtains a short-lived crates.io token, `releases:cargo:publish:workspace` rechecks the registry and publishes only remaining packages. Keep registry checks and Cargo commands in these Bake tasks so generated workflows contain no embedded Python or shell programs.

The workflow still uses `rust-lang/crates-io-auth-action` to exchange GitHub's OIDC identity for a crates.io token. It uses the `crates-io` environment so repository owners can configure approval requirements and trusted publishing.
