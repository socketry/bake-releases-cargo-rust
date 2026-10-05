// Released under the MIT License.
// Copyright, 2026, by Samuel Williams.

//! Cargo, GitHub Actions, and crates.io release tasks for Bake.
//!
//! The reusable tasks register beneath `releases:cargo`. They inspect workspace
//! metadata, generate a trusted-publishing workflow, and manage the GitHub and
//! crates.io settings needed to publish workspace packages.
mod cargo;
mod crates_io;
mod github;
mod release;
#[cfg(test)]
mod test_support;
mod version_support;

use bake::{Context, Error, Result, Value};
use serde_json::json;
use std::fs;
use std::path::PathBuf;

use crate::{cargo as cargo_helpers, github as github_helpers};

/// Package the publishable workspace, create a version tag, and push it to origin.
/// The configured GitHub Actions workflow publishes the workspace after the tag arrives.
#[bake::task]
pub fn release(context: &mut Context, #[bake(default = true)] push: bool) -> Result<Value> {
    let version = crate::version_support::workspace_version(context)?;
    crate::release::release(context, &version, push)
}

/// List publishable packages in the current Cargo workspace.
#[bake::task]
pub fn packages(context: &mut Context) -> Result<Value> {
    Ok(json!(cargo_helpers::workspace_packages(context)?))
}

/// Build and validate the package archive for one workspace package.
#[bake::task(name = "releases:cargo:package")]
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

/// Report which workspace packages for a release version still need publishing.
#[bake::task(name = "releases:cargo:publish:pending")]
pub fn publish_pending(context: &mut Context, version: String) -> Result<Value> {
    let (pending, published) = cargo_helpers::publication_state(context, &version)?;
    let has_packages = !pending.is_empty();
    cargo_helpers::append_github_output("version", &version)?;
    cargo_helpers::append_github_output(
        "has_packages",
        if has_packages { "true" } else { "false" },
    )?;

    Ok(json!({
        "version": version,
        "has_packages": has_packages,
        "pending": pending.iter().map(|package| package.name.as_str()).collect::<Vec<_>>(),
        "published": published.iter().map(|package| package.name.as_str()).collect::<Vec<_>>(),
    }))
}

/// Publish workspace packages that do not yet exist at the requested version.
#[bake::task(name = "releases:cargo:publish:workspace")]
pub fn publish_workspace(context: &mut Context, version: String) -> Result<Value> {
    let (pending, published) = cargo_helpers::publication_state(context, &version)?;
    if pending.is_empty() {
        return Ok(json!({
            "version": version,
            "published": [],
            "already_published": published.iter().map(|package| package.name.as_str()).collect::<Vec<_>>(),
        }));
    }

    let mut arguments = vec![
        "publish".to_owned(),
        "--workspace".to_owned(),
        "--locked".to_owned(),
    ];
    for package in &published {
        arguments.extend(["--exclude".to_owned(), package.name.clone()]);
    }
    cargo_helpers::run_cargo_arguments(context, &arguments)?;

    Ok(json!({
        "version": version,
        "published": pending.iter().map(|package| package.name.as_str()).collect::<Vec<_>>(),
        "already_published": published.iter().map(|package| package.name.as_str()).collect::<Vec<_>>(),
    }))
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
        .map_err(|error| Error::new(format!("initial crates.io publication failed: {error}")))?;

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
        let parent = path
            .parent()
            .unwrap_or_else(|| unreachable!("workflow paths always have a parent"));
        fs::create_dir_all(parent)?;

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

        write_workflow_file(&path, &contents)?;
        Ok(format!(
            "Generated {} for {} package(s)",
            path.display(),
            packages.len()
        ))
    }

    fn write_workflow_file(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
        #[cfg(test)]
        if std::env::var_os("BAKE_TEST_WORKFLOW_WRITE_FAILURE").is_some() {
            return Err(std::io::Error::other("injected workflow write failure"));
        }

        fs::write(path, contents)
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
        crates_io::configure_trusted_publisher(&package, &repository, &workflow, &environment)
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

/// Change the shared stable version of the publishable Cargo workspace packages.
pub mod version {
    use bake::{Context, Result, Value};

    /// Increment the patch component of the workspace version.
    #[bake::task(name = "releases:version:patch")]
    pub fn patch(context: &mut Context) -> Result<Value> {
        crate::version_support::increment(context, crate::version_support::Component::Patch)
    }

    /// Increment the minor component and reset patch to zero.
    #[bake::task(name = "releases:version:minor")]
    pub fn minor(context: &mut Context) -> Result<Value> {
        crate::version_support::increment(context, crate::version_support::Component::Minor)
    }

    /// Increment the major component and reset minor and patch to zero.
    #[bake::task(name = "releases:version:major")]
    pub fn major(context: &mut Context) -> Result<Value> {
        crate::version_support::increment(context, crate::version_support::Component::Major)
    }

    /// Set the workspace to an explicit stable version greater than its current version.
    #[bake::task(name = "releases:version:bump")]
    pub fn bump(
        context: &mut Context,
        #[bake(
            named,
            help = "New stable workspace version in MAJOR.MINOR.PATCH form."
        )]
        version: String,
    ) -> Result<Value> {
        crate::version_support::set(context, &version)
    }
}

/// Compatibility paths for callers using the former namespace wrappers.
#[doc(hidden)]
pub mod releases {
    pub mod cargo {
        pub use crate::{
            bootstrap, bootstrap_task, create_package_archive, create_package_archive_task,
            packages, packages_task, publish, publish_pending, publish_pending_task, publish_task,
            publish_workspace, publish_workspace_task, release, release_task, setup,
            trusted_publishing,
        };
    }
    pub use crate::version;
}

#[cfg(test)]
mod tests {
    use super as tasks;
    use super::version as version_tasks;
    use crate::test_support::{Environment, Project, http_server};
    use bake::Registry;
    use serde_json::json;

    fn project() -> (Project, Environment) {
        let project = Project::new();
        let mut environment = Environment::new();
        project.single_package("fixture", "1.2.3");
        project.cargo_proxy(&mut environment, None);
        (project, environment)
    }

    fn git_origin(project: &Project, environment: &mut Environment) {
        project.executable(
            "git",
            "#!/bin/sh\nif [ \"$1 $2\" = 'remote get-url' ]; then echo https://github.com/socketry/fixture.git; exit 0; fi\nif [ \"$1 $2\" = 'status --porcelain' ]; then exit 0; fi\nif [ \"$1 $2\" = 'tag --list' ]; then exit 0; fi\nif [ \"$1 $2\" = 'tag -a' ]; then exit 0; fi\necho unexpected-git >&2\nexit 1\n",
        );
        environment.prepend_path(&project.root().join("bin"));
    }

    fn gh_for_setup(project: &Project, environment: &mut Environment) {
        project.executable(
            "gh",
            "#!/bin/sh\ncase \"$4\" in\n  */environments?per_page=100) echo '{\"environments\":[]}';;\n  */rulesets?per_page=100) echo '[]';;\n  *) cat >/dev/null; echo '{\"id\":1}';;\nesac\n",
        );
        environment.prepend_path(&project.root().join("bin"));
    }

    #[test]
    fn discovers_the_tag_publishing_tasks() {
        let registry = Registry::discover().unwrap();
        let names = registry.tasks().map(|task| task.name()).collect::<Vec<_>>();

        assert!(names.contains(&"releases:cargo:publish:pending"));
        assert!(names.contains(&"releases:cargo:publish:workspace"));
        assert!(names.contains(&"releases:version:patch"));
        assert!(names.contains(&"releases:version:bump"));
    }

    #[test]
    fn exposes_package_archive_and_single_package_publish_tasks() {
        let (project, mut environment) = project();
        let mut context = project.context();

        assert_eq!(tasks::packages(&mut context).unwrap()[0]["name"], "fixture");
        assert_eq!(
            tasks::create_package_archive(&mut context, "fixture".to_owned()).unwrap(),
            "Packaged fixture"
        );
        assert_eq!(
            tasks::publish(&mut context, "fixture".to_owned()).unwrap(),
            "Published fixture"
        );
        assert!(
            project
                .cargo_arguments()
                .contains("package --locked --package fixture")
        );
        assert!(
            project
                .cargo_arguments()
                .contains("publish --locked --package fixture")
        );

        environment.set("BAKE_TEST_CARGO_FAILURE", "package");
        assert!(tasks::create_package_archive(&mut context, "fixture".to_owned()).is_err());
        environment.set("BAKE_TEST_CARGO_FAILURE", "publish");
        assert!(tasks::publish(&mut context, "fixture".to_owned()).is_err());
    }

    #[test]
    fn checks_and_publishes_only_pending_workspace_packages() {
        let project = Project::new();
        let mut environment = Environment::new();
        project.write(
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\"]\nresolver = \"3\"\n",
        );
        for name in ["alpha", "beta"] {
            project.write(
                format!("crates/{name}/Cargo.toml"),
                &format!("[package]\nname = \"{name}\"\nversion = \"1.2.3\"\nedition = \"2024\"\n"),
            );
            project.write(format!("crates/{name}/src/lib.rs"), "// fixture\n");
        }
        project.cargo_proxy(&mut environment, None);
        let output = project.root().join("github-output");
        environment.set("GITHUB_OUTPUT", output.as_os_str());
        let (api, server) = http_server(vec![
            (200, r#"{"versions":[{"num":"1.2.3"}]}"#.to_owned()),
            (404, "{}".to_owned()),
            (200, r#"{"versions":[{"num":"1.2.3"}]}"#.to_owned()),
            (404, "{}".to_owned()),
        ]);
        environment.set("BAKE_TEST_CRATES_IO_API", &api);
        let mut context = project.context();

        let pending = tasks::publish_pending(&mut context, "1.2.3".to_owned()).unwrap();
        assert_eq!(pending["has_packages"], true);
        assert_eq!(pending["pending"], json!(["beta"]));
        assert_eq!(pending["published"], json!(["alpha"]));
        assert_eq!(
            std::fs::read_to_string(output).unwrap(),
            "version=1.2.3\nhas_packages=true\n"
        );
        let published = tasks::publish_workspace(&mut context, "1.2.3".to_owned()).unwrap();
        assert_eq!(published["published"], json!(["beta"]));
        assert!(
            project
                .cargo_arguments()
                .contains("publish --workspace --locked --exclude alpha")
        );
        server.join().unwrap();
    }

    #[test]
    fn skips_publishing_when_all_workspace_versions_already_exist() {
        let (project, mut environment) = project();
        let output = project.root().join("github-output");
        environment.set("GITHUB_OUTPUT", output.as_os_str());
        let (api, server) = http_server(vec![
            (200, r#"{"versions":[{"num":"1.2.3"}]}"#.to_owned()),
            (200, r#"{"versions":[{"num":"1.2.3"}]}"#.to_owned()),
        ]);
        environment.set("BAKE_TEST_CRATES_IO_API", &api);
        let mut context = project.context();

        let pending = tasks::publish_pending(&mut context, "1.2.3".to_owned()).unwrap();
        assert_eq!(pending["has_packages"], false);
        assert_eq!(
            std::fs::read_to_string(output).unwrap(),
            "version=1.2.3\nhas_packages=false\n"
        );
        let response = tasks::publish_workspace(&mut context, "1.2.3".to_owned()).unwrap();

        assert_eq!(response["published"], json!([]));
        assert_eq!(response["already_published"], json!(["fixture"]));
        assert!(!project.cargo_arguments().contains("publish --workspace"));
        server.join().unwrap();
    }

    #[test]
    fn reports_publication_task_version_and_command_failures() {
        let (project, mut environment) = project();
        let mut context = project.context();
        assert!(tasks::publish_pending(&mut context, "9.9.9".to_owned()).is_err());
        assert!(tasks::publish_workspace(&mut context, "9.9.9".to_owned()).is_err());

        let output = project.root().join("missing/output");
        environment.set("GITHUB_OUTPUT", output.as_os_str());
        let (api, server) = http_server(vec![(404, "{}".to_owned())]);
        environment.set("BAKE_TEST_CRATES_IO_API", &api);
        assert!(tasks::publish_pending(&mut context, "1.2.3".to_owned()).is_err());
        server.join().unwrap();

        environment.set(
            "GITHUB_OUTPUT",
            project.root().join("github-output").as_os_str(),
        );
        environment.set("BAKE_TEST_GITHUB_OUTPUT_FAILURE", "has_packages");
        let (api, server) = http_server(vec![(404, "{}".to_owned())]);
        environment.set("BAKE_TEST_CRATES_IO_API", &api);
        assert!(tasks::publish_pending(&mut context, "1.2.3".to_owned()).is_err());
        server.join().unwrap();
        environment.remove("BAKE_TEST_GITHUB_OUTPUT_FAILURE");

        environment.set(
            "GITHUB_OUTPUT",
            project.root().join("github-output").as_os_str(),
        );
        environment.set("BAKE_TEST_CARGO_FAILURE", "publish");
        let (api, server) = http_server(vec![(404, "{}".to_owned())]);
        environment.set("BAKE_TEST_CRATES_IO_API", &api);
        assert!(tasks::publish_workspace(&mut context, "1.2.3".to_owned()).is_err());
        server.join().unwrap();
    }

    #[test]
    fn release_task_creates_a_tag_for_the_current_workspace_version() {
        let (project, mut environment) = project();
        git_origin(&project, &mut environment);

        let result = tasks::release(&mut project.context(), false).unwrap();

        assert_eq!(result["version"], "1.2.3");
        assert_eq!(result["tag"], "v1.2.3");
    }

    #[test]
    fn bootstraps_a_package_and_registers_its_trusted_publisher() {
        let (project, mut environment) = project();
        git_origin(&project, &mut environment);
        environment.set("CARGO_REGISTRY_TOKEN", "secret");
        let config = r#"{"github_config":{"id":1,"crate":"fixture","repository_owner":"socketry","repository_name":"fixture","workflow_filename":"publish.yml","environment":"crates-io"}}"#;
        let (api, server) = http_server(vec![
            (200, r#"{"github_configs":[]}"#.to_owned()),
            (200, config.to_owned()),
        ]);
        environment.set("BAKE_TEST_CRATES_IO_API", &api);
        let mut context = project.context();

        let result = tasks::bootstrap(
            &mut context,
            "fixture".to_owned(),
            "publish.yml".to_owned(),
            "crates-io".to_owned(),
        )
        .unwrap();

        assert_eq!(result["initial_publish"], "complete");
        assert_eq!(result["trusted_publishing_only"], false);
        assert!(
            project
                .cargo_arguments()
                .contains("publish --locked --package fixture")
        );
        server.join().unwrap();
    }

    #[test]
    fn reports_failures_during_package_bootstrap() {
        let (project, mut environment) = project();
        git_origin(&project, &mut environment);
        environment.set("CARGO_REGISTRY_TOKEN", "secret");
        assert!(
            tasks::bootstrap(
                &mut project.context(),
                "missing".to_owned(),
                "publish.yml".to_owned(),
                "crates-io".to_owned(),
            )
            .is_err()
        );
        assert!(
            tasks::bootstrap(
                &mut project.context(),
                "fixture".to_owned(),
                "bad.json".to_owned(),
                "crates-io".to_owned(),
            )
            .is_err()
        );
        environment.set("BAKE_TEST_CARGO_FAILURE", "publish");
        assert!(
            tasks::bootstrap(
                &mut project.context(),
                "fixture".to_owned(),
                "publish.yml".to_owned(),
                "crates-io".to_owned(),
            )
            .unwrap_err()
            .to_string()
            .contains("initial crates.io publication failed")
        );

        environment.set("BAKE_TEST_CARGO_FAILURE", "");
        let (api, server) = http_server(vec![(500, "registry unavailable".to_owned())]);
        environment.set("BAKE_TEST_CRATES_IO_API", &api);
        assert!(
            tasks::bootstrap(
                &mut project.context(),
                "fixture".to_owned(),
                "publish.yml".to_owned(),
                "crates-io".to_owned(),
            )
            .unwrap_err()
            .to_string()
            .contains("was published, but trusted publisher setup failed")
        );
        server.join().unwrap();
    }

    #[test]
    fn creates_preserves_and_replaces_generated_workflows() {
        let (project, mut _environment) = project();
        let mut context = project.context();
        let path = project.root().join(".github/workflows/publish.yml");

        assert!(
            tasks::setup::workflow(
                &mut context,
                "publish.yml".to_owned(),
                "main".to_owned(),
                false,
            )
            .unwrap()
            .contains("Generated")
        );
        let original = std::fs::read_to_string(&path).unwrap();
        assert!(original.contains("releases:cargo:publish:pending"));
        assert!(
            tasks::setup::workflow(
                &mut context,
                "publish.yml".to_owned(),
                "main".to_owned(),
                false,
            )
            .unwrap()
            .contains("already matches")
        );
        assert!(
            tasks::setup::workflow(
                &mut context,
                "publish.yml".to_owned(),
                "develop".to_owned(),
                false,
            )
            .unwrap_err()
            .to_string()
            .contains("already exists and differs")
        );
        tasks::setup::workflow(
            &mut context,
            "publish.yml".to_owned(),
            "develop".to_owned(),
            true,
        )
        .unwrap();
        assert!(
            tasks::setup::workflow(
                &mut context,
                "publish.yaml".to_owned(),
                "main".to_owned(),
                false,
            )
            .unwrap()
            .contains("Generated")
        );
        assert_ne!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn rejects_invalid_workflow_names_branches_and_empty_workspaces() {
        let (project, mut environment) = project();
        let mut context = project.context();
        assert!(
            tasks::setup::workflow(
                &mut context,
                "../publish.yml".to_owned(),
                "main".to_owned(),
                false,
            )
            .unwrap_err()
            .to_string()
            .contains("single .yml or .yaml filename")
        );
        assert!(
            tasks::setup::workflow(
                &mut context,
                "publish.yml".to_owned(),
                "bad/branch".to_owned(),
                false,
            )
            .unwrap_err()
            .to_string()
            .contains("branch")
        );
        assert!(
            tasks::setup::workflow(&mut context, String::new(), "main".to_owned(), false,)
                .unwrap_err()
                .to_string()
                .contains("single .yml or .yaml filename")
        );
        assert!(
            tasks::setup::workflow(
                &mut context,
                "publish.txt".to_owned(),
                "main".to_owned(),
                false,
            )
            .is_err()
        );

        let cargo = project.executable("cargo-empty", "#!/bin/sh\necho '{\"packages\":[]}'\n");
        environment.set("CARGO", cargo.as_os_str());
        assert!(
            tasks::setup::workflow(
                &mut context,
                "publish.yml".to_owned(),
                "main".to_owned(),
                false,
            )
            .unwrap_err()
            .to_string()
            .contains("no publishable packages")
        );

        environment.set("CARGO", project.root().join("missing-cargo").as_os_str());
        assert!(
            tasks::setup::workflow(
                &mut context,
                "publish.yml".to_owned(),
                "main".to_owned(),
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn reports_workflow_directory_and_write_errors() {
        let project = Project::new();
        let mut environment = Environment::new();
        project.single_package("fixture", "1.2.3");
        project.cargo_proxy(&mut environment, None);
        project.write(".github", "not a directory\n");
        assert!(
            tasks::setup::workflow(
                &mut project.context(),
                "publish.yml".to_owned(),
                "main".to_owned(),
                false,
            )
            .is_err()
        );

        drop(environment);
        let project = Project::new();
        let mut environment = Environment::new();
        project.single_package("fixture", "1.2.3");
        project.cargo_proxy(&mut environment, None);
        project.write(".github/workflows/publish.yml/placeholder", "file\n");
        assert!(
            tasks::setup::workflow(
                &mut project.context(),
                "publish.yml".to_owned(),
                "main".to_owned(),
                false,
            )
            .is_err()
        );

        environment.set("BAKE_TEST_WORKFLOW_WRITE_FAILURE", "true");
        assert!(
            tasks::setup::workflow(
                &mut project.context(),
                "other.yml".to_owned(),
                "main".to_owned(),
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_workspaces_with_different_publish_versions() {
        let project = Project::new();
        let mut environment = Environment::new();
        project.write(
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\"]\nresolver = \"3\"\n",
        );
        for (name, version) in [("alpha", "1.2.3"), ("beta", "1.2.4")] {
            project.write(
                format!("crates/{name}/Cargo.toml"),
                &format!(
                    "[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2024\"\n"
                ),
            );
            project.write(format!("crates/{name}/src/lib.rs"), "// fixture\n");
        }
        project.cargo_proxy(&mut environment, None);

        assert!(
            tasks::setup::workflow(
                &mut project.context(),
                "publish.yml".to_owned(),
                "main".to_owned(),
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn reports_workspace_and_trusted_publishing_task_errors() {
        let (project, mut environment) = project();
        let mut context = project.context();
        assert!(
            tasks::bootstrap(
                &mut context,
                "fixture".to_owned(),
                "publish.yml".to_owned(),
                "crates-io".to_owned(),
            )
            .is_err()
        );
        assert!(
            tasks::trusted_publishing::plan(
                &mut context,
                "fixture".to_owned(),
                "publish.yml".to_owned(),
                "crates-io".to_owned(),
            )
            .is_err()
        );
        assert!(
            tasks::trusted_publishing::configure(
                &mut context,
                "fixture".to_owned(),
                "publish.yml".to_owned(),
                "crates-io".to_owned(),
            )
            .is_err()
        );

        let cargo = project.executable(
            "cargo-metadata-failure",
            "#!/bin/sh\necho metadata failed >&2\nexit 2\n",
        );
        environment.set("CARGO", cargo.as_os_str());
        assert!(tasks::release(&mut project.context(), false).is_err());
        assert!(tasks::packages(&mut project.context()).is_err());
        assert!(version_tasks::patch(&mut project.context()).is_err());
        assert!(
            tasks::setup::github::plan(
                &mut project.context(),
                "socketry/fixture".to_owned(),
                "main".to_owned(),
                1,
                vec![],
                vec![],
                None,
                "crates-io".to_owned(),
            )
            .is_err()
        );

        let cargo = project.executable(
            "cargo-empty-metadata",
            "#!/bin/sh\necho '{\"packages\":[]}'\n",
        );
        environment.set("CARGO", cargo.as_os_str());
        let mut context = project.context();
        assert!(
            tasks::setup::github::plan(
                &mut context,
                "socketry/fixture".to_owned(),
                "main".to_owned(),
                1,
                vec![],
                vec![],
                None,
                "crates-io".to_owned(),
            )
            .is_err()
        );
        assert!(
            tasks::setup::github::apply(
                &mut context,
                "socketry/fixture".to_owned(),
                "main".to_owned(),
                1,
                vec![],
                vec![],
                None,
                "crates-io".to_owned(),
            )
            .is_err()
        );
        assert!(
            tasks::trusted_publishing::plan(
                &mut context,
                "fixture".to_owned(),
                "publish.yml".to_owned(),
                "crates-io".to_owned(),
            )
            .is_err()
        );
        assert!(
            tasks::trusted_publishing::configure(
                &mut context,
                "fixture".to_owned(),
                "publish.yml".to_owned(),
                "crates-io".to_owned(),
            )
            .is_err()
        );
        assert!(
            tasks::trusted_publishing::require(&mut context, "fixture".to_owned(), true,).is_err()
        );
    }

    #[test]
    fn plans_and_applies_github_repository_settings() {
        let (project, mut environment) = project();
        let mut context = project.context();
        let plan = tasks::setup::github::plan(
            &mut context,
            "socketry/fixture".to_owned(),
            "main".to_owned(),
            1,
            vec![],
            vec![],
            None,
            "crates-io".to_owned(),
        )
        .unwrap();
        assert_eq!(plan["repository"], "socketry/fixture");
        assert_eq!(
            plan["branch_ruleset"]["rules"][1]["parameters"]["required_status_checks"][0]["context"],
            "Publish to crates.io / check"
        );
        assert!(
            tasks::setup::github::plan(
                &mut context,
                "invalid".to_owned(),
                "main".to_owned(),
                1,
                vec![],
                vec![],
                None,
                "crates-io".to_owned(),
            )
            .is_err()
        );
        assert!(
            tasks::setup::github::plan(
                &mut context,
                "socketry/fixture".to_owned(),
                "bad/branch".to_owned(),
                1,
                vec![],
                vec![],
                None,
                "crates-io".to_owned(),
            )
            .is_err()
        );

        gh_for_setup(&project, &mut environment);
        assert!(
            tasks::setup::github::apply(
                &mut context,
                "invalid".to_owned(),
                "main".to_owned(),
                1,
                vec![],
                vec![],
                None,
                "crates-io".to_owned(),
            )
            .is_err()
        );
        assert!(
            tasks::setup::github::apply(
                &mut context,
                "socketry/fixture".to_owned(),
                "bad/branch".to_owned(),
                1,
                vec![],
                vec![],
                None,
                "crates-io".to_owned(),
            )
            .is_err()
        );
        let result = tasks::setup::github::apply(
            &mut context,
            "socketry/fixture".to_owned(),
            "main".to_owned(),
            1,
            vec![],
            vec![],
            None,
            "crates-io".to_owned(),
        )
        .unwrap();
        assert_eq!(result["repository"], "socketry/fixture");
    }

    #[test]
    fn exposes_trusted_publisher_plan_configuration_and_requirement_tasks() {
        let (project, mut environment) = project();
        git_origin(&project, &mut environment);
        let mut context = project.context();

        let plan = tasks::trusted_publishing::plan(
            &mut context,
            "fixture".to_owned(),
            "publish.yml".to_owned(),
            "crates-io".to_owned(),
        )
        .unwrap();
        assert_eq!(plan["github_config"]["crate"], "fixture");

        environment.set("CARGO_REGISTRY_TOKEN", "secret");
        let created = r#"{"github_config":{"id":1,"crate":"fixture","repository_owner":"socketry","repository_name":"fixture","workflow_filename":"publish.yml","environment":"crates-io"}}"#;
        let (api, server) = http_server(vec![
            (200, r#"{"github_configs":[]}"#.to_owned()),
            (200, created.to_owned()),
        ]);
        environment.set("BAKE_TEST_CRATES_IO_API", &api);
        let configured = tasks::trusted_publishing::configure(
            &mut context,
            "fixture".to_owned(),
            "publish.yml".to_owned(),
            "crates-io".to_owned(),
        )
        .unwrap();
        assert_eq!(configured["status"], "created");
        server.join().unwrap();

        let (api, server) = http_server(vec![(200, r#"{"trustpub_only":true}"#.to_owned())]);
        environment.set("BAKE_TEST_CRATES_IO_API", &api);
        let required =
            tasks::trusted_publishing::require(&mut context, "fixture".to_owned(), true).unwrap();
        assert_eq!(required["trustpub_only"], true);
        server.join().unwrap();
    }

    #[test]
    fn exposes_all_workspace_version_tasks() {
        let (project, _environment) = project();
        let mut context = project.context();

        assert_eq!(
            version_tasks::patch(&mut context).unwrap()["version"],
            "1.2.4"
        );
        assert_eq!(
            version_tasks::minor(&mut context).unwrap()["version"],
            "1.3.0"
        );
        assert_eq!(
            version_tasks::major(&mut context).unwrap()["version"],
            "2.0.0"
        );
        assert_eq!(
            version_tasks::bump(&mut context, "2.1.0".to_owned()).unwrap()["version"],
            "2.1.0"
        );
    }
}
