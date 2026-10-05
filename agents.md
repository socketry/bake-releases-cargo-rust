# Agent instructions

Read the shared context index at `.agents/context/index.md` and follow the
relevant Socketry project and Bake task guidance before changing this crate.

This crate exposes Bake tasks and can generate tag-based Cargo publishing
workflows for downstream workspaces. Treat task names and generated workflow
steps as public interfaces. Keep workflow operations in Bake tasks instead of
inline scripts, and preserve the generated workflow's tag and trusted-publishing
behavior.

Regenerate task links with `cargo bake --regenerate` after changing dependencies
in the private `bake/` package. The standard `test.yml` workflow requires full
source-region coverage through `cargo bake test:coverage`.
