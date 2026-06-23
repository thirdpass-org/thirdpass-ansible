//! Galaxy version and requirement parsing helpers.

use anyhow::{format_err, Context, Result};

/// Normalize Ansible collection versions into semver's three-component form.
pub(crate) fn normalize_version(version: &str) -> Result<String> {
    let mut split = version.split('-');
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
    Ok(prefix)
}

/// Parse a Galaxy collection version requirement.
pub(crate) fn parse_version_requirement(requirement: &str) -> Result<semver::VersionReq> {
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
    Ok(format!("{}{}", operator, normalize_version(version)?))
}

/// Select the newest semver-compatible version.
pub(crate) fn select_latest_version(versions: &[String]) -> Result<String> {
    let mut versions = parse_versions(versions);
    versions.sort_by(|left, right| left.0.cmp(&right.0));
    versions
        .last()
        .map(|(_version, original)| original.clone())
        .ok_or(format_err!("Failed to find latest version."))
}

/// Select the newest semver-compatible version matching the requirement.
pub(crate) fn select_latest_matching_version(
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
        .filter_map(|version| match parse_version(version) {
            Ok(parsed) => Some((parsed, version.clone())),
            Err(error) => {
                log::debug!(
                    "Skipping Galaxy version {} because it is not semver-compatible: {}",
                    version,
                    error
                );
                None
            }
        })
        .collect()
}

fn parse_version(version: &str) -> Result<semver::Version> {
    let normalized = normalize_version(version)?;
    semver::Version::parse(&normalized).context(format!(
        "Failed to parse normalized version: {}",
        normalized
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_version_adds_missing_components() -> Result<()> {
        assert_eq!(normalize_version("0.1")?, "0.1.0".to_string());
        assert_eq!(
            normalize_version("0.1-alpha-123")?,
            "0.1.0-alpha-123".to_string()
        );
        assert_eq!(normalize_version("1")?, "1.0.0".to_string());
        Ok(())
    }

    #[test]
    fn version_requirements_normalize_ansible_syntax() -> Result<()> {
        let requirement = parse_version_requirement(">= 1.0, ==2.0.0")?;

        assert!(requirement.matches(&semver::Version::parse("2.0.0")?));
        assert!(!requirement.matches(&semver::Version::parse("1.5.0")?));
        Ok(())
    }

    #[test]
    fn version_requirements_parse_real_galaxy_shapes() -> Result<()> {
        assert!(parse_version_requirement("*")?.matches(&semver::Version::parse("1.0.0")?));
        assert!(parse_version_requirement(">=2.7.0")?.matches(&semver::Version::parse("6.0.3")?));
        assert!(parse_version_requirement("==3.1")?.matches(&semver::Version::parse("3.1.0")?));
        assert!(
            parse_version_requirement(">= 1.0, < 2.0")?.matches(&semver::Version::parse("1.9.0")?)
        );
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

    #[test]
    fn latest_matching_version_handles_prereleases() -> Result<()> {
        let versions = vec![
            "1.0.0-alpha.1".to_string(),
            "1.0.0-beta.1".to_string(),
            "1.0.0".to_string(),
        ];
        let requirement = parse_version_requirement(">=1.0.0-alpha.1,<1.0.0")?;

        assert_eq!(
            select_latest_matching_version(&versions, &requirement)?,
            "1.0.0-beta.1"
        );
        Ok(())
    }

    #[test]
    fn latest_version_skips_non_semver_values() -> Result<()> {
        let versions = vec!["not-semver".to_string(), "1.0.0".to_string()];

        assert_eq!(select_latest_version(&versions)?, "1.0.0");
        Ok(())
    }
}
