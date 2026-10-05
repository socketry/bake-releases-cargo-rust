use bake::{Context, Error, Result};
use serde::Serialize;
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::Write;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct WorkspacePackage {
    pub name: String,
    pub version: String,
    pub manifest_path: String,
}

pub(crate) fn workspace_packages(context: &Context) -> Result<Vec<WorkspacePackage>> {
    let output = command(context, ["metadata", "--format-version", "1", "--no-deps"])?;
    let metadata: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| Error::new(format!("could not parse Cargo metadata: {error}")))?;
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::new("Cargo metadata did not contain a packages array"))?;

    let mut result = Vec::new();
    for package in packages {
        let publishable = match package.get("publish") {
            None | Some(Value::Null) | Some(Value::Bool(true)) => true,
            Some(Value::Bool(false)) => false,
            Some(Value::Array(registries)) => registries
                .iter()
                .any(|registry| registry.as_str() == Some("crates-io")),
            Some(_) => true,
        };
        if !publishable {
            continue;
        }
        let name = package
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::new("Cargo metadata package has no name"))?;
        let version = package
            .get("version")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::new(format!("Cargo metadata package {name:?} has no version")))?;
        let manifest_path = package
            .get("manifest_path")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Error::new(format!(
                    "Cargo metadata package {name:?} has no manifest path"
                ))
            })?;
        result.push(WorkspacePackage {
            name: name.to_owned(),
            version: version.to_owned(),
            manifest_path: manifest_path.to_owned(),
        });
    }

    result.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(result)
}

pub(crate) fn package_by_name(context: &Context, name: &str) -> Result<WorkspacePackage> {
    workspace_packages(context)?
        .into_iter()
        .find(|package| package.name == name)
        .ok_or_else(|| {
            Error::new(format!(
                "publishable package {name:?} was not found in the workspace"
            ))
        })
}

pub(crate) fn run_cargo<const COUNT: usize>(
    context: &Context,
    arguments: [&str; COUNT],
) -> Result<()> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = context.command(cargo);
    command.args(arguments);
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::new(format!(
            "cargo {} failed: {status}",
            arguments.join(" ")
        )))
    }
}

pub(crate) fn run_cargo_arguments(context: &Context, arguments: &[String]) -> Result<()> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = context.command(cargo).args(arguments).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::new(format!(
            "cargo {} failed: {status}",
            arguments.join(" ")
        )))
    }
}

pub(crate) fn publication_state(
    context: &Context,
    version: &str,
) -> Result<(Vec<WorkspacePackage>, Vec<WorkspacePackage>)> {
    let current = crate::version::workspace_version(context)?;
    if current != version {
        return Err(Error::new(format!(
            "release version {version:?} does not match the workspace version {current}"
        )));
    }

    let mut pending = Vec::new();
    let mut published = Vec::new();
    for package in workspace_packages(context)? {
        if crate::crates_io::version_is_published(&package.name, version)? {
            published.push(package);
        } else {
            pending.push(package);
        }
    }

    Ok((pending, published))
}

pub(crate) fn append_github_output(name: &str, value: &str) -> Result<()> {
    let Some(path) = std::env::var_os("GITHUB_OUTPUT") else {
        return Ok(());
    };
    let mut output = OpenOptions::new().append(true).create(true).open(path)?;
    write_github_output(&mut output, name, value)?;
    Ok(())
}

fn write_github_output(output: &mut impl Write, name: &str, value: &str) -> std::io::Result<()> {
    #[cfg(test)]
    if std::env::var("BAKE_TEST_GITHUB_OUTPUT_FAILURE").as_deref() == Ok(name) {
        return Err(std::io::Error::other("injected GitHub output failure"));
    }

    writeln!(output, "{name}={value}")
}

fn command<const COUNT: usize>(
    context: &Context,
    arguments: [&str; COUNT],
) -> Result<std::process::Output> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = context.command(cargo);
    let output = command.args(arguments).output()?;
    if !output.status.success() {
        return Err(Error::new(format!(
            "cargo {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output)
}

pub(crate) fn publish_workflow(packages: &[WorkspacePackage], branch: &str) -> Result<String> {
    let first = packages
        .first()
        .ok_or_else(|| Error::new("the workspace has no publishable packages"))?;
    let version = first.version.as_str();
    if packages.iter().any(|package| package.version != version) {
        return Err(Error::new(
            "workspace release publishing requires every publishable package to share one version",
        ));
    }
    Ok(format!(
        r#"name: Publish to crates.io

on:
  push:
    branches:
      - "{branch}"
    tags:
      - "v*"
  pull_request:
    branches:
      - "{branch}"

permissions:
  contents: read

jobs:
  check:
    runs-on: ubuntu-latest
    timeout-minutes: 20
    outputs:
      version: ${{{{ steps.packages.outputs.version }}}}
      has_packages: ${{{{ steps.packages.outputs.has_packages }}}}
    steps:
      - uses: actions/checkout@v7
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt
      - name: Install Bake launcher
        run: cargo install socketry-cargo-bake --locked
      - name: Check formatting
        run: cargo fmt --all -- --check
      - name: Run Clippy
        run: cargo clippy --workspace --all-targets --locked -- -D warnings
      - name: Run tests
        if: github.event_name == 'push'
        run: cargo test --workspace --locked
      - name: Find workspace packages that need publishing
        if: startsWith(github.ref, 'refs/tags/')
        id: packages
        run: cargo bake --locked releases:cargo:publish:pending --version "${{GITHUB_REF_NAME#v}}"
  publish:
    needs: check
    if: startsWith(github.ref, 'refs/tags/') && needs.check.outputs.has_packages == 'true'
    runs-on: ubuntu-latest
    timeout-minutes: 20
    environment: crates-io
    concurrency: publish-${{{{ github.ref }}}}
    permissions:
      contents: read
      id-token: write
    steps:
      - uses: actions/checkout@v7
      - uses: dtolnay/rust-toolchain@stable
      - name: Install Bake launcher
        run: cargo install socketry-cargo-bake --locked
      - uses: rust-lang/crates-io-auth-action@v1
        id: auth
      - name: Publish remaining workspace packages
        env:
          CARGO_REGISTRY_TOKEN: ${{{{ steps.auth.outputs.token }}}}
          BAKE_VERSION: ${{{{ needs.check.outputs.version }}}}
        run: cargo bake --locked releases:cargo:publish:workspace --version "$BAKE_VERSION"
"#
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Environment, Project};
    use std::fs;

    fn workspace(project: &Project) {
        project.write(
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\", \"crates/private\"]\nresolver = \"3\"\n",
        );
        for (name, publish) in [
            ("alpha", "publish = true\n"),
            ("beta", "publish = [\"crates-io\"]\n"),
            ("private", "publish = false\n"),
        ] {
            project.write(
                format!("crates/{name}/Cargo.toml"),
                &format!(
                    "[package]\nname = \"{name}\"\nversion = \"1.2.3\"\nedition = \"2024\"\n{publish}"
                ),
            );
            project.write(format!("crates/{name}/src/lib.rs"), "// fixture\n");
        }
    }

    #[test]
    fn discovers_and_sorts_publishable_workspace_packages() {
        let project = Project::new();
        let mut environment = Environment::new();
        workspace(&project);
        project.cargo_proxy(&mut environment, None);

        let packages = workspace_packages(&project.context()).unwrap();

        assert_eq!(
            packages
                .iter()
                .map(|package| package.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        assert!(
            packages
                .iter()
                .all(|package| package.manifest_path.contains("crates"))
        );
    }

    #[test]
    fn reports_cargo_metadata_failures_and_invalid_json() {
        let project = Project::new();
        let mut environment = Environment::new();
        let cargo = project.executable(
            "cargo-failure",
            "#!/bin/sh\necho metadata failure >&2\nexit 8\n",
        );
        environment.set("CARGO", cargo.as_os_str());
        let error = workspace_packages(&project.context()).unwrap_err();
        assert!(error.to_string().contains("metadata failure"));

        let cargo = project.executable("cargo-invalid-json", "#!/bin/sh\necho invalid\n");
        environment.set("CARGO", cargo.as_os_str());
        assert!(
            workspace_packages(&project.context())
                .unwrap_err()
                .to_string()
                .contains("could not parse Cargo metadata")
        );

        let cargo = project.executable(
            "cargo-missing-packages",
            "#!/bin/sh\necho '{\"workspace_root\": \"/tmp\"}'\n",
        );
        environment.set("CARGO", cargo.as_os_str());
        assert!(
            workspace_packages(&project.context())
                .unwrap_err()
                .to_string()
                .contains("packages array")
        );
        assert!(package_by_name(&project.context(), "fixture").is_err());
    }

    #[test]
    fn reports_missing_fields_for_publishable_packages() {
        let project = Project::new();
        let mut environment = Environment::new();
        let cargo = project.executable(
            "cargo-missing-name",
            "#!/bin/sh\necho '{\"packages\":[{}]}'\n",
        );
        environment.set("CARGO", cargo.as_os_str());
        assert!(
            workspace_packages(&project.context())
                .unwrap_err()
                .to_string()
                .contains("package has no name")
        );

        let cargo = project.executable(
            "cargo-missing-version",
            "#!/bin/sh\necho '{\"packages\":[{\"name\":\"fixture\"}]}'\n",
        );
        environment.set("CARGO", cargo.as_os_str());
        assert!(
            workspace_packages(&project.context())
                .unwrap_err()
                .to_string()
                .contains("has no version")
        );

        let cargo = project.executable(
            "cargo-missing-path",
            "#!/bin/sh\necho '{\"packages\":[{\"name\":\"fixture\",\"version\":\"1.2.3\"}]}'\n",
        );
        environment.set("CARGO", cargo.as_os_str());
        assert!(
            workspace_packages(&project.context())
                .unwrap_err()
                .to_string()
                .contains("has no manifest path")
        );
    }

    #[test]
    fn handles_cargo_publish_metadata_variants() {
        let project = Project::new();
        let mut environment = Environment::new();
        let metadata = serde_json::json!({
            "packages": [
                {"publish": false},
                {"name":"private-registry","version":"1.2.3","manifest_path":"private/Cargo.toml","publish":["private"]},
                {"name":"crates-io","version":"1.2.3","manifest_path":"crates/Cargo.toml","publish":["crates-io"]},
                {"name":"unusual","version":"1.2.3","manifest_path":"unusual/Cargo.toml","publish":"unexpected"}
            ]
        });
        let metadata_path = project.write("metadata.json", &metadata.to_string());
        let cargo =
            project.executable("cargo-metadata", "#!/bin/sh\ncat \"$BAKE_TEST_METADATA\"\n");
        environment.set("CARGO", cargo.as_os_str());
        environment.set("BAKE_TEST_METADATA", metadata_path.as_os_str());

        let packages = workspace_packages(&project.context()).unwrap();

        assert_eq!(
            packages
                .iter()
                .map(|package| package.name.as_str())
                .collect::<Vec<_>>(),
            ["crates-io", "unusual"]
        );
    }

    #[test]
    fn finds_packages_and_runs_cargo_commands() {
        let project = Project::new();
        let mut environment = Environment::new();
        workspace(&project);
        project.cargo_proxy(&mut environment, None);
        let context = project.context();

        assert_eq!(package_by_name(&context, "beta").unwrap().version, "1.2.3");
        assert!(
            package_by_name(&context, "missing")
                .unwrap_err()
                .to_string()
                .contains("was not found")
        );
        run_cargo(&context, ["fmt", "--check"]).unwrap();
        run_cargo_arguments(&context, &["test".to_owned(), "--workspace".to_owned()]).unwrap();
        assert!(project.cargo_arguments().contains("fmt --check"));
        assert!(project.cargo_arguments().contains("test --workspace"));
    }

    #[test]
    fn uses_the_cargo_path_fallback_when_the_environment_variable_is_unset() {
        let project = Project::new();
        let mut environment = Environment::new();
        let real_cargo = std::path::PathBuf::from(environment.original("CARGO").unwrap());
        let proxy = project.executable(
            "cargo",
            &format!(
                "#!/bin/sh\nif [ \"$1\" = metadata ]; then exec {} \"$@\"; fi\nexit 0\n",
                crate::test_support::shell_quote(&real_cargo)
            ),
        );
        environment.remove("CARGO");
        environment.prepend_path(&proxy.parent().unwrap().to_path_buf());
        project.single_package("fixture", "1.2.3");

        let context = project.context();
        assert_eq!(workspace_packages(&context).unwrap()[0].name, "fixture");
        run_cargo(&context, ["check"]).unwrap();
        run_cargo_arguments(&context, &["check".to_owned()]).unwrap();
    }

    #[test]
    fn reports_cargo_commands_that_cannot_be_started() {
        let project = Project::new();
        let mut environment = Environment::new();
        environment.set("CARGO", project.root().join("missing-cargo").as_os_str());
        let context = project.context();

        assert!(workspace_packages(&context).is_err());
        assert!(run_cargo(&context, ["check"]).is_err());
        assert!(run_cargo_arguments(&context, &["check".to_owned()]).is_err());
    }

    #[test]
    fn reports_failed_cargo_commands() {
        let project = Project::new();
        let mut environment = Environment::new();
        let cargo = project.executable(
            "cargo-failure",
            "#!/bin/sh\necho fake cargo failure >&2\nexit 7\n",
        );
        environment.set("CARGO", cargo.as_os_str());
        let context = project.context();

        assert!(
            run_cargo(&context, ["publish", "--locked"])
                .unwrap_err()
                .to_string()
                .contains("cargo publish --locked failed")
        );
        assert!(
            run_cargo_arguments(&context, &["publish".to_owned()])
                .unwrap_err()
                .to_string()
                .contains("cargo publish failed")
        );
    }

    #[test]
    fn reports_pending_workspace_packages_and_github_outputs() {
        let project = Project::new();
        let mut environment = Environment::new();
        workspace(&project);
        project.cargo_proxy(&mut environment, None);
        let output = project.root().join("github-output");
        environment.set("GITHUB_OUTPUT", output.as_os_str());
        let (api, server) = crate::test_support::http_server(vec![
            (200, r#"{"versions":[{"num":"1.2.3"}]}"#.to_owned()),
            (404, r#"{}"#.to_owned()),
        ]);
        environment.set("BAKE_TEST_CRATES_IO_API", api);

        let (pending, published) = publication_state(&project.context(), "1.2.3").unwrap();

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].name, "beta");
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].name, "alpha");
        append_github_output("version", "1.2.3").unwrap();
        append_github_output("has_packages", "true").unwrap();
        assert_eq!(
            fs::read_to_string(output).unwrap(),
            "version=1.2.3\nhas_packages=true\n"
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /crates/alpha/versions "));
        assert!(requests[1].starts_with("GET /crates/beta/versions "));
    }

    #[test]
    fn skips_registry_lookups_for_a_mismatched_release_version() {
        let project = Project::new();
        let mut environment = Environment::new();
        workspace(&project);
        project.cargo_proxy(&mut environment, None);

        let error = publication_state(&project.context(), "1.2.4").unwrap_err();

        assert!(
            error
                .to_string()
                .contains("does not match the workspace version")
        );
    }

    #[test]
    fn reports_publication_metadata_and_registry_errors() {
        let project = Project::new();
        let mut environment = Environment::new();
        let cargo = project.executable("cargo-empty", "#!/bin/sh\necho '{\"packages\":[]}'\n");
        environment.set("CARGO", cargo.as_os_str());
        assert!(publication_state(&project.context(), "1.2.3").is_err());

        let metadata = serde_json::json!({
            "packages": [{
                "name": "fixture",
                "version": "1.2.3",
                "manifest_path": project.root().join("Cargo.toml"),
            }]
        });
        let metadata_path = project.write("metadata.json", &metadata.to_string());
        let marker = project.root().join("metadata-called");
        let cargo = project.executable(
            "cargo-second-metadata-failure",
            "#!/bin/sh\nif [ -f \"$BAKE_TEST_METADATA_MARKER\" ]; then echo second metadata failed >&2; exit 2; fi\ntouch \"$BAKE_TEST_METADATA_MARKER\"\ncat \"$BAKE_TEST_METADATA\"\n",
        );
        environment.set("CARGO", cargo.as_os_str());
        environment.set("BAKE_TEST_METADATA", metadata_path.as_os_str());
        environment.set("BAKE_TEST_METADATA_MARKER", marker.as_os_str());
        assert!(publication_state(&project.context(), "1.2.3").is_err());

        let cargo =
            project.executable("cargo-metadata", "#!/bin/sh\ncat \"$BAKE_TEST_METADATA\"\n");
        environment.set("CARGO", cargo.as_os_str());
        let (api, server) =
            crate::test_support::http_server(vec![(500, "registry unavailable".to_owned())]);
        environment.set("BAKE_TEST_CRATES_IO_API", &api);
        assert!(publication_state(&project.context(), "1.2.3").is_err());
        server.join().unwrap();
    }

    #[test]
    fn appends_github_outputs_only_when_configured() {
        let mut environment = Environment::new();
        environment.remove("GITHUB_OUTPUT");

        append_github_output("has_packages", "false").unwrap();
    }

    #[test]
    fn reports_github_output_file_errors() {
        let project = Project::new();
        let mut environment = Environment::new();
        environment.set(
            "GITHUB_OUTPUT",
            project.root().join("missing/output").as_os_str(),
        );

        assert!(append_github_output("version", "1.2.3").is_err());

        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("write failed"))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        assert!(write_github_output(&mut FailingWriter, "version", "1.2.3").is_err());
        let mut writer = FailingWriter;
        assert!(writer.flush().is_ok());

        environment.set("BAKE_TEST_GITHUB_OUTPUT_FAILURE", "version");
        environment.set(
            "GITHUB_OUTPUT",
            project.root().join("injected-output").as_os_str(),
        );
        assert!(append_github_output("version", "1.2.3").is_err());
    }

    #[test]
    fn generates_a_bake_based_tag_publishing_workflow() {
        let packages = vec![
            WorkspacePackage {
                name: "fixture-one".to_owned(),
                version: "1.2.3".to_owned(),
                manifest_path: "Cargo.toml".to_owned(),
            },
            WorkspacePackage {
                name: "fixture-two".to_owned(),
                version: "1.2.3".to_owned(),
                manifest_path: "Cargo.toml".to_owned(),
            },
        ];
        let workflow = publish_workflow(&packages, "main").unwrap();

        assert!(workflow.contains("releases:cargo:publish:pending"));
        assert!(workflow.contains("releases:cargo:publish:workspace"));
        assert!(workflow.contains("crates-io-auth-action"));
        assert!(!workflow.contains("python"));
        assert!(workflow.contains("pull_request:"));
    }

    #[test]
    fn rejects_empty_or_mismatched_package_sets_for_workflow_generation() {
        assert!(
            publish_workflow(&[], "main")
                .unwrap_err()
                .to_string()
                .contains("no publishable packages")
        );
        let packages = vec![
            WorkspacePackage {
                name: "one".to_owned(),
                version: "1.0.0".to_owned(),
                manifest_path: "one/Cargo.toml".to_owned(),
            },
            WorkspacePackage {
                name: "two".to_owned(),
                version: "1.0.1".to_owned(),
                manifest_path: "two/Cargo.toml".to_owned(),
            },
        ];
        assert!(
            publish_workflow(&packages, "main")
                .unwrap_err()
                .to_string()
                .contains("share one version")
        );
    }
}
