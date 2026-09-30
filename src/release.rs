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
