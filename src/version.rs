use bake::{Context, Error, Result, Value};
use serde_json::json;
use std::collections::{BTreeSet, HashSet};
use std::fs;
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
        let source = fs::read_to_string(&path)
            .map_err(|error| Error::new(format!("{}: {error}", path.display())))?;
        let mut document: DocumentMut = source
            .parse()
            .map_err(|error| Error::new(format!("{}: {error}", path.display())))?;
        if publishable_manifests.contains(&path) {
            update_package_version(&mut document, current, target)?;
        }
        if path == workspace_manifest {
            update_workspace_version(&mut document, current, target)?;
        }
        update_local_dependency_versions(&mut document, target, &package_names);
        updated.push((path, document.to_string()));
    }

    if updated.is_empty() {
        return Err(Error::new("no Cargo manifests were found to update"));
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
    let permissions = fs::metadata(path)?.permissions();
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(contents.as_bytes())?;
    temporary.as_file().set_permissions(permissions)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| Error::from(error.error))?;
    Ok(())
}
