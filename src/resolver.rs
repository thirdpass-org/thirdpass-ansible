//! Recursive Ansible Galaxy dependency resolution.
//!
//! Galaxy stores collection dependencies as collection names with version
//! requirements. This module resolves those requirements to exact collection
//! versions, then walks the transitive closure while tracking visited packages
//! to avoid cycles.

use anyhow::{format_err, Context, Result};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Resolve package dependencies for one Ansible Galaxy collection release.
pub(crate) fn identify_package_dependencies(
    package_name: &str,
    package_version: &Option<&str>,
) -> Result<Vec<thirdpass_core::extension::PackageDependencies>> {
    let source = HttpGalaxySource;
    let dependencies =
        identify_package_dependencies_with_source(&source, package_name, package_version)?;
    Ok(vec![dependencies])
}

/// Resolve the latest Galaxy version for a collection.
pub(crate) fn latest_version(package_name: &str) -> Result<String> {
    let source = HttpGalaxySource;
    select_latest_version(&source.package_versions(package_name)?)
}

trait GalaxySource {
    fn package_entry(&self, package_name: &str, package_version: &str)
        -> Result<serde_json::Value>;

    fn package_versions(&self, package_name: &str) -> Result<Vec<String>>;
}

struct HttpGalaxySource;

impl GalaxySource for HttpGalaxySource {
    fn package_entry(
        &self,
        package_name: &str,
        package_version: &str,
    ) -> Result<serde_json::Value> {
        crate::get_registry_entry_json(package_name, package_version)
    }

    fn package_versions(&self, package_name: &str) -> Result<Vec<String>> {
        crate::get_registry_versions(package_name)
    }
}

fn identify_package_dependencies_with_source(
    source: &dyn GalaxySource,
    package_name: &str,
    package_version: &Option<&str>,
) -> Result<thirdpass_core::extension::PackageDependencies> {
    let package_version = match package_version {
        Some(version) => version.to_string(),
        None => select_latest_version(&source.package_versions(package_name)?)?,
    };
    let dependencies = resolve_dependency_closure(source, package_name, &package_version)?;

    Ok(thirdpass_core::extension::PackageDependencies {
        package_version: Ok(package_version),
        registry_host_name: crate::galaxy::get_registry_host_name(),
        dependencies,
    })
}

#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd)]
struct PackageKey {
    name: String,
    version: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct DependencyRequirement {
    name: String,
    requirement: String,
}

fn resolve_dependency_closure(
    source: &dyn GalaxySource,
    package_name: &str,
    package_version: &str,
) -> Result<Vec<thirdpass_core::extension::Dependency>> {
    let root = PackageKey {
        name: package_name.to_string(),
        version: package_version.to_string(),
    };
    let root_entry = source.package_entry(package_name, package_version)?;
    let mut pending =
        VecDeque::<DependencyRequirement>::from(dependency_requirements(&root_entry)?);
    let mut visited = BTreeSet::<PackageKey>::new();
    let mut dependencies = BTreeMap::<PackageKey, thirdpass_core::extension::Dependency>::new();
    visited.insert(root.clone());

    while let Some(requirement) = pending.pop_front() {
        let version = resolve_requirement(source, &requirement.name, &requirement.requirement)?;
        let package = PackageKey {
            name: requirement.name,
            version,
        };
        if !visited.insert(package.clone()) {
            continue;
        }

        if package != root {
            dependencies.insert(
                package.clone(),
                thirdpass_core::extension::Dependency {
                    name: package.name.clone(),
                    version: Ok(package.version.clone()),
                },
            );
        }

        let entry = source.package_entry(&package.name, &package.version)?;
        for dependency in dependency_requirements(&entry)? {
            pending.push_back(dependency);
        }
    }

    Ok(dependencies.into_values().collect())
}

fn dependency_requirements(entry: &serde_json::Value) -> Result<Vec<DependencyRequirement>> {
    let raw_dependencies = match entry["metadata"]["dependencies"].as_object() {
        Some(dependencies) => dependencies,
        None if entry["metadata"]["dependencies"].is_null() => return Ok(Vec::new()),
        None => {
            return Err(format_err!(
                "Failed to parse collection dependencies section as object."
            ))
        }
    };

    let mut dependencies = Vec::new();
    for (name, requirement) in raw_dependencies {
        let requirement = requirement.as_str().ok_or(format_err!(
            "Failed to parse collection dependency requirement as string."
        ))?;
        dependencies.push(DependencyRequirement {
            name: name.clone(),
            requirement: requirement.to_string(),
        });
    }
    Ok(dependencies)
}

fn resolve_requirement(
    source: &dyn GalaxySource,
    package_name: &str,
    requirement: &str,
) -> Result<String> {
    let version_req = parse_version_requirement(requirement)?;
    let versions = source.package_versions(package_name)?;
    select_latest_matching_version(&versions, &version_req).with_context(|| {
        format!(
            "Failed to resolve dependency {} with requirement {}.",
            package_name, requirement
        )
    })
}

fn parse_version_requirement(requirement: &str) -> Result<semver::VersionReq> {
    let requirement = normalize_version_requirement(requirement)?;
    semver::VersionReq::parse(&requirement).context(format!(
        "Failed to parse collection dependency requirement: {}",
        requirement
    ))
}

fn normalize_version_requirement(requirement: &str) -> Result<String> {
    let requirement = requirement.trim();
    if requirement.is_empty() || requirement == "*" {
        return Ok("*".to_string());
    }

    let mut comparators = Vec::new();
    for comparator in requirement.split(',') {
        comparators.push(normalize_requirement_comparator(comparator.trim())?);
    }
    Ok(comparators.join(", "))
}

fn normalize_requirement_comparator(comparator: &str) -> Result<String> {
    if comparator.is_empty() || comparator == "*" {
        return Ok("*".to_string());
    }

    let (operator, version) = if let Some(version) = comparator.strip_prefix(">=") {
        (">=", version)
    } else if let Some(version) = comparator.strip_prefix("<=") {
        ("<=", version)
    } else if let Some(version) = comparator.strip_prefix("==") {
        ("=", version)
    } else if let Some(version) = comparator.strip_prefix('=') {
        ("=", version)
    } else if let Some(version) = comparator.strip_prefix('>') {
        (">", version)
    } else if let Some(version) = comparator.strip_prefix('<') {
        ("<", version)
    } else if let Some(version) = comparator.strip_prefix('~') {
        ("~", version)
    } else if let Some(version) = comparator.strip_prefix('^') {
        ("^", version)
    } else {
        ("", comparator)
    };

    let version = version.trim();
    if version == "*" {
        return Ok("*".to_string());
    }
    Ok(format!(
        "{}{}",
        operator,
        crate::galaxy::normalize_version(version)?
    ))
}

fn select_latest_version(versions: &[String]) -> Result<String> {
    let mut versions = parse_versions(versions);
    versions.sort_by(|left, right| left.0.cmp(&right.0));
    versions
        .last()
        .map(|(_version, original)| original.clone())
        .ok_or(format_err!("Failed to find latest version."))
}

fn select_latest_matching_version(
    versions: &[String],
    requirement: &semver::VersionReq,
) -> Result<String> {
    let mut versions = parse_versions(versions);
    versions.sort_by(|left, right| left.0.cmp(&right.0));
    versions
        .into_iter()
        .rev()
        .find(|(version, _original)| requirement.matches(version))
        .map(|(_version, original)| original)
        .ok_or(format_err!("Failed to find matching version."))
}

fn parse_versions(versions: &[String]) -> Vec<(semver::Version, String)> {
    versions
        .iter()
        .filter_map(|version| {
            crate::galaxy::normalize_version(version)
                .ok()
                .and_then(|normalized| semver::Version::parse(&normalized).ok())
                .map(|parsed| (parsed, version.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeGalaxySource {
        entries: BTreeMap<PackageKey, serde_json::Value>,
        versions: BTreeMap<String, Vec<String>>,
    }

    impl FakeGalaxySource {
        fn add_package(
            &mut self,
            package_name: &str,
            package_version: &str,
            dependencies: &[(&str, &str)],
        ) {
            self.entries.insert(
                PackageKey {
                    name: package_name.to_string(),
                    version: package_version.to_string(),
                },
                collection_entry(dependencies),
            );
            self.versions
                .entry(package_name.to_string())
                .or_default()
                .push(package_version.to_string());
        }
    }

    impl GalaxySource for FakeGalaxySource {
        fn package_entry(
            &self,
            package_name: &str,
            package_version: &str,
        ) -> Result<serde_json::Value> {
            self.entries
                .get(&PackageKey {
                    name: package_name.to_string(),
                    version: package_version.to_string(),
                })
                .cloned()
                .ok_or(format_err!(
                    "missing fake package {}@{}",
                    package_name,
                    package_version
                ))
        }

        fn package_versions(&self, package_name: &str) -> Result<Vec<String>> {
            self.versions
                .get(package_name)
                .cloned()
                .ok_or(format_err!("missing fake versions for {}", package_name))
        }
    }

    #[test]
    fn package_dependencies_resolve_transitive_closure() -> Result<()> {
        let mut source = FakeGalaxySource::default();
        source.add_package(
            "example.root",
            "1.0.0",
            &[
                ("example.one", ">=1.0.0,<2.0.0"),
                ("example.two", ">=2.0.0"),
            ],
        );
        source.add_package("example.one", "1.0.0", &[]);
        source.add_package("example.one", "1.2.0", &[("example.three", "==3.1")]);
        source.add_package("example.one", "2.0.0", &[]);
        source.add_package("example.two", "2.0.0", &[]);
        source.add_package("example.three", "3.1.0", &[]);

        let dependencies =
            identify_package_dependencies_with_source(&source, "example.root", &Some("1.0.0"))?;

        assert_eq!(dependencies.package_version, Ok("1.0.0".to_string()));
        assert_eq!(dependencies.registry_host_name, "galaxy.ansible.com");
        assert_dependency(&dependencies.dependencies, "example.one", "1.2.0");
        assert_dependency(&dependencies.dependencies, "example.two", "2.0.0");
        assert_dependency(&dependencies.dependencies, "example.three", "3.1.0");
        assert_eq!(dependencies.dependencies.len(), 3);
        Ok(())
    }

    #[test]
    fn package_dependencies_resolve_missing_target_version() -> Result<()> {
        let mut source = FakeGalaxySource::default();
        source.add_package("example.root", "1.0.0", &[]);
        source.add_package("example.root", "1.2.0", &[]);

        let dependencies =
            identify_package_dependencies_with_source(&source, "example.root", &None)?;

        assert_eq!(dependencies.package_version, Ok("1.2.0".to_string()));
        assert!(dependencies.dependencies.is_empty());
        Ok(())
    }

    #[test]
    fn package_dependencies_skip_cycle_to_root() -> Result<()> {
        let mut source = FakeGalaxySource::default();
        source.add_package("example.root", "1.0.0", &[("example.one", ">=1.0.0")]);
        source.add_package("example.one", "1.0.0", &[("example.root", ">=1.0.0")]);

        let dependencies =
            identify_package_dependencies_with_source(&source, "example.root", &Some("1.0.0"))?;

        assert_dependency(&dependencies.dependencies, "example.one", "1.0.0");
        assert!(dependencies
            .dependencies
            .iter()
            .all(|dependency| dependency.name != "example.root"));
        Ok(())
    }

    #[test]
    fn package_dependencies_report_unsatisfied_requirement() {
        let mut source = FakeGalaxySource::default();
        source.add_package("example.root", "1.0.0", &[("example.one", ">=2.0.0")]);
        source.add_package("example.one", "1.0.0", &[]);

        let error =
            identify_package_dependencies_with_source(&source, "example.root", &Some("1.0.0"))
                .expect_err("expected requirement resolution to fail");

        assert!(error
            .to_string()
            .contains("Failed to resolve dependency example.one with requirement >=2.0.0."));
    }

    #[test]
    fn version_requirements_normalize_ansible_syntax() -> Result<()> {
        let requirement = parse_version_requirement(">= 1.0, ==2.0.0")?;

        assert!(requirement.matches(&semver::Version::parse("2.0.0")?));
        assert!(!requirement.matches(&semver::Version::parse("1.5.0")?));
        Ok(())
    }

    #[test]
    fn latest_matching_version_uses_full_candidate_set() -> Result<()> {
        let versions = vec![
            "2.0.0".to_string(),
            "1.5.0".to_string(),
            "1.2.0".to_string(),
        ];
        let requirement = parse_version_requirement("<2.0.0")?;

        assert_eq!(
            select_latest_matching_version(&versions, &requirement)?,
            "1.5.0"
        );
        Ok(())
    }

    fn collection_entry(dependencies: &[(&str, &str)]) -> serde_json::Value {
        let dependencies = dependencies
            .iter()
            .map(|(name, requirement)| ((*name).to_string(), serde_json::json!(requirement)))
            .collect::<serde_json::Map<_, _>>();
        serde_json::json!({
            "metadata": {
                "dependencies": dependencies
            }
        })
    }

    fn assert_dependency(
        dependencies: &[thirdpass_core::extension::Dependency],
        name: &str,
        version: &str,
    ) {
        assert!(
            dependencies
                .iter()
                .any(|dependency| dependency.name == name
                    && dependency.version == Ok(version.into())),
            "expected dependency {}@{} in {:?}",
            name,
            version,
            dependencies
        );
    }
}
