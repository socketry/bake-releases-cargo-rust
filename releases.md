# Releases

## Unreleased

- Expose task APIs at the crate root while preserving legacy function and descriptor paths and registered command names.

## v0.2.1

- Adopt `socketry-project` 0.3.7 for shared project tasks and Markdown normalization.
- Require the aggregate test and coverage result for pull request merges.

## v0.2.0

- Migrate public task interfaces from `socketry-bake` to `bake >=0.18.0`.
- Generate tag-based publishing workflows that perform registry checks and package publication through Bake tasks.
- Add `releases:cargo:publish:pending` and `releases:cargo:publish:workspace` tasks.
- Include publishing guidance in the published package's agent context.
- Clean up GitHub API subprocesses when writing their request body fails.

## v0.1.0

- Move the Cargo, GitHub, and crates.io release tasks into their own repository.
- Support Cargo workspace discovery, publishing workflow setup, and trusted publishing.
- Add shared workspace version bumps and a tag-based release task.
- Skip package versions already published when running generated release workflows.
