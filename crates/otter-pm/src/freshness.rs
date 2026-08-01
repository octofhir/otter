//! Pre-run check that the installed tree still matches the manifest.
//!
//! A dependency added to `package.json` but never installed surfaces as
//! whatever the program does when its import fails — usually a stack trace
//! about a module, or a shell reporting a missing command. This check runs
//! before the program does and names the actual cause.
//!
//! It walks the manifest's direct dependencies against `node_modules` and
//! parses no lockfile, so it works the same whichever tool installed the tree.
//!
//! # Contents
//! - [`DependencyState`] — the answer: fresh, stale, or undetermined.
//! - [`StaleDependency`] — one dependency that does not match.
//! - [`inspect_installed_dependencies`] — run the check.
//!
//! # Invariants
//! - Never false-warn. Every uncertain case — a specifier that is not a plain
//!   semver range, an unreadable manifest, an unparseable installed version, a
//!   layout this check does not model — is silently dropped rather than
//!   reported. A missed warning costs a confusing error later; a wrong one
//!   costs trust in every warning after it.
//! - A missing optional dependency is not staleness: not installing it is
//!   exactly what optional means.
//! - The check reads; it never installs, writes, or repairs.
//!
//! # See also
//! - [`otter_pm_manifest::VerifyDependencies`] for what a caller does with a
//!   stale answer.

use std::path::Path;

use otter_pm_manifest::{PACKAGE_JSON, PackageManifest};

/// Whether the installed tree still matches the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyState {
    /// Every direct dependency this check can judge is installed and in range.
    Fresh,
    /// Direct dependencies are missing or out of range.
    Stale(Vec<StaleDependency>),
    /// The project could not be judged, and nothing should be reported.
    Undetermined,
}

/// One direct dependency that does not match the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleDependency {
    /// Dependency name.
    pub name: String,
    /// Declared range.
    pub range: String,
    /// Why it does not match.
    pub reason: StaleReason,
}

/// Why a direct dependency does not match the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaleReason {
    /// Nothing is installed under that name.
    Missing,
    /// The installed version falls outside the declared range.
    OutOfRange {
        /// Version found in `node_modules`.
        installed: String,
    },
}

/// Check whether `project_root`'s installed tree matches its manifest.
pub async fn inspect_installed_dependencies(project_root: &Path) -> DependencyState {
    let Ok(manifest) = PackageManifest::read_from_dir(project_root).await else {
        return DependencyState::Undetermined;
    };
    // A project whose modules are resolved from something other than a
    // directory tree cannot be judged by looking at one.
    if tokio::fs::try_exists(project_root.join(".pnp.cjs"))
        .await
        .unwrap_or(false)
        || tokio::fs::try_exists(project_root.join(".pnp.data.json"))
            .await
            .unwrap_or(false)
    {
        return DependencyState::Undetermined;
    }

    let required = manifest
        .dependencies
        .iter()
        .chain(manifest.dev_dependencies.iter())
        .filter(|(_, range)| is_plain_semver_range(range))
        .collect::<Vec<_>>();
    if required.is_empty() {
        return DependencyState::Fresh;
    }

    let node_modules = project_root.join("node_modules");
    let mut stale = Vec::new();
    for (name, range) in required {
        let installed = installed_version(&node_modules, name).await;
        match installed {
            InstalledVersion::Absent => stale.push(StaleDependency {
                name: name.clone(),
                range: range.clone(),
                reason: StaleReason::Missing,
            }),
            InstalledVersion::Unreadable => {}
            InstalledVersion::Present(version) => {
                if version_satisfies(&version, range) == Satisfaction::No {
                    stale.push(StaleDependency {
                        name: name.clone(),
                        range: range.clone(),
                        reason: StaleReason::OutOfRange { installed: version },
                    });
                }
            }
        }
    }
    if stale.is_empty() {
        DependencyState::Fresh
    } else {
        DependencyState::Stale(stale)
    }
}

enum InstalledVersion {
    /// No package directory under that name.
    Absent,
    /// A package directory exists but its version could not be read.
    Unreadable,
    /// The installed version.
    Present(String),
}

async fn installed_version(node_modules: &Path, name: &str) -> InstalledVersion {
    let root = name
        .split('/')
        .fold(node_modules.to_path_buf(), |path, part| path.join(part));
    if !tokio::fs::try_exists(root.join(PACKAGE_JSON))
        .await
        .unwrap_or(false)
    {
        return InstalledVersion::Absent;
    }
    match PackageManifest::read_from_dir(&root).await {
        Ok(manifest) => manifest
            .version
            .map_or(InstalledVersion::Unreadable, InstalledVersion::Present),
        Err(_) => InstalledVersion::Unreadable,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Satisfaction {
    Yes,
    No,
    /// Neither side parsed; the pair says nothing either way.
    Unknown,
}

fn version_satisfies(version: &str, range: &str) -> Satisfaction {
    let Ok(version) = semver::Version::parse(version) else {
        return Satisfaction::Unknown;
    };
    let Some(req) = normalized_range(range) else {
        return Satisfaction::Unknown;
    };
    if req.matches(&version) {
        Satisfaction::Yes
    } else {
        Satisfaction::No
    }
}

fn normalized_range(range: &str) -> Option<semver::VersionReq> {
    let trimmed = range.trim();
    let normalized = match trimmed {
        "*" | "x" | "latest" => "*".to_string(),
        value if value.starts_with('^') || value.starts_with('~') => value.to_string(),
        value if value.chars().next().is_some_and(|c| c.is_ascii_digit()) => format!("={value}"),
        value => value.to_string(),
    };
    semver::VersionReq::parse(&normalized).ok()
}

/// `true` when a specifier is a plain semver range rather than a protocol,
/// alias, dist-tag, or URL that this check cannot judge from a version alone.
fn is_plain_semver_range(spec: &str) -> bool {
    let spec = spec.trim();
    if spec.is_empty() || spec.contains(':') || spec.contains('/') {
        return false;
    }
    if spec == "*" || spec == "x" {
        return true;
    }
    // A dist-tag is a bare word; a range always starts with a digit or one of
    // the comparator characters.
    spec.starts_with(|c: char| c.is_ascii_digit()) || spec.starts_with(['^', '~', '>', '<', '='])
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn write(path: &Path, text: &str) {
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(path, text).await.unwrap();
    }

    async fn install(root: &Path, name: &str, version: &str) {
        write(
            &root.join("node_modules").join(name).join(PACKAGE_JSON),
            &format!(r#"{{"name":"{name}","version":"{version}"}}"#),
        )
        .await;
    }

    #[tokio::test]
    async fn a_project_without_a_manifest_is_undetermined() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            inspect_installed_dependencies(tmp.path()).await,
            DependencyState::Undetermined
        );
    }

    #[tokio::test]
    async fn a_matching_tree_is_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join(PACKAGE_JSON),
            r#"{"name":"app","dependencies":{"left-pad":"^1.3.0"},"devDependencies":{"@scope/tool":"~2.0.0"}}"#,
        )
        .await;
        install(tmp.path(), "left-pad", "1.3.1").await;
        install(tmp.path(), "@scope/tool", "2.0.4").await;
        assert_eq!(
            inspect_installed_dependencies(tmp.path()).await,
            DependencyState::Fresh
        );
    }

    #[tokio::test]
    async fn a_missing_dependency_is_stale() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join(PACKAGE_JSON),
            r#"{"name":"app","dependencies":{"left-pad":"^1.3.0"}}"#,
        )
        .await;
        assert_eq!(
            inspect_installed_dependencies(tmp.path()).await,
            DependencyState::Stale(vec![StaleDependency {
                name: "left-pad".to_string(),
                range: "^1.3.0".to_string(),
                reason: StaleReason::Missing,
            }])
        );
    }

    #[tokio::test]
    async fn an_installed_version_outside_the_range_is_stale() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join(PACKAGE_JSON),
            r#"{"name":"app","dependencies":{"left-pad":"^2.0.0"}}"#,
        )
        .await;
        install(tmp.path(), "left-pad", "1.3.1").await;
        assert_eq!(
            inspect_installed_dependencies(tmp.path()).await,
            DependencyState::Stale(vec![StaleDependency {
                name: "left-pad".to_string(),
                range: "^2.0.0".to_string(),
                reason: StaleReason::OutOfRange {
                    installed: "1.3.1".to_string()
                },
            }])
        );
    }

    #[tokio::test]
    async fn optional_and_peer_dependencies_are_not_judged() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join(PACKAGE_JSON),
            r#"{
              "name": "app",
              "optionalDependencies": { "fsevents": "^2.0.0" },
              "peerDependencies": { "react": "^19.0.0" }
            }"#,
        )
        .await;
        assert_eq!(
            inspect_installed_dependencies(tmp.path()).await,
            DependencyState::Fresh
        );
    }

    #[tokio::test]
    async fn non_semver_specifiers_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join(PACKAGE_JSON),
            r#"{
              "name": "app",
              "dependencies": {
                "member": "workspace:*",
                "local": "file:../local",
                "aliased": "npm:left-pad@^1",
                "tagged": "latest",
                "forked": "github:acme/left-pad"
              }
            }"#,
        )
        .await;
        assert_eq!(
            inspect_installed_dependencies(tmp.path()).await,
            DependencyState::Fresh
        );
    }

    #[tokio::test]
    async fn a_plug_and_play_project_is_undetermined() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join(PACKAGE_JSON),
            r#"{"name":"app","dependencies":{"left-pad":"^1.3.0"}}"#,
        )
        .await;
        write(&tmp.path().join(".pnp.cjs"), "// resolver").await;
        assert_eq!(
            inspect_installed_dependencies(tmp.path()).await,
            DependencyState::Undetermined
        );
    }

    #[tokio::test]
    async fn an_unreadable_installed_version_is_not_reported() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join(PACKAGE_JSON),
            r#"{"name":"app","dependencies":{"left-pad":"^1.3.0"}}"#,
        )
        .await;
        write(
            &tmp.path()
                .join("node_modules")
                .join("left-pad")
                .join(PACKAGE_JSON),
            r#"{"name":"left-pad"}"#,
        )
        .await;
        assert_eq!(
            inspect_installed_dependencies(tmp.path()).await,
            DependencyState::Fresh
        );
    }
}
