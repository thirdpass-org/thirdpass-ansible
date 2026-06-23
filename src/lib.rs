use anyhow::{format_err, Context, Result};
use std::io::Read;
use strum::IntoEnumIterator;

mod galaxy;
mod resolver;
mod version;

#[derive(Clone, Debug)]
pub struct AnsibleExtension {
    name_: String,
    registry_host_names_: Vec<String>,
    registry_human_url_template_: String,
}

impl thirdpass_core::extension::FromLib for AnsibleExtension {
    fn new() -> Self {
        Self {
            name_: "ansible".to_string(),
            registry_host_names_: vec!["galaxy.ansible.com".to_owned()],
            registry_human_url_template_:
                "https://galaxy.ansible.com/ui/repo/published/{{package_name}}/".to_string(),
        }
    }
}

impl thirdpass_core::extension::Extension for AnsibleExtension {
    fn name(&self) -> String {
        self.name_.clone()
    }

    fn registries(&self) -> Vec<String> {
        self.registry_host_names_.clone()
    }

    /// Returns a list of dependencies for the given package.
    ///
    /// Returns one package dependencies structure per registry.
    fn identify_package_dependencies(
        &self,
        package_name: &str,
        package_version: &Option<&str>,
        _extension_args: &[String],
    ) -> Result<Vec<thirdpass_core::extension::PackageDependencies>> {
        resolver::identify_package_dependencies(package_name, package_version)
    }

    fn identify_file_defined_dependencies(
        &self,
        working_directory: &std::path::Path,
        _extension_args: &[String],
    ) -> Result<Vec<thirdpass_core::extension::FileDefinedDependencies>> {
        // Identify dependency definition file.
        let dependency_files = identify_dependency_files(working_directory);
        let dependency_file = match select_preferred_dependency_file(&dependency_files) {
            Some(dependency_file) => dependency_file,
            None => return Ok(Vec::new()),
        };

        let global_dependencies = galaxy::get_global_dependencies()?;

        // Read all dependencies definitions files.
        let mut dependency_specs = Vec::new();
        let (dependencies, registry_host_name) = match dependency_file.r#type {
            DependencyFileType::GalaxyManifest => (
                galaxy::get_manifest_dependencies(&dependency_file.path, &global_dependencies)?,
                galaxy::get_registry_host_name(),
            ),
            DependencyFileType::GalaxyYml => (
                galaxy::get_galaxy_yml_dependencies(&dependency_file.path, &global_dependencies)?,
                galaxy::get_registry_host_name(),
            ),
        };
        dependency_specs.push(thirdpass_core::extension::FileDefinedDependencies {
            path: dependency_file.path.clone(),
            registry_host_name,
            dependencies: dependencies.into_iter().collect(),
        });

        Ok(dependency_specs)
    }

    fn registries_package_metadata(
        &self,
        package_name: &str,
        package_version: &Option<&str>,
    ) -> Result<Vec<thirdpass_core::extension::RegistryPackageMetadata>> {
        let package_version = match package_version {
            Some(v) => Some(v.to_string()),
            None => get_latest_version(package_name)?,
        }
        .ok_or(format_err!("Failed to find package version."))?;

        // Query remote package registry for given package.
        let human_url = get_registry_human_url(self, package_name)?;

        // Currently, only one registry is supported. Therefore simply extract.
        let registry_host_name = self
            .registries()
            .first()
            .ok_or(format_err!(
                "Code error: vector of registry host names is empty."
            ))?
            .clone();

        let entry_json = get_registry_entry_json(package_name, &package_version)?;
        let artifact_url = get_archive_url(&entry_json)?;

        Ok(vec![thirdpass_core::extension::RegistryPackageMetadata {
            registry_host_name,
            human_url: human_url.to_string(),
            artifact_url: artifact_url.to_string(),
            is_primary: true,
            package_version: package_version.to_string(),
        }])
    }
}

/// Given package name, return latest version.
fn get_latest_version(package_name: &str) -> Result<Option<String>> {
    resolver::latest_version(package_name).map(Some)
}

#[cfg(test)]
fn latest_version_from_versions_json(json: &serde_json::Value) -> Result<String> {
    let version_entries = json["data"]
        .as_array()
        .ok_or(format_err!("Failed to find data JSON section."))?;

    let mut versions = Vec::<semver::Version>::new();
    for version_entry in version_entries {
        let version_entry = version_entry
            .as_object()
            .ok_or(format_err!("Failed to parse version entry as JSON object."))?;
        let version = version_entry["version"]
            .as_str()
            .ok_or(format_err!("Failed to parse version as str."))?;
        let version = match semver::Version::parse(version) {
            Ok(v) => v,
            Err(_) => continue,
        };
        versions.push(version);
    }
    versions.sort();

    let latest_version = versions
        .last()
        .ok_or(format_err!("Failed to find latest version."))?;
    Ok(latest_version.to_string())
}

fn get_registry_human_url(extension: &AnsibleExtension, package_name: &str) -> Result<url::Url> {
    // Example return value: https://galaxy.ansible.com/crivetimihai/development
    let package_name = package_name.replace(".", "/");
    let handlebars_registry = handlebars::Handlebars::new();
    let url = handlebars_registry.render_template(
        &extension.registry_human_url_template_,
        &maplit::btreemap! {
            "package_name" => package_name,
        },
    )?;
    Ok(url::Url::parse(url.as_str())?)
}

fn get_registry_versions_url(package_name: &str) -> Result<String> {
    let package_name = package_name.replace(".", "/");
    let handlebars_registry = handlebars::Handlebars::new();
    Ok(handlebars_registry.render_template(
        "https://galaxy.ansible.com/api/v3/plugin/ansible/content/published/collections/index/{{package_name}}/versions/",
        &maplit::btreemap! {"package_name" => package_name},
    )?)
}

pub(crate) fn get_registry_versions(package_name: &str) -> Result<Vec<String>> {
    let mut versions = Vec::new();
    let mut next_url = Some(get_registry_versions_url(package_name)?);
    while let Some(url) = next_url {
        let json = get_registry_json(&url)?;
        versions.extend(versions_from_versions_json(&json)?);
        next_url = next_versions_url(&json)?;
    }
    Ok(versions)
}

pub(crate) fn get_registry_entry_json(
    package_name: &str,
    package_version: &str,
) -> Result<serde_json::Value> {
    let package_name = package_name.replace(".", "/");
    let handlebars_registry = handlebars::Handlebars::new();
    let json_url = handlebars_registry.render_template(
        "https://galaxy.ansible.com/api/v3/plugin/ansible/content/published/collections/index/{{package_name}}/versions/{{package_version}}/",
        &maplit::btreemap! {"package_name" => package_name, "package_version" => package_version.to_string()},
    )?;

    get_registry_json(&json_url)
}

fn versions_from_versions_json(json: &serde_json::Value) -> Result<Vec<String>> {
    let version_entries = json["data"]
        .as_array()
        .ok_or(format_err!("Failed to find data JSON section."))?;
    let mut versions = Vec::new();
    for version_entry in version_entries {
        let version = version_entry["version"]
            .as_str()
            .ok_or(format_err!("Failed to parse version as str."))?;
        versions.push(version.to_string());
    }
    Ok(versions)
}

fn next_versions_url(json: &serde_json::Value) -> Result<Option<String>> {
    let next = match json["links"]["next"].as_str() {
        Some(next) => next,
        None => return Ok(None),
    };
    if next.starts_with("http://") || next.starts_with("https://") {
        Ok(Some(next.to_string()))
    } else {
        Ok(Some(format!("https://galaxy.ansible.com{}", next)))
    }
}

fn get_registry_json(json_url: &str) -> Result<serde_json::Value> {
    let mut result = reqwest::blocking::get(json_url)?;
    let status = result.status();
    let mut body = String::new();
    result.read_to_string(&mut body)?;
    if !status.is_success() {
        return Err(format_err!(
            "Galaxy registry request failed ({}): {}",
            status,
            body
        ));
    }

    serde_json::from_str(&body).context(format!("JSON was not well-formatted:\n{}", body))
}

fn get_archive_url(registry_entry_json: &serde_json::Value) -> Result<url::Url> {
    Ok(url::Url::parse(
        registry_entry_json["download_url"]
            .as_str()
            .ok_or(format_err!("Failed to parse package archive URL."))?,
    )?)
}

/// Package dependency file types.
#[derive(Debug, Copy, Clone, strum_macros::EnumIter)]
enum DependencyFileType {
    GalaxyManifest,
    GalaxyYml,
}

impl DependencyFileType {
    /// Return file name associated with dependency type.
    pub fn file_name(&self) -> std::path::PathBuf {
        match self {
            Self::GalaxyManifest => std::path::PathBuf::from("MANIFEST.json"),
            Self::GalaxyYml => std::path::PathBuf::from("galaxy.yml"),
        }
    }
}

/// Package dependency file type and file path.
#[derive(Debug, Clone)]
struct DependencyFile {
    r#type: DependencyFileType,
    path: std::path::PathBuf,
}

/// Select preferred galaxy.yml dependency file type.
fn select_preferred_dependency_file(
    dependency_files: &[DependencyFile],
) -> Option<&DependencyFile> {
    if dependency_files
        .iter()
        .any(|file| matches!(file.r#type, DependencyFileType::GalaxyYml))
    {
        dependency_files
            .iter()
            .find(|file| matches!(file.r#type, DependencyFileType::GalaxyYml))
    } else {
        dependency_files.first()
    }
}

/// Returns a vector of identified package dependency definition files.
///
/// Walks up the directory tree directory tree until the first positive result is found.
fn identify_dependency_files(working_directory: &std::path::Path) -> Vec<DependencyFile> {
    assert!(working_directory.is_absolute());
    let mut working_directory = working_directory.to_path_buf();

    loop {
        // If at least one target is found, assume package is present.
        let mut found_dependency_file = false;

        let mut dependency_files: Vec<DependencyFile> = Vec::new();
        for dependency_file_type in DependencyFileType::iter() {
            let target_absolute_path = working_directory.join(dependency_file_type.file_name());
            if target_absolute_path.is_file() {
                found_dependency_file = true;
                dependency_files.push(DependencyFile {
                    r#type: dependency_file_type,
                    path: target_absolute_path,
                })
            }
        }
        if found_dependency_file {
            return dependency_files;
        }

        // No need to move further up the directory tree after this loop.
        if working_directory == std::path::Path::new("/") {
            break;
        }

        // Move further up the directory tree.
        working_directory.pop();
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use thirdpass_core::extension::FromLib;

    #[test]
    fn latest_version_reads_galaxy_v3_data_entries() -> Result<()> {
        let json = serde_json::json!({
            "meta": { "count": 2 },
            "data": [
                { "version": "1.0.0" },
                { "version": "1.2.0" }
            ]
        });

        assert_eq!(latest_version_from_versions_json(&json)?, "1.2.0");
        Ok(())
    }

    #[test]
    fn version_page_reads_versions_and_relative_next_link() -> Result<()> {
        let next_path = "/api/v3/plugin/ansible/content/published/collections/index/example/root/versions/?limit=10&offset=10";
        let json = serde_json::json!({
            "data": [
                { "version": "2.0.0" },
                { "version": "1.9.0" }
            ],
            "links": {
                "next": next_path
            }
        });

        assert_eq!(
            versions_from_versions_json(&json)?,
            vec!["2.0.0".to_string(), "1.9.0".to_string()]
        );
        assert_eq!(
            next_versions_url(&json)?,
            Some(format!("https://galaxy.ansible.com{}", next_path))
        );
        Ok(())
    }

    #[test]
    fn archive_url_reads_galaxy_v3_download_url() -> Result<()> {
        let json = serde_json::json!({
            "download_url": "https://galaxy.ansible.com/api/v3/plugin/ansible/content/published/collections/artifacts/c01110011-protonpass-1.0.0.tar.gz"
        });

        assert_eq!(
            get_archive_url(&json)?.as_str(),
            "https://galaxy.ansible.com/api/v3/plugin/ansible/content/published/collections/artifacts/c01110011-protonpass-1.0.0.tar.gz"
        );
        Ok(())
    }

    #[test]
    fn human_url_uses_published_collection_route() -> Result<()> {
        let extension = AnsibleExtension::new();

        assert_eq!(
            get_registry_human_url(&extension, "c01110011.protonpass")?.as_str(),
            "https://galaxy.ansible.com/ui/repo/published/c01110011/protonpass/"
        );
        Ok(())
    }
}
