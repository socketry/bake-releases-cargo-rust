# Releases

## Unreleased

- Generate tag-based publishing workflows that perform registry checks and package publication through Bake tasks.

## v0.1.0

- Move the Cargo, GitHub, and crates.io release tasks into their own repository.
- Support Cargo workspace discovery, publishing workflow setup, and trusted publishing.
- Add shared workspace version bumps and a tag-based release task.
- Skip package versions already published when running generated release workflows.
