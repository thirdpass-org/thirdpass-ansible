use anyhow::{format_err, Context, Result};

static HOST_NAME: &str = "galaxy.ansible.com";

/// Returns global dependencies.
pub fn get_global_dependencies() -> Result<std::collections::HashMap<String, String>> {
    let handle = std::process::Command::new("ansible-galaxy")
        .args(["collection", "list", "--format", "json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .output()?;
    let stdout = String::from_utf8_lossy(&handle.stdout);
    let stdout = stdout.to_string();

    let mut dependencies = std::collections::HashMap::<String, String>::new();

    let json: serde_json::Value = serde_json::from_str(&stdout)?;
    let json = match json.as_object() {
        Some(x) => x,
        None => return Ok(dependencies),
    };

    let json_error_message =
        "Failed to parse JSON from command: ansible-galaxy collection list --format json";

    for (_collections_directory, packages) in json.into_iter() {
        let packages = packages
            .as_object()
            .ok_or(format_err!(json_error_message))?;
        for (package_name, package_info) in packages {
            let package_version = match package_info["version"].as_str() {
                Some(x) => x,
                None => continue,
            };
            dependencies.insert(package_name.clone(), package_version.to_string());
        }
    }

    Ok(dependencies)
}

/// Order newest version greater than oldest.
fn order_version_requirement_comparators(
    a: &semver::Comparator,
    b: &semver::Comparator,
) -> std::cmp::Ordering {
    let major_ord = a.major.cmp(&b.major);
    if major_ord != std::cmp::Ordering::Equal {
        return major_ord;
    }

    let minor_ord = a.minor.unwrap_or(0).cmp(&b.minor.unwrap_or(0));
    if minor_ord != std::cmp::Ordering::Equal {
        return minor_ord;
    }

    let patch_ord = a.patch.unwrap_or(0).cmp(&b.patch.unwrap_or(0));
    if patch_ord != std::cmp::Ordering::Equal {
        return patch_ord;
    }

    let prerelease_ord = a.pre.cmp(&b.pre);
    if prerelease_ord != std::cmp::Ordering::Equal {
        return prerelease_ord;
    }

    std::cmp::Ordering::Equal
}

fn select_latest_equal_comparator(
    comparators: &[semver::Comparator],
) -> Option<semver::Comparator> {
    let mut comparators = comparators.to_owned();
    comparators.sort_by(order_version_requirement_comparators);
    let mut selected_comparator = None;
    for comparator in comparators {
        if comparator.op == semver::Op::Exact
            || comparator.op == semver::Op::GreaterEq
            || comparator.op == semver::Op::LessEq
            || comparator.op == semver::Op::Tilde
            || comparator.op == semver::Op::Caret
        {
            selected_comparator = Some(comparator);
        }
    }
    selected_comparator
}

#[test]
fn test_select_latest_equal_comparator() -> Result<()> {
    let comparators = vec![
        semver::Comparator::parse("=1.3.2")?,
        semver::Comparator::parse(">=2.3.2")?,
        semver::Comparator::parse(">3.3.2")?,
    ];
    let result = select_latest_equal_comparator(&comparators);
    let expected = Some(semver::Comparator::parse(">=2.3.2")?);
    assert_eq!(result, expected);
    Ok(())
}

pub(crate) fn normalize_version(version: &str) -> Result<String> {
    let mut split = version.split("-");
    let prefix = split
        .next()
        .ok_or(format_err!("Failed to parse version: {}", version))?;
    let mut prefix = String::from(prefix);

    let count_periods = prefix.chars().filter(|c| c == &'.').count();

    if count_periods == 0 {
        prefix += ".0.0";
    } else if count_periods == 1 {
        prefix += ".0";
    }

    for part in split {
        prefix += "-";
        prefix += part;
    }
    let normalized_version = prefix;
    Ok(normalized_version)
}

#[test]
fn test_normalize_version() -> Result<()> {
    assert_eq!(normalize_version("0.1")?, "0.1.0".to_string());
    assert_eq!(
        normalize_version("0.1-alpha-123")?,
        "0.1.0-alpha-123".to_string()
    );
    assert_eq!(normalize_version("1")?, "1.0.0".to_string());
    Ok(())
}

/// Convert a version comparator to a plain version by stripping the operator prefix.
fn comparator_to_version(comparator: &semver::Comparator) -> Result<semver::Version> {
    let comparator_str = comparator.to_string();

    let op_str = match comparator.op {
        semver::Op::Exact => "=",
        semver::Op::Greater => ">",
        semver::Op::GreaterEq => ">=",
        semver::Op::Less => "<",
        semver::Op::LessEq => "<=",
        semver::Op::Tilde => "~",
        semver::Op::Caret => "^",
        semver::Op::Wildcard => "*",
        _ => "",
    };

    let version_str = comparator_str.trim_start_matches(op_str);
    let version = normalize_version(version_str)?;
    let version = semver::Version::parse(&version);
    Ok(version?)
}

/// Given version requirement and installed global package version,
/// return the version which is probabilistically most relevant.
fn package_specific_version_from_requirement(
    version_requirement: &semver::VersionReq,
    global_version: Option<semver::Version>,
) -> std::result::Result<semver::Version, thirdpass_core::extension::VersionError> {
    if let Some(global_version) = global_version {
        if version_requirement.matches(&global_version) {
            return Ok(global_version);
        }
    } else {
        let comparator = select_latest_equal_comparator(&version_requirement.comparators)
            .ok_or(thirdpass_core::extension::VersionError::from_missing_version())?;
        let version = comparator_to_version(&comparator).or(Err(
            thirdpass_core::extension::VersionError::from_missing_version(),
        ))?;
        return Ok(version);
    }
    Err(thirdpass_core::extension::VersionError::from_missing_version())
}

/// Parse dependencies from project MANIFEST.json file.
pub fn get_manifest_dependencies(
    file_path: &std::path::Path,
    global_dependencies: &std::collections::HashMap<String, String>,
) -> Result<std::collections::HashSet<thirdpass_core::extension::Dependency>> {
    let file = std::fs::File::open(file_path)?;
    let reader = std::io::BufReader::new(file);
    let package_meta: serde_json::Value = serde_json::from_reader(reader)
        .context(format!("Failed to parse json: {}", file_path.display()))?;
    let raw_dependencies = &package_meta["collection_info"]["dependencies"]
        .as_object()
        .ok_or(format_err!(
            "Failed to parse dependencies section as object."
        ))?;

    let mut dependencies =
        std::collections::HashSet::<thirdpass_core::extension::Dependency>::new();
    for (package_name, version_requirement) in raw_dependencies.iter() {
        let version_requirement = version_requirement.as_str().ok_or(format_err!(
            "Failed to parse version requirement as string."
        ))?;

        let version_requirement = semver::VersionReq::parse(version_requirement)?;
        let global_version = global_dependencies
            .get(package_name.as_str())
            .and_then(|f| semver::Version::parse(f.as_str()).ok());
        let version =
            package_specific_version_from_requirement(&version_requirement, global_version);

        dependencies.insert(thirdpass_core::extension::Dependency {
            name: package_name.clone(),
            version: version.map(|v| v.to_string()),
        });
    }

    Ok(dependencies)
}

/// Parse dependencies from project galaxy.yml file.
pub fn get_galaxy_yml_dependencies(
    file_path: &std::path::Path,
    global_dependencies: &std::collections::HashMap<String, String>,
) -> Result<std::collections::HashSet<thirdpass_core::extension::Dependency>> {
    let file = std::fs::File::open(file_path)?;
    let reader = std::io::BufReader::new(file);
    let package_meta: serde_json::Value = serde_yaml::from_reader(reader)
        .context(format!("Failed to parse json: {}", file_path.display()))?;
    let raw_dependencies = &package_meta["dependencies"].as_object().ok_or(format_err!(
        "Failed to parse dependencies section as object."
    ))?;

    let mut dependencies =
        std::collections::HashSet::<thirdpass_core::extension::Dependency>::new();
    for (package_name, version_requirement) in raw_dependencies.iter() {
        let version_requirement = version_requirement.as_str().ok_or(format_err!(
            "Failed to parse version requirement as string."
        ))?;

        let version_requirement = semver::VersionReq::parse(version_requirement)?;
        let global_version = global_dependencies
            .get(package_name.as_str())
            .and_then(|f| semver::Version::parse(f.as_str()).ok());
        let version =
            package_specific_version_from_requirement(&version_requirement, global_version);

        dependencies.insert(thirdpass_core::extension::Dependency {
            name: package_name.clone(),
            version: version.map(|v| v.to_string()),
        });
    }

    Ok(dependencies)
}

pub fn get_registry_host_name() -> String {
    HOST_NAME.to_string()
}
