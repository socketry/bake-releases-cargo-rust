use bake::{Context, Error, Result};
use serde::Serialize;
use serde_json::Value;

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
    let names = serde_json::to_string(
        &packages
            .iter()
            .map(|package| package.name.as_str())
            .collect::<Vec<_>>(),
    )?;

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
    steps:
      - uses: actions/checkout@v7
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt
      - name: Check formatting
        run: cargo fmt --all -- --check
      - name: Run Clippy
        run: cargo clippy --workspace --all-targets --locked -- -D warnings
      - name: Run tests
        run: cargo test --workspace --locked
  publish:
    needs: check
    if: startsWith(github.ref, 'refs/tags/')
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
      - name: Validate workspace version from release tag
        id: packages
        shell: bash
        env:
          BAKE_PACKAGES: '{names}'
        run: |
          cargo metadata --format-version 1 --locked > "$RUNNER_TEMP/bake-metadata.json"
          python3 - <<'PY'
          import json
          import os
          from pathlib import Path
          from urllib.error import HTTPError, URLError
          from urllib.parse import quote
          from urllib.request import Request, urlopen

          tag = os.environ["GITHUB_REF_NAME"]
          if not tag.startswith("v"):
              raise SystemExit(f"Unsupported release tag: {{tag}}")
          version = tag[1:]
          metadata = json.loads((Path(os.environ["RUNNER_TEMP"]) / "bake-metadata.json").read_text())
          names = json.loads(os.environ["BAKE_PACKAGES"])
          workspace_members = set(metadata["workspace_members"])
          by_name = {{package["name"]: package for package in metadata["packages"] if package["id"] in workspace_members}}
          mismatched = [name for name in names if name not in by_name or by_name[name]["version"] != version]
          if mismatched:
              raise SystemExit(f"Tag version {{version}} does not match workspace packages: {{', '.join(mismatched)}}")
          existing = []
          for name in names:
              url = f"https://crates.io/api/v1/crates/{{quote(name, safe='')}}/versions"
              request = Request(url, headers={{"User-Agent": "socketry-bake-publish-workflow"}})
              try:
                  with urlopen(request, timeout=30) as response:
                      registry = json.load(response)
              except HTTPError as error:
                  if error.code == 404:
                      continue
                  raise SystemExit(f"Could not check crates.io for {{name}}: HTTP {{error.code}}")
              except (URLError, TimeoutError, json.JSONDecodeError) as error:
                  raise SystemExit(f"Could not check crates.io for {{name}}: {{error}}")
              versions = registry.get("versions")
              if not isinstance(versions, list):
                  raise SystemExit(f"Unexpected crates.io response while checking {{name}}")
              if any(item.get("num") == version for item in versions if isinstance(item, dict)):
                  existing.append(name)
          missing = [name for name in names if name not in existing]
          with open(os.environ["GITHUB_OUTPUT"], "a") as output:
              output.write(f"exclude={{json.dumps(existing)}}\n")
              output.write(f"has_packages={{'true' if missing else 'false'}}\n")
          PY
      - uses: rust-lang/crates-io-auth-action@v1
        id: auth
        if: steps.packages.outputs.has_packages == 'true'
      - name: Publish workspace packages
        if: steps.packages.outputs.has_packages == 'true'
        env:
          BAKE_EXCLUDE: ${{{{ steps.packages.outputs.exclude }}}}
          CARGO_REGISTRY_TOKEN: ${{{{ steps.auth.outputs.token }}}}
        run: |
          python3 - <<'PY'
          import json
          import os
          import subprocess

          arguments = ["cargo", "publish", "--workspace", "--locked"]
          for package in json.loads(os.environ["BAKE_EXCLUDE"]):
              arguments.extend(["--exclude", package])
          subprocess.run(arguments, check=True)
          PY
"#
    ))
}
