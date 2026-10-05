// Released under the MIT License.
// Copyright, 2026, by Samuel Williams.

use bake::{Context, Error, Result, Value};
use serde_json::json;

pub(crate) fn release(context: &Context, version: &str, push: bool) -> Result<Value> {
    let tag = format!("v{version}");
    let status = git_output(
        context,
        ["status", "--porcelain", "--untracked-files=normal"],
    )?;
    if !status.stdout.is_empty() {
        return Err(Error::new(
            "the working tree must be clean before creating a release tag",
        ));
    }

    if push {
        git_output(context, ["remote", "get-url", "origin"])?;
    }

    let tags = git_output(context, ["tag", "--list", &tag])?;
    if !tags.stdout.is_empty() {
        return Err(Error::new(format!("release tag {tag:?} already exists")));
    }

    let packages = crate::cargo::workspace_packages(context)?;
    let mut package_arguments = vec!["package".to_owned(), "--locked".to_owned()];
    for package in packages {
        package_arguments.extend(["--package".to_owned(), package.name]);
    }
    crate::cargo::run_cargo_arguments(context, &package_arguments)?;
    git_run(
        context,
        ["tag", "-a", &tag, "-m", &format!("Release {tag}")],
    )?;

    if push && let Err(error) = git_run(context, ["push", "origin", &format!("refs/tags/{tag}")]) {
        let _ = git_run(context, ["tag", "--delete", &tag]);
        return Err(Error::new(format!(
            "created local tag {tag}, but pushing it failed: {error}; the local tag was removed"
        )));
    }

    Ok(json!({
        "version": version,
        "tag": tag,
        "pushed": push,
        "next": if push {
            "GitHub Actions will publish the workspace from this tag."
        } else {
            "Push this tag to origin to trigger the configured publishing workflow."
        },
    }))
}

fn git_output<const COUNT: usize>(
    context: &Context,
    arguments: [&str; COUNT],
) -> Result<std::process::Output> {
    let output = context.command("git").args(arguments).output()?;
    if !output.status.success() {
        return Err(Error::new(format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output)
}

fn git_run<const COUNT: usize>(context: &Context, arguments: [&str; COUNT]) -> Result<()> {
    let output = context.command("git").args(arguments).output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::new(format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Environment, Project};

    fn git(project: &Project, environment: &mut Environment) {
        let script = r#"#!/bin/sh
printf '%s\n' "$*" >> "$BAKE_TEST_GIT_LOG"
case "$1 $2" in
  "status --porcelain")
    if [ "$BAKE_TEST_GIT_DIRTY" = true ]; then echo ' M file'; fi
    if [ "$BAKE_TEST_GIT_FAILURE" = status ]; then echo status-failed >&2; exit 3; fi
    ;;
  "remote get-url")
    if [ "$BAKE_TEST_GIT_FAILURE" = remote ]; then echo remote-failed >&2; exit 3; fi
    echo https://github.com/socketry/fixture.git
    ;;
  "tag --list")
    if [ "$BAKE_TEST_GIT_TAG_EXISTS" = true ]; then echo "$3"; fi
    if [ "$BAKE_TEST_GIT_FAILURE" = list ]; then echo list-failed >&2; exit 3; fi
    ;;
  "tag -a")
    if [ "$BAKE_TEST_GIT_FAILURE" = tag ]; then echo tag-failed >&2; exit 3; fi
    ;;
  "push origin")
    if [ "$BAKE_TEST_GIT_FAILURE" = push ]; then echo push-failed >&2; exit 3; fi
    ;;
  "tag --delete")
    if [ "$BAKE_TEST_GIT_FAILURE" = delete ] || [ "$BAKE_TEST_GIT_FAILURE_DELETE" = true ]; then echo delete-failed >&2; exit 3; fi
    ;;
esac
exit 0
"#;
        project.executable("git", script);
        environment.prepend_path(&project.root().join("bin"));
        environment.set(
            "BAKE_TEST_GIT_LOG",
            project.root().join("git.log").as_os_str(),
        );
        environment.set("BAKE_TEST_GIT_DIRTY", "false");
        environment.set("BAKE_TEST_GIT_TAG_EXISTS", "false");
        environment.set("BAKE_TEST_GIT_FAILURE", "");
    }

    fn project() -> (Project, Environment) {
        let project = Project::new();
        let mut environment = Environment::new();
        project.single_package("fixture", "1.2.3");
        project.cargo_proxy(&mut environment, None);
        git(&project, &mut environment);
        (project, environment)
    }

    #[test]
    fn packages_and_tags_a_release_without_pushing() {
        let (project, _environment) = project();

        let result = release(&project.context(), "1.2.3", false).unwrap();

        assert_eq!(result["tag"], "v1.2.3");
        assert_eq!(result["pushed"], false);
        let log = std::fs::read_to_string(project.root().join("git.log")).unwrap();
        assert!(!log.contains("remote get-url origin"));
        assert!(log.contains("tag -a v1.2.3 -m Release v1.2.3"));
        assert!(
            project
                .cargo_arguments()
                .contains("package --locked --package fixture")
        );
    }

    #[test]
    fn pushes_a_release_tag_when_requested() {
        let (project, _environment) = project();

        let result = release(&project.context(), "1.2.3", true).unwrap();

        assert_eq!(result["pushed"], true);
        assert!(result["next"].as_str().unwrap().contains("GitHub Actions"));
        let log = std::fs::read_to_string(project.root().join("git.log")).unwrap();
        assert!(log.contains("remote get-url origin"));
        assert!(log.contains("push origin refs/tags/v1.2.3"));
    }

    #[test]
    fn rejects_dirty_trees_missing_remotes_and_existing_tags() {
        let (project, mut environment) = project();
        environment.set("BAKE_TEST_GIT_DIRTY", "true");
        assert!(
            release(&project.context(), "1.2.3", false)
                .unwrap_err()
                .to_string()
                .contains("working tree must be clean")
        );

        environment.set("BAKE_TEST_GIT_DIRTY", "false");
        environment.set("BAKE_TEST_GIT_FAILURE", "remote");
        assert!(
            release(&project.context(), "1.2.3", true)
                .unwrap_err()
                .to_string()
                .contains("remote get-url origin failed")
        );

        environment.set("BAKE_TEST_GIT_FAILURE", "");
        environment.set("BAKE_TEST_GIT_TAG_EXISTS", "true");
        assert!(
            release(&project.context(), "1.2.3", false)
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );

        environment.set("BAKE_TEST_GIT_TAG_EXISTS", "false");
        environment.set("BAKE_TEST_GIT_FAILURE", "list");
        assert!(
            release(&project.context(), "1.2.3", false)
                .unwrap_err()
                .to_string()
                .contains("git tag --list")
        );
    }

    #[test]
    fn removes_a_local_tag_when_pushing_fails() {
        let (project, mut environment) = project();
        environment.set("BAKE_TEST_GIT_FAILURE", "push");

        let error = release(&project.context(), "1.2.3", true).unwrap_err();

        assert!(error.to_string().contains("pushing it failed"));
        let log = std::fs::read_to_string(project.root().join("git.log")).unwrap();
        assert!(log.contains("tag --delete v1.2.3"));
    }

    #[test]
    fn reports_cargo_and_git_command_failures() {
        let (project, mut environment) = project();
        environment.set("BAKE_TEST_CARGO_FAILURE", "package");
        assert!(
            release(&project.context(), "1.2.3", false)
                .unwrap_err()
                .to_string()
                .contains("cargo package --locked")
        );

        environment.set("BAKE_TEST_CARGO_FAILURE", "");
        environment.set("BAKE_TEST_GIT_FAILURE", "status");
        assert!(
            release(&project.context(), "1.2.3", false)
                .unwrap_err()
                .to_string()
                .contains("git status --porcelain")
        );

        environment.set("BAKE_TEST_GIT_FAILURE", "tag");
        assert!(
            release(&project.context(), "1.2.3", false)
                .unwrap_err()
                .to_string()
                .contains("git tag -a")
        );

        environment.set("BAKE_TEST_GIT_FAILURE", "push");
        environment.set("BAKE_TEST_GIT_FAILURE_DELETE", "true");
        assert!(release(&project.context(), "1.2.3", true).is_err());
    }

    #[test]
    fn reports_unstartable_git_and_cargo_commands() {
        let (project, mut environment) = project();
        environment.set("PATH", project.root());
        assert!(git_output(&project.context(), ["status"]).is_err());
        assert!(git_run(&project.context(), ["tag"]).is_err());

        environment.prepend_path(&project.root().join("bin"));
        environment.set("CARGO", project.root().join("missing-cargo").as_os_str());
        assert!(release(&project.context(), "1.2.3", false).is_err());
    }
}
