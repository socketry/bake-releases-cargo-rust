// Released under the MIT License.
// Copyright, 2026, by Samuel Williams.

use bake::{Context, Error, Result, Value};
use serde_json::json;
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use toml_edit::{DocumentMut, Item, Table, Value as TomlValue, value};

use crate::cargo::{WorkspacePackage, workspace_packages};

#[derive(Clone, Copy)]
pub(crate) enum Component {
    Major,
    Minor,
    Patch,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    fn parse(value: &str) -> Result<Self> {
        let components: Vec<_> = value.split('.').collect();
        if components.len() != 3 {
            return Err(Error::new(format!(
                "version {value:?} must use the stable MAJOR.MINOR.PATCH form"
            )));
        }

        let mut numbers = [0; 3];
        for (index, component) in components.into_iter().enumerate() {
            if component.is_empty()
                || component.len() > 1 && component.starts_with('0')
                || !component.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(Error::new(format!(
                    "version {value:?} must use the stable MAJOR.MINOR.PATCH form"
                )));
            }
            numbers[index] = component
                .parse()
                .map_err(|_| Error::new(format!("version component in {value:?} is too large")))?;
        }

        Ok(Self {
            major: numbers[0],
            minor: numbers[1],
            patch: numbers[2],
        })
    }

    fn increment(self, component: Component) -> Result<Self> {
        match component {
            Component::Major => Ok(Self {
                major: self.major.checked_add(1).ok_or_else(version_overflow)?,
                minor: 0,
                patch: 0,
            }),
            Component::Minor => Ok(Self {
                major: self.major,
                minor: self.minor.checked_add(1).ok_or_else(version_overflow)?,
                patch: 0,
            }),
            Component::Patch => Ok(Self {
                major: self.major,
                minor: self.minor,
                patch: self.patch.checked_add(1).ok_or_else(version_overflow)?,
            }),
        }
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

fn version_overflow() -> Error {
    Error::new("version component is too large to increment")
}

pub(crate) fn workspace_version(context: &Context) -> Result<String> {
    let packages = workspace_packages(context)?;
    let version = shared_version(&packages)?;
    Ok(version.to_string())
}

pub(crate) fn increment(context: &Context, component: Component) -> Result<Value> {
    let packages = workspace_packages(context)?;
    let current = shared_version(&packages)?;
    let next = current.increment(component)?;
    set_workspace_version(context, &packages, current, next)
}

pub(crate) fn set(context: &Context, target: &str) -> Result<Value> {
    let target = Version::parse(target)?;
    let packages = workspace_packages(context)?;
    let current = shared_version(&packages)?;
    if target <= current {
        return Err(Error::new(format!(
            "new version {target} must be greater than the current workspace version {current}"
        )));
    }
    set_workspace_version(context, &packages, current, target)
}

fn shared_version(packages: &[WorkspacePackage]) -> Result<Version> {
    let first = packages
        .first()
        .ok_or_else(|| Error::new("the workspace has no publishable packages"))?;
    let version = Version::parse(&first.version)?;
    for package in &packages[1..] {
        let package_version = Version::parse(&package.version)?;
        if package_version != version {
            return Err(Error::new(format!(
                "publishable workspace packages must share one version; {} is {}, while {} is {}",
                first.name, first.version, package.name, package.version
            )));
        }
    }
    Ok(version)
}

fn set_workspace_version(
    context: &Context,
    packages: &[WorkspacePackage],
    current: Version,
    target: Version,
) -> Result<Value> {
    if target <= current {
        return Err(Error::new(format!(
            "new version {target} must be greater than the current workspace version {current}"
        )));
    }

    let package_names: HashSet<_> = packages
        .iter()
        .map(|package| package.name.as_str())
        .collect();
    let (workspace_manifest, manifests) = workspace_manifests(context)?;
    let publishable_manifests: HashSet<_> = packages
        .iter()
        .map(|package| PathBuf::from(&package.manifest_path))
        .collect();

    let mut updated = Vec::new();
    for path in manifests {
        let source = with_io_failure("manifest_read", || fs::read_to_string(&path))
            .map_err(|error| Error::new(format!("{}: {error}", path.display())))?;
        let mut document = parse_manifest(&path, &source)?;
        if publishable_manifests.contains(&path) {
            update_package_version(&mut document, current, target)?;
        }
        if path == workspace_manifest {
            update_workspace_version(&mut document, current, target)?;
        }
        update_local_dependency_versions(&mut document, target, &package_names);
        updated.push((path, document.to_string()));
    }

    for (path, contents) in &updated {
        replace_file(path, contents)?;
    }

    crate::cargo::run_cargo(context, ["update", "--workspace"]).map_err(|error| {
        Error::new(format!(
            "updated Cargo versions, but could not update Cargo.lock: {error}"
        ))
    })?;

    Ok(json!({
        "previous_version": current.to_string(),
        "version": target.to_string(),
        "packages": packages.iter().map(|package| package.name.as_str()).collect::<Vec<_>>(),
        "manifests_updated": updated.iter().map(|(path, _)| path.display().to_string()).collect::<Vec<_>>(),
    }))
}

fn parse_manifest(path: &Path, source: &str) -> Result<DocumentMut> {
    source
        .parse()
        .map_err(|error| Error::new(format!("{}: {error}", path.display())))
}

fn workspace_manifests(context: &Context) -> Result<(PathBuf, BTreeSet<PathBuf>)> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = context
        .command(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()?;
    if !output.status.success() {
        return Err(Error::new(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| Error::new(format!("could not parse Cargo metadata: {error}")))?;
    let root = metadata
        .get("workspace_root")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Error::new("Cargo metadata did not contain a workspace root"))?;
    let workspace_manifest = Path::new(root).join("Cargo.toml");
    let packages = metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| Error::new("Cargo metadata did not contain a packages array"))?;
    let mut manifests = BTreeSet::from([workspace_manifest.clone()]);
    for package in packages {
        let manifest = package
            .get("manifest_path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Error::new("Cargo metadata package has no manifest path"))?;
        manifests.insert(PathBuf::from(manifest));
    }
    Ok((workspace_manifest, manifests))
}

fn update_package_version(
    document: &mut DocumentMut,
    current: Version,
    target: Version,
) -> Result<()> {
    let Some(package) = document.get_mut("package").and_then(Item::as_table_mut) else {
        return Ok(());
    };
    let Some(version) = package.get("version") else {
        return Ok(());
    };
    if version.as_str() == Some(&current.to_string()) {
        document["package"]["version"] = value(target.to_string());
    } else if version
        .as_table_like()
        .and_then(|table| table.get("workspace"))
        .and_then(Item::as_bool)
        == Some(true)
    {
        // This package inherits the version from [workspace.package].
    } else {
        return Err(Error::new(format!(
            "package version is not the expected workspace version {current}"
        )));
    }
    Ok(())
}

fn update_workspace_version(
    document: &mut DocumentMut,
    current: Version,
    target: Version,
) -> Result<()> {
    let Some(version) = document
        .get("workspace")
        .and_then(Item::as_table)
        .and_then(|workspace| workspace.get("package"))
        .and_then(Item::as_table)
        .and_then(|package| package.get("version"))
    else {
        return Ok(());
    };
    if version.as_str() != Some(&current.to_string()) {
        return Err(Error::new(format!(
            "[workspace.package].version is not the expected workspace version {current}"
        )));
    }
    document["workspace"]["package"]["version"] = value(target.to_string());
    Ok(())
}

fn update_local_dependency_versions(
    document: &mut DocumentMut,
    target: Version,
    package_names: &HashSet<&str>,
) {
    update_dependency_groups(document.as_table_mut(), target, package_names);

    if let Some(workspace) = document.get_mut("workspace").and_then(Item::as_table_mut) {
        update_dependency_groups(workspace, target, package_names);
    }

    if let Some(targets) = document.get_mut("target").and_then(Item::as_table_mut) {
        for (_, configuration) in targets.iter_mut() {
            if let Some(configuration) = configuration.as_table_mut() {
                update_dependency_groups(configuration, target, package_names);
            }
        }
    }
}

fn update_dependency_groups(table: &mut Table, target: Version, package_names: &HashSet<&str>) {
    for group_name in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(group) = table.get_mut(group_name).and_then(Item::as_table_mut) {
            for (name, specification) in group.iter_mut() {
                let name = name.get();
                if let Some(specification) = specification.as_table_mut() {
                    update_table_dependency(specification, name, target, package_names);
                } else if let Some(specification) = specification
                    .as_value_mut()
                    .and_then(TomlValue::as_inline_table_mut)
                {
                    update_inline_dependency(specification, name, target, package_names);
                }
            }
        }
    }
}

fn update_table_dependency(
    dependency: &mut Table,
    name: &str,
    target: Version,
    package_names: &HashSet<&str>,
) {
    let package_name = dependency
        .get("package")
        .and_then(Item::as_str)
        .unwrap_or(name);
    if package_names.contains(package_name) && dependency.get("version").is_some() {
        dependency["version"] = value(target.to_string());
    }
}

fn update_inline_dependency(
    dependency: &mut toml_edit::InlineTable,
    name: &str,
    target: Version,
    package_names: &HashSet<&str>,
) {
    let package_name = dependency
        .get("package")
        .and_then(TomlValue::as_str)
        .unwrap_or(name);
    if package_names.contains(package_name) && dependency.get("version").is_some() {
        dependency.insert("version", TomlValue::from(target.to_string()));
    }
}

fn replace_file(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::new(format!("{} has no parent directory", path.display())))?;
    let permissions = with_io_failure("metadata", || fs::metadata(path))?.permissions();
    let mut temporary = with_io_failure("create_temp", || NamedTempFile::new_in(parent))?;
    with_io_failure("write_temp", || temporary.write_all(contents.as_bytes()))?;
    with_io_failure("set_permissions", || {
        temporary.as_file().set_permissions(permissions)
    })?;
    with_io_failure("sync", || temporary.as_file().sync_all())?;
    with_io_failure("persist", || {
        temporary
            .persist(path)
            .map(|_| ())
            .map_err(|error| error.error)
    })?;
    Ok(())
}

#[cfg(test)]
fn with_io_failure<T>(operation: &str, action: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    if std::env::var("BAKE_TEST_VERSION_IO_FAILURE").as_deref() == Ok(operation) {
        Err(io::Error::other(format!("injected {operation} failure")))
    } else {
        action()
    }
}

#[cfg(not(test))]
fn with_io_failure<T>(_: &str, action: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    action()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Environment, Project};

    fn metadata_proxy(
        project: &Project,
        environment: &mut Environment,
        manifest_path: &Path,
        version: &str,
    ) {
        let metadata = serde_json::json!({
            "workspace_root": project.root(),
            "packages": [{
                "name": "fixture",
                "version": version,
                "manifest_path": manifest_path,
            }]
        });
        let metadata_path = project.write("metadata.json", &metadata.to_string());
        let cargo = project.executable(
            "cargo-metadata",
            "#!/bin/sh\nif [ \"$1\" = metadata ]; then cat \"$BAKE_TEST_METADATA\"; fi\nexit 0\n",
        );
        environment.set("CARGO", cargo.as_os_str());
        environment.set("BAKE_TEST_METADATA", metadata_path.as_os_str());
    }

    fn empty_metadata_proxy(project: &Project, environment: &mut Environment) {
        let cargo = project.executable(
            "cargo-empty-metadata",
            "#!/bin/sh\necho '{\"packages\":[]}'\n",
        );
        environment.set("CARGO", cargo.as_os_str());
    }

    fn package(name: &str, version: &str) -> WorkspacePackage {
        WorkspacePackage {
            name: name.to_owned(),
            version: version.to_owned(),
            manifest_path: format!("{name}/Cargo.toml"),
        }
    }

    #[test]
    fn parses_displays_and_increments_stable_versions() {
        let version = Version::parse("1.2.3").unwrap();

        assert_eq!(version.to_string(), "1.2.3");
        assert_eq!(
            version.increment(Component::Major).unwrap().to_string(),
            "2.0.0"
        );
        assert_eq!(
            version.increment(Component::Minor).unwrap().to_string(),
            "1.3.0"
        );
        assert_eq!(
            version.increment(Component::Patch).unwrap().to_string(),
            "1.2.4"
        );
    }

    #[test]
    fn rejects_non_stable_and_overflowing_versions() {
        for version in [
            "1", "1.2", "1.2.3.4", "", "1..3", "01.2.3", "1.02.3", "1.2.03", "1.a.3", "-1.2.3",
        ] {
            assert!(
                Version::parse(version)
                    .unwrap_err()
                    .to_string()
                    .contains("stable MAJOR.MINOR.PATCH")
            );
        }
        assert!(
            Version::parse("18446744073709551616.0.0")
                .unwrap_err()
                .to_string()
                .contains("too large")
        );
    }

    #[test]
    fn reports_overflow_for_each_version_component() {
        let version = Version {
            major: u64::MAX,
            minor: u64::MAX,
            patch: u64::MAX,
        };
        for component in [Component::Major, Component::Minor, Component::Patch] {
            assert!(
                version
                    .increment(component)
                    .unwrap_err()
                    .to_string()
                    .contains("too large to increment")
            );
        }
    }

    #[test]
    fn requires_a_shared_version_for_publishable_packages() {
        assert!(
            shared_version(&[])
                .unwrap_err()
                .to_string()
                .contains("no publishable packages")
        );
        assert_eq!(
            shared_version(&[package("one", "1.2.3")])
                .unwrap()
                .to_string(),
            "1.2.3"
        );
        assert_eq!(
            shared_version(&[package("one", "1.2.3"), package("two", "1.2.3")])
                .unwrap()
                .to_string(),
            "1.2.3"
        );
        assert!(
            shared_version(&[package("one", "1.2.3"), package("two", "1.2.4")])
                .unwrap_err()
                .to_string()
                .contains("must share one version")
        );
        assert!(shared_version(&[package("one", "1.2")]).is_err());
        assert!(shared_version(&[package("one", "1.2.3"), package("two", "invalid")]).is_err());
    }

    #[test]
    fn updates_package_versions_and_keeps_inherited_versions() {
        let mut document = "[workspace]\n".parse::<DocumentMut>().unwrap();
        update_package_version(
            &mut document,
            Version::parse("1.2.3").unwrap(),
            Version::parse("1.2.4").unwrap(),
        )
        .unwrap();

        let mut document = "[package]\nname = \"fixture\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        update_package_version(
            &mut document,
            Version::parse("1.2.3").unwrap(),
            Version::parse("1.2.4").unwrap(),
        )
        .unwrap();

        let mut document = "[package]\nversion = \"1.2.3\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        update_package_version(
            &mut document,
            Version::parse("1.2.3").unwrap(),
            Version::parse("1.2.4").unwrap(),
        )
        .unwrap();
        assert_eq!(document["package"]["version"].as_str(), Some("1.2.4"));

        let mut document = "[package]\nversion = { workspace = true }\n"
            .parse::<DocumentMut>()
            .unwrap();
        update_package_version(
            &mut document,
            Version::parse("1.2.3").unwrap(),
            Version::parse("1.2.4").unwrap(),
        )
        .unwrap();
        assert_eq!(
            document["package"]["version"]["workspace"].as_bool(),
            Some(true)
        );

        let mut document = "[package]\nversion = \"1.2.2\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        assert!(
            update_package_version(
                &mut document,
                Version::parse("1.2.3").unwrap(),
                Version::parse("1.2.4").unwrap(),
            )
            .unwrap_err()
            .to_string()
            .contains("not the expected workspace version")
        );
    }

    #[test]
    fn updates_workspace_versions_and_rejects_inconsistent_manifests() {
        let mut document = "[package]\nname = \"fixture\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        update_workspace_version(
            &mut document,
            Version::parse("1.2.3").unwrap(),
            Version::parse("1.2.4").unwrap(),
        )
        .unwrap();

        let mut document = "[workspace.package]\nversion = \"1.2.3\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        update_workspace_version(
            &mut document,
            Version::parse("1.2.3").unwrap(),
            Version::parse("1.2.4").unwrap(),
        )
        .unwrap();
        assert_eq!(
            document["workspace"]["package"]["version"].as_str(),
            Some("1.2.4")
        );

        let mut document = "[workspace.package]\nversion = \"1.2.2\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        assert!(
            update_workspace_version(
                &mut document,
                Version::parse("1.2.3").unwrap(),
                Version::parse("1.2.4").unwrap(),
            )
            .unwrap_err()
            .to_string()
            .contains("not the expected workspace version")
        );
    }

    #[test]
    fn updates_local_dependency_versions_in_manifest_tables() {
        let mut document = r#"
[dependencies]
fixture = { path = "../fixture", version = "1.2.3" }
alias = { package = "fixture", path = "../fixture", version = "1.2.3" }
other = { path = "../other", version = "1.2.3" }
without_version = { path = "../fixture" }
scalar = "1.2.3"

[dependencies.table_alias]
package = "fixture"
path = "../fixture"
version = "1.2.3"

[dependencies.other_table]
package = "other"
path = "../other"
version = "1.2.3"

[dependencies.no_version_table]
package = "fixture"
path = "../fixture"

[workspace.dependencies]
fixture = { path = "crates/fixture", version = "1.2.3" }

[dev-dependencies]
fixture = { path = "../fixture", version = "1.2.3" }

[build-dependencies]
fixture = { path = "../fixture", version = "1.2.3" }

[target]
scalar = "not a target table"

[target.'cfg(unix)'.build-dependencies]
fixture = { path = "../fixture", version = "1.2.3" }

[target.unused]
value = "not a dependency group"
"#
        .parse::<DocumentMut>()
        .unwrap();
        let names = HashSet::from(["fixture"]);

        update_local_dependency_versions(&mut document, Version::parse("1.2.4").unwrap(), &names);

        assert_eq!(
            document["dependencies"]["fixture"]["version"].as_str(),
            Some("1.2.4")
        );
        assert_eq!(
            document["dependencies"]["alias"]["version"].as_str(),
            Some("1.2.4")
        );
        assert_eq!(
            document["dependencies"]["table_alias"]["version"].as_str(),
            Some("1.2.4")
        );
        assert_eq!(
            document["dependencies"]["other"]["version"].as_str(),
            Some("1.2.3")
        );
        assert!(
            document["dependencies"]["without_version"]
                .get("version")
                .is_none()
        );
        assert_eq!(
            document["workspace"]["dependencies"]["fixture"]["version"].as_str(),
            Some("1.2.4")
        );
        assert_eq!(
            document["target"]["cfg(unix)"]["build-dependencies"]["fixture"]["version"].as_str(),
            Some("1.2.4")
        );
    }

    #[test]
    fn updates_a_workspace_manifest_and_refreshes_its_lockfile() {
        let project = Project::new();
        let mut environment = Environment::new();
        project.single_package("fixture", "1.2.3");
        project.cargo_proxy(&mut environment, None);

        let result = set(&project.context(), "1.2.4").unwrap();

        assert_eq!(result["previous_version"], "1.2.3");
        assert_eq!(result["version"], "1.2.4");
        assert!(
            std::fs::read_to_string(project.root().join("Cargo.toml"))
                .unwrap()
                .contains("version = \"1.2.4\"")
        );
        assert!(project.cargo_arguments().contains("update --workspace"));
    }

    #[test]
    fn reports_empty_workspace_and_version_increment_errors() {
        let project = Project::new();
        let mut environment = Environment::new();
        empty_metadata_proxy(&project, &mut environment);

        assert!(workspace_version(&project.context()).is_err());
        assert!(increment(&project.context(), Component::Patch).is_err());
        assert!(set(&project.context(), "1.2.4").is_err());

        environment.set("CARGO", project.root().join("missing-cargo").as_os_str());
        assert!(workspace_version(&project.context()).is_err());
        assert!(set(&project.context(), "1.2.4").is_err());
    }

    #[test]
    fn reports_increment_overflow_from_workspace_metadata() {
        let project = Project::new();
        let mut environment = Environment::new();
        let manifest = project.write(
            "Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"1.2.3\"\nedition = \"2024\"\n",
        );
        metadata_proxy(
            &project,
            &mut environment,
            &manifest,
            "18446744073709551615.0.0",
        );

        assert!(increment(&project.context(), Component::Major).is_err());
    }

    #[test]
    fn updates_workspace_and_inherited_package_versions() {
        let project = Project::new();
        let mut environment = Environment::new();
        project.write(
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/fixture\"]\nresolver = \"3\"\n\
             [workspace.package]\nversion = \"1.2.3\"\n\
             [workspace.dependencies]\nfixture = { path = \"crates/fixture\", version = \"1.2.3\" }\n",
        );
        let manifest = project.write(
            "crates/fixture/Cargo.toml",
            "[package]\nname = \"fixture\"\nversion.workspace = true\nedition = \"2024\"\n",
        );
        project.write("crates/fixture/src/lib.rs", "// fixture\n");
        metadata_proxy(&project, &mut environment, &manifest, "1.2.3");

        let result = set(&project.context(), "1.2.4").unwrap();

        assert_eq!(result["version"], "1.2.4");
        let root = std::fs::read_to_string(project.root().join("Cargo.toml")).unwrap();
        let member = std::fs::read_to_string(manifest).unwrap();
        assert!(root.contains("version = \"1.2.4\""));
        assert!(root.contains("version = \"1.2.4\" }"));
        assert!(member.contains("version.workspace = true"));
    }

    #[test]
    fn reports_manifest_read_and_parse_errors() {
        let project = Project::new();
        let mut environment = Environment::new();
        project.write("Cargo.toml", "[workspace]\n");
        let missing = project.root().join("crates/fixture/Cargo.toml");
        metadata_proxy(&project, &mut environment, &missing, "1.2.3");

        assert!(
            set_workspace_version(
                &project.context(),
                &[package("fixture", "1.2.3")],
                Version::parse("1.2.3").unwrap(),
                Version::parse("1.2.4").unwrap(),
            )
            .unwrap_err()
            .to_string()
            .contains("Cargo.toml")
        );

        let invalid = project.write("crates/fixture/Cargo.toml", "not = [valid\n");
        metadata_proxy(&project, &mut environment, &invalid, "1.2.3");
        assert!(
            set_workspace_version(
                &project.context(),
                &[package("fixture", "1.2.3")],
                Version::parse("1.2.3").unwrap(),
                Version::parse("1.2.4").unwrap(),
            )
            .unwrap_err()
            .to_string()
            .contains("Cargo.toml")
        );

        environment.set("BAKE_TEST_VERSION_IO_FAILURE", "manifest_read");
        assert!(
            set_workspace_version(
                &project.context(),
                &[package("fixture", "1.2.3")],
                Version::parse("1.2.3").unwrap(),
                Version::parse("1.2.4").unwrap(),
            )
            .is_err()
        );
    }

    #[test]
    fn reports_workspace_version_and_lockfile_update_errors() {
        let current = Version::parse("1.2.3").unwrap();
        let target = Version::parse("1.2.4").unwrap();

        let project = Project::new();
        let mut environment = Environment::new();
        project.write("Cargo.toml", "[workspace.package]\nversion = \"9.9.9\"\n");
        let manifest = project.write(
            "crates/fixture/Cargo.toml",
            "[package]\nname = \"fixture\"\nversion.workspace = true\nedition = \"2024\"\n",
        );
        metadata_proxy(&project, &mut environment, &manifest, "1.2.3");
        let mut publishable = package("fixture", "1.2.3");
        publishable.manifest_path = manifest.display().to_string();
        assert!(
            set_workspace_version(&project.context(), &[publishable], current, target,).is_err()
        );

        drop(environment);
        let project = Project::new();
        let mut environment = Environment::new();
        project.write("Cargo.toml", "[workspace]\n");
        let manifest = project.write(
            "crates/fixture/Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"9.9.9\"\nedition = \"2024\"\n",
        );
        metadata_proxy(&project, &mut environment, &manifest, "1.2.3");
        let mut publishable = package("fixture", "1.2.3");
        publishable.manifest_path = manifest.display().to_string();
        assert!(
            set_workspace_version(&project.context(), &[publishable], current, target,).is_err()
        );

        drop(environment);
        let project = Project::new();
        let mut environment = Environment::new();
        project.single_package("fixture", "1.2.3");
        project.cargo_proxy(&mut environment, None);
        environment.set("BAKE_TEST_VERSION_IO_FAILURE", "metadata");
        assert!(set(&project.context(), "1.2.4").is_err());

        environment.remove("BAKE_TEST_VERSION_IO_FAILURE");
        project.cargo_proxy(&mut environment, Some("update"));
        assert!(
            set(&project.context(), "1.2.5")
                .unwrap_err()
                .to_string()
                .contains("could not update Cargo.lock")
        );

        drop(environment);
        let project = Project::new();
        let mut environment = Environment::new();
        let cargo = project.executable(
            "cargo-metadata-failure",
            "#!/bin/sh\necho metadata failed >&2\nexit 2\n",
        );
        environment.set("CARGO", cargo.as_os_str());
        assert!(
            set_workspace_version(
                &project.context(),
                &[package("fixture", "1.2.3")],
                current,
                target,
            )
            .is_err()
        );
    }

    #[test]
    fn uses_the_cargo_path_fallback_for_manifest_metadata() {
        let project = Project::new();
        let mut environment = Environment::new();
        project.write("Cargo.toml", "[workspace]\n");
        let metadata = serde_json::json!({"workspace_root": project.root(), "packages": []});
        let metadata_path = project.write("metadata.json", &metadata.to_string());
        project.executable("cargo", "#!/bin/sh\ncat \"$BAKE_TEST_METADATA\"\n");
        environment.set("BAKE_TEST_METADATA", metadata_path.as_os_str());
        environment.remove("CARGO");
        environment.prepend_path(&project.root().join("bin"));

        assert_eq!(
            workspace_manifests(&project.context()).unwrap().0,
            project.root().join("Cargo.toml")
        );

        environment.set("CARGO", project.root().join("missing-cargo").as_os_str());
        assert!(workspace_manifests(&project.context()).is_err());
    }

    #[test]
    fn increments_versions_and_rejects_nonincreasing_targets() {
        let project = Project::new();
        let mut environment = Environment::new();
        project.single_package("fixture", "1.2.3");
        project.cargo_proxy(&mut environment, None);

        increment(&project.context(), Component::Minor).unwrap();
        assert!(
            std::fs::read_to_string(project.root().join("Cargo.toml"))
                .unwrap()
                .contains("version = \"1.3.0\"")
        );
        assert!(
            set(&project.context(), "1.3.0")
                .unwrap_err()
                .to_string()
                .contains("must be greater")
        );
        assert!(
            set(&project.context(), "1.2.9")
                .unwrap_err()
                .to_string()
                .contains("must be greater")
        );
        assert!(
            set(&project.context(), "invalid")
                .unwrap_err()
                .to_string()
                .contains("stable MAJOR.MINOR.PATCH")
        );
    }

    #[test]
    fn reports_workspace_manifest_metadata_errors() {
        let project = Project::new();
        let mut environment = Environment::new();
        let cargo = project.executable(
            "cargo-failure",
            "#!/bin/sh\necho bad metadata >&2\nexit 2\n",
        );
        environment.set("CARGO", cargo.as_os_str());
        assert!(
            workspace_manifests(&project.context())
                .unwrap_err()
                .to_string()
                .contains("cargo metadata failed")
        );

        let cargo = project.executable("cargo-json", "#!/bin/sh\necho invalid\n");
        environment.set("CARGO", cargo.as_os_str());
        assert!(
            workspace_manifests(&project.context())
                .unwrap_err()
                .to_string()
                .contains("could not parse Cargo metadata")
        );

        let cargo = project.executable(
            "cargo-missing-root",
            "#!/bin/sh\necho '{\"packages\":[]}'\n",
        );
        environment.set("CARGO", cargo.as_os_str());
        assert!(
            workspace_manifests(&project.context())
                .unwrap_err()
                .to_string()
                .contains("workspace root")
        );

        let cargo = project.executable(
            "cargo-missing-packages",
            "#!/bin/sh\necho '{\"workspace_root\":\"/tmp\"}'\n",
        );
        environment.set("CARGO", cargo.as_os_str());
        assert!(
            workspace_manifests(&project.context())
                .unwrap_err()
                .to_string()
                .contains("packages array")
        );

        let cargo = project.executable(
            "cargo-missing-manifest",
            "#!/bin/sh\necho '{\"workspace_root\":\"/tmp\",\"packages\":[{}]}'\n",
        );
        environment.set("CARGO", cargo.as_os_str());
        assert!(
            workspace_manifests(&project.context())
                .unwrap_err()
                .to_string()
                .contains("package has no manifest path")
        );
    }

    #[test]
    fn replaces_files_atomically_and_reports_paths_without_parents() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("manifest.toml");
        std::fs::write(&path, "old\n").unwrap();

        replace_file(&path, "new\n").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        assert!(
            replace_file(Path::new("/"), "contents")
                .unwrap_err()
                .to_string()
                .contains("has no parent directory")
        );

        assert!(replace_file(&directory.path().join("missing.toml"), "contents").is_err());
        assert!(replace_file(directory.path(), "contents").is_err());

        let mut environment = Environment::new();
        for operation in [
            "metadata",
            "create_temp",
            "write_temp",
            "set_permissions",
            "sync",
            "persist",
        ] {
            environment.set("BAKE_TEST_VERSION_IO_FAILURE", operation);
            assert!(replace_file(&path, "new\n").is_err(), "{operation}");
        }
    }

    #[test]
    fn rejects_nonincreasing_internal_workspace_updates() {
        let project = Project::new();
        let mut environment = Environment::new();
        project.single_package("fixture", "1.2.3");
        project.cargo_proxy(&mut environment, None);

        assert!(
            set_workspace_version(
                &project.context(),
                &[package("fixture", "1.2.3")],
                Version::parse("1.2.3").unwrap(),
                Version::parse("1.2.3").unwrap(),
            )
            .unwrap_err()
            .to_string()
            .contains("must be greater")
        );
    }
}
