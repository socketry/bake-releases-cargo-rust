//! Cargo, GitHub Actions, and crates.io release tasks for Bake.
//!
//! The reusable tasks register beneath `releases:cargo`. They inspect workspace
//! metadata, generate a trusted-publishing workflow, and manage the GitHub and
//! crates.io settings needed to publish workspace packages.

mod cargo;
mod crates_io;
mod github;
mod release;
mod version;

/// Cargo release and publication tasks.
pub mod releases {
    /// Tasks and helpers for publishing Cargo packages from GitHub Actions.
    pub mod cargo {
        use bake::{Context, Error, Result, Value};
        use serde_json::json;
        use std::fs;
        use std::path::PathBuf;

        use crate::{cargo as cargo_helpers, crates_io, github as github_helpers};

        /// Package the publishable workspace, create a version tag, and push it to origin.
        /// The configured GitHub Actions workflow publishes the workspace after the tag arrives.
        #[bake::task]
        pub fn release(context: &mut Context, #[bake(default = true)] push: bool) -> Result<Value> {
            let version = crate::version::workspace_version(context)?;
            crate::release::release(context, &version, push)
        }

        /// List publishable packages in the current Cargo workspace.
        #[bake::task]
        pub fn packages(context: &mut Context) -> Result<Value> {
            Ok(json!(cargo_helpers::workspace_packages(context)?))
        }

        /// Build and validate the package archive for one workspace package.
        #[bake::task(name = "package")]
        pub fn create_package_archive(context: &mut Context, package: String) -> Result<String> {
            cargo_helpers::run_cargo(context, ["package", "--locked", "--package", &package])?;
            Ok(format!("Packaged {package}"))
        }

        /// Publish one workspace package to crates.io using the configured Cargo credentials.
        #[bake::task]
        pub fn publish(context: &mut Context, package: String) -> Result<String> {
            cargo_helpers::run_cargo(context, ["publish", "--locked", "--package", &package])?;
            Ok(format!("Published {package}"))
        }

        /// Publish a package once, then register this repository's trusted publisher.
        ///
        /// If publisher registration fails after Cargo accepts the upload, the package
        /// remains published; rerun `releases:cargo:trusted-publishing:configure` to finish setup.
        #[bake::task]
        pub fn bootstrap(
            context: &mut Context,
            package: String,
            #[bake(default = "publish.yml")] workflow: String,
            #[bake(default = "crates-io")] environment: String,
        ) -> Result<Value> {
            cargo_helpers::package_by_name(context, &package)?;
            let repository = github_helpers::Repository::from_origin(context)?;
            crates_io::validate_trusted_publisher_inputs(&package, &workflow, &environment)?;
            cargo_helpers::run_cargo(context, ["publish", "--locked", "--package", &package])
                .map_err(|error| {
                    Error::new(format!("initial crates.io publication failed: {error}"))
                })?;

            let configuration = crates_io::configure_trusted_publisher(
                &package,
                &repository,
                &workflow,
                &environment,
            )
            .map_err(|error| {
                Error::new(format!(
                    "{package} was published, but trusted publisher setup failed: {error}; rerun `releases:cargo:trusted-publishing:configure {package}`"
                ))
            })?;

            Ok(json!({
                "package": package,
                "initial_publish": "complete",
                "trusted_publisher": configuration,
                "trusted_publishing_only": false,
                "next": format!("Review the workflow and publisher configuration, then run releases:cargo:trusted-publishing:require {package} --required true when ready."),
            }))
        }

        /// Generate or update the GitHub Actions workflow for workspace package publication.
        /// Existing files are preserved unless `--force true` is supplied.
        pub mod setup {
            use super::*;

            /// Write `.github/workflows/publish.yml` for the current workspace.
            #[bake::task]
            pub fn workflow(
                context: &mut Context,
                #[bake(default = "publish.yml")] filename: String,
                #[bake(default = "main")] branch: String,
                #[bake(default = false)] force: bool,
            ) -> Result<String> {
                let packages = cargo_helpers::workspace_packages(context)?;
                if packages.is_empty() {
                    return Err(Error::new("the workspace has no publishable packages"));
                }
                github_helpers::validate_branch(&branch)?;
                if PathBuf::from(&filename).components().count() != 1
                    || filename.is_empty()
                    || !filename.ends_with(".yml") && !filename.ends_with(".yaml")
                {
                    return Err(Error::new(
                        "workflow filename must be a single .yml or .yaml filename",
                    ));
                }

                let contents = cargo_helpers::publish_workflow(&packages, &branch)?;
                let path = context
                    .root()
                    .join(".github")
                    .join("workflows")
                    .join(filename);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }

                if path.exists() {
                    let existing = fs::read_to_string(&path)?;
                    if existing == contents {
                        return Ok(format!(
                            "{} already matches the generated workflow",
                            path.display()
                        ));
                    }
                    if !force {
                        return Err(Error::new(format!(
                            "{} already exists and differs; review it, or pass --force true to replace it",
                            path.display()
                        )));
                    }
                }

                fs::write(&path, contents)?;
                Ok(format!(
                    "Generated {} for {} package(s)",
                    path.display(),
                    packages.len()
                ))
            }

            /// GitHub repository ruleset and environment setup tasks.
            pub mod github {
                use super::super::*;

                /// Show the desired repository rulesets and publishing environment.
                #[bake::task]
                #[allow(clippy::too_many_arguments)]
                pub fn plan(
                    context: &mut Context,
                    #[bake(default = "")] repository: String,
                    #[bake(default = "main")] branch: String,
                    #[bake(default = 1)] approvals: u32,
                    checks: Vec<String>,
                    reviewers: Vec<String>,
                    wait_timer: Option<u32>,
                    #[bake(default = "crates-io")] environment: String,
                ) -> Result<Value> {
                    let repository = github_helpers::Repository::resolve(context, &repository)?;
                    github_helpers::validate_setup(
                        &branch,
                        approvals,
                        &checks,
                        &reviewers,
                        wait_timer,
                        &environment,
                    )?;
                    let packages = cargo_helpers::workspace_packages(context)?;
                    if packages.is_empty() {
                        return Err(Error::new("the workspace has no publishable packages"));
                    }
                    let checks = github_helpers::effective_checks(&checks);
                    Ok(github_helpers::setup_plan(
                        &repository,
                        &branch,
                        approvals,
                        &checks,
                        &reviewers,
                        wait_timer,
                        &environment,
                    ))
                }

                /// Apply the managed rulesets and create/update the publishing environment.
                /// Run `releases:cargo:setup:github:plan` first and review its output.
                #[bake::task]
                #[allow(clippy::too_many_arguments)]
                pub fn apply(
                    context: &mut Context,
                    #[bake(default = "")] repository: String,
                    #[bake(default = "main")] branch: String,
                    #[bake(default = 1)] approvals: u32,
                    checks: Vec<String>,
                    reviewers: Vec<String>,
                    wait_timer: Option<u32>,
                    #[bake(default = "crates-io")] environment: String,
                ) -> Result<Value> {
                    let repository = github_helpers::Repository::resolve(context, &repository)?;
                    github_helpers::validate_setup(
                        &branch,
                        approvals,
                        &checks,
                        &reviewers,
                        wait_timer,
                        &environment,
                    )?;
                    let checks = github_helpers::effective_checks(&checks);
                    github_helpers::apply_setup(
                        context,
                        &repository,
                        &branch,
                        approvals,
                        &checks,
                        &reviewers,
                        wait_timer,
                        &environment,
                    )
                }
            }
        }

        /// Configure crates.io's GitHub Actions trusted publisher for one package.
        pub mod trusted_publishing {
            use super::*;

            /// Show the trusted publisher configuration that would be registered.
            #[bake::task]
            pub fn plan(
                context: &mut Context,
                package: String,
                #[bake(default = "publish.yml")] workflow: String,
                #[bake(default = "crates-io")] environment: String,
            ) -> Result<Value> {
                cargo_helpers::package_by_name(context, &package)?;
                let repository = github_helpers::Repository::from_origin(context)?;
                crates_io::trusted_publisher_plan(&package, &repository, &workflow, &environment)
            }

            /// Add the GitHub Actions trusted publisher configuration on crates.io.
            #[bake::task]
            pub fn configure(
                context: &mut Context,
                package: String,
                #[bake(default = "publish.yml")] workflow: String,
                #[bake(default = "crates-io")] environment: String,
            ) -> Result<Value> {
                cargo_helpers::package_by_name(context, &package)?;
                let repository = github_helpers::Repository::from_origin(context)?;
                crates_io::configure_trusted_publisher(
                    &package,
                    &repository,
                    &workflow,
                    &environment,
                )
            }

            /// Enable or disable crates.io's trusted-publishing-only requirement.
            #[bake::task]
            pub fn require(
                context: &mut Context,
                package: String,
                #[bake(default = true)] required: bool,
            ) -> Result<Value> {
                cargo_helpers::package_by_name(context, &package)?;
                crates_io::set_trusted_publishing_only(&package, required)
            }
        }
    }

    /// Change the shared stable version of the publishable Cargo workspace packages.
    pub mod version {
        use bake::{Context, Result, Value};

        /// Increment the patch component of the workspace version.
        #[bake::task]
        pub fn patch(context: &mut Context) -> Result<Value> {
            crate::version::increment(context, crate::version::Component::Patch)
        }

        /// Increment the minor component and reset patch to zero.
        #[bake::task]
        pub fn minor(context: &mut Context) -> Result<Value> {
            crate::version::increment(context, crate::version::Component::Minor)
        }

        /// Increment the major component and reset minor and patch to zero.
        #[bake::task]
        pub fn major(context: &mut Context) -> Result<Value> {
            crate::version::increment(context, crate::version::Component::Major)
        }

        /// Set the workspace to an explicit stable version greater than its current version.
        #[bake::task]
        pub fn bump(
            context: &mut Context,
            #[bake(
                named,
                help = "New stable workspace version in MAJOR.MINOR.PATCH form."
            )]
            version: String,
        ) -> Result<Value> {
            crate::version::set(context, &version)
        }
    }
}
