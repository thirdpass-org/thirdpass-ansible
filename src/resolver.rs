//! Recursive Ansible Galaxy dependency resolution.
//!
//! Galaxy stores collection dependencies as collection names with version
//! requirements. This module resolves those requirements to exact collection
//! versions, then walks the transitive closure while tracking visited packages
//! to avoid cycles.

use anyhow::{format_err, Context, Result};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Resolve package dependencies for one Ansible Galaxy collection release.
pub(crate) fn identify_package_dependencies(
    package_name: &str,
    package_version: &Option<&str>,
) -> Result<Vec<thirdpass_core::extension::PackageDependencies>> {
    let source = CachedGalaxySource::new(HttpGalaxySource);
    let dependencies =
        identify_package_dependencies_with_source(&source, package_name, package_version)?;
    Ok(vec![dependencies])
}

/// Resolve the latest Galaxy version for a collection.
pub(crate) fn latest_version(package_name: &str) -> Result<String> {
    let source = CachedGalaxySource::new(HttpGalaxySource);
    crate::version::select_latest_version(&source.package_versions(package_name)?)
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

struct CachedGalaxySource<T> {
    source: T,
    entries: RefCell<BTreeMap<PackageKey, serde_json::Value>>,
    versions: RefCell<BTreeMap<String, Vec<String>>>,
}

impl<T> CachedGalaxySource<T> {
    fn new(source: T) -> Self {
        Self {
            source,
            entries: RefCell::new(BTreeMap::new()),
            versions: RefCell::new(BTreeMap::new()),
        }
    }
}

impl<T: GalaxySource> GalaxySource for CachedGalaxySource<T> {
    fn package_entry(
        &self,
        package_name: &str,
        package_version: &str,
    ) -> Result<serde_json::Value> {
        let key = PackageKey {
            name: package_name.to_string(),
            version: package_version.to_string(),
        };
        if let Some(entry) = self.entries.borrow().get(&key).cloned() {
            return Ok(entry);
        }

        let entry = self.source.package_entry(package_name, package_version)?;
        self.entries.borrow_mut().insert(key, entry.clone());
        Ok(entry)
    }

    fn package_versions(&self, package_name: &str) -> Result<Vec<String>> {
        if let Some(versions) = self.versions.borrow().get(package_name).cloned() {
            return Ok(versions);
        }

        let versions = self.source.package_versions(package_name)?;
        self.versions
            .borrow_mut()
            .insert(package_name.to_string(), versions.clone());
        Ok(versions)
    }
}

fn identify_package_dependencies_with_source(
    source: &dyn GalaxySource,
    package_name: &str,
    package_version: &Option<&str>,
) -> Result<thirdpass_core::extension::PackageDependencies> {
    let package_version = match package_version {
        Some(version) => version.to_string(),
        None => crate::version::select_latest_version(&source.package_versions(package_name)?)?,
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
    let version_req = crate::version::parse_version_requirement(requirement)?;
    let versions = source.package_versions(package_name)?;
    crate::version::select_latest_matching_version(&versions, &version_req).with_context(|| {
        format!(
            "Failed to resolve dependency {} with requirement {}.",
            package_name, requirement
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeGalaxySource {
        entries: BTreeMap<PackageKey, serde_json::Value>,
        versions: BTreeMap<String, Vec<String>>,
        entry_calls: RefCell<BTreeMap<PackageKey, usize>>,
        version_calls: RefCell<BTreeMap<String, usize>>,
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

        fn entry_call_count(&self, package_name: &str, package_version: &str) -> usize {
            self.entry_calls
                .borrow()
                .get(&PackageKey {
                    name: package_name.to_string(),
                    version: package_version.to_string(),
                })
                .copied()
                .unwrap_or(0)
        }

        fn version_call_count(&self, package_name: &str) -> usize {
            self.version_calls
                .borrow()
                .get(package_name)
                .copied()
                .unwrap_or(0)
        }
    }

    impl GalaxySource for FakeGalaxySource {
        fn package_entry(
            &self,
            package_name: &str,
            package_version: &str,
        ) -> Result<serde_json::Value> {
            let key = PackageKey {
                name: package_name.to_string(),
                version: package_version.to_string(),
            };
            *self
                .entry_calls
                .borrow_mut()
                .entry(key.clone())
                .or_default() += 1;
            self.entries.get(&key).cloned().ok_or(format_err!(
                "missing fake package {}@{}",
                package_name,
                package_version
            ))
        }

        fn package_versions(&self, package_name: &str) -> Result<Vec<String>> {
            *self
                .version_calls
                .borrow_mut()
                .entry(package_name.to_string())
                .or_default() += 1;
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
    fn package_dependencies_use_cached_galaxy_data() -> Result<()> {
        let mut source = FakeGalaxySource::default();
        source.add_package(
            "example.root",
            "1.0.0",
            &[("example.left", ">=1.0.0"), ("example.right", ">=1.0.0")],
        );
        source.add_package("example.left", "1.0.0", &[("example.shared", ">=1.0.0")]);
        source.add_package("example.right", "1.0.0", &[("example.shared", ">=1.0.0")]);
        source.add_package("example.shared", "1.0.0", &[]);
        let source = CachedGalaxySource::new(source);

        let dependencies =
            identify_package_dependencies_with_source(&source, "example.root", &Some("1.0.0"))?;

        assert_dependency(&dependencies.dependencies, "example.shared", "1.0.0");
        assert_eq!(source.source.version_call_count("example.shared"), 1);
        assert_eq!(source.source.entry_call_count("example.shared", "1.0.0"), 1);
        Ok(())
    }

    #[test]
    fn dependency_requirements_parse_real_galaxy_metadata_example() -> Result<()> {
        let entry = serde_json::json!({
            "version": "5.0.0",
            "metadata": {
                "dependencies": {
                    "ansible.utils": ">=2.7.0"
                }
            }
        });

        let requirements = dependency_requirements(&entry)?;

        assert_eq!(
            requirements,
            vec![DependencyRequirement {
                name: "ansible.utils".to_string(),
                requirement: ">=2.7.0".to_string()
            }]
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
