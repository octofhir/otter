//! Deterministic `otter.lock` support for Otter package management.
//!
//! This crate owns the active lockfile wire model. The format is
//! TOML-compatible text and intentionally diffable from the first PM slice.
//! It records enough graph and lifecycle metadata for install/run integration,
//! while leaving registry fetch and tarball extraction to later slices. It also
//! includes read-only migration adapters for npm and pnpm lockfiles so projects
//! can be inspected before writing a native `otter.lock`.
//!
//! # Contents
//! - [`LOCKFILE_NAME`] — canonical filename, `otter.lock`.
//! - [`LockfileFormat`] — supported on-disk lockfile formats.
//! - [`Lockfile`] — top-level graph document.
//! - [`LockedPackage`] — one resolved package.
//! - [`ResolvedSource`] — recorded package source.
//! - [`LifecycleMetadata`] — lifecycle scripts and trust state.
//!
//! # Invariants
//! - Package and dependency maps are [`std::collections::BTreeMap`] values for
//!   stable ordering.
//! - [`Lockfile::to_toml_string`] emits byte-stable output for equivalent
//!   graphs.
//! - npm/pnpm adapters are read-only and normalize into the native in-memory
//!   model; Otter still writes only [`LOCKFILE_NAME`].
//! - Lifecycle scripts are recorded, not executed.
//!
//! # See also
//! - [`otter-pm-manifest`](../../otter-pm-manifest/src/lib.rs)
//! - [`otter-pm`](../../otter-pm/src/lib.rs)

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Canonical lockfile filename.
pub const LOCKFILE_NAME: &str = "otter.lock";

/// npm lockfile filename.
pub const PACKAGE_LOCK_JSON: &str = "package-lock.json";

/// npm shrinkwrap filename.
pub const NPM_SHRINKWRAP_JSON: &str = "npm-shrinkwrap.json";

/// pnpm lockfile filename.
pub const PNPM_LOCK_YAML: &str = "pnpm-lock.yaml";

/// Current lockfile schema version.
pub const LOCKFILE_VERSION: u32 = 1;

/// Supported on-disk lockfile formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockfileFormat {
    /// Native Otter TOML-compatible lockfile.
    Otter,
    /// npm `package-lock.json`.
    PackageLock,
    /// npm `npm-shrinkwrap.json`.
    NpmShrinkwrap,
    /// pnpm `pnpm-lock.yaml`.
    Pnpm,
}

impl LockfileFormat {
    /// Filename used by the format.
    #[must_use]
    pub const fn filename(self) -> &'static str {
        match self {
            Self::Otter => LOCKFILE_NAME,
            Self::PackageLock => PACKAGE_LOCK_JSON,
            Self::NpmShrinkwrap => NPM_SHRINKWRAP_JSON,
            Self::Pnpm => PNPM_LOCK_YAML,
        }
    }
}

/// Candidate project lockfiles in read preference order.
#[must_use]
pub fn project_lockfile_candidates(project_root: &Path) -> Vec<(PathBuf, LockfileFormat)> {
    [
        LockfileFormat::Otter,
        LockfileFormat::Pnpm,
        LockfileFormat::NpmShrinkwrap,
        LockfileFormat::PackageLock,
    ]
    .into_iter()
    .map(|format| (project_root.join(format.filename()), format))
    .collect()
}

/// Parse/serialize errors for `otter.lock`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LockfileError {
    /// TOML parsing failed.
    #[error("invalid otter.lock TOML: {0}")]
    Parse(String),
    /// TOML serialization failed.
    #[error("cannot serialize otter.lock: {0}")]
    Serialize(String),
}

/// A deterministic `otter.lock` package graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockfile {
    /// Wire schema version.
    pub lockfile_version: u32,
    /// Resolved packages keyed by stable package id.
    #[serde(default)]
    pub packages: BTreeMap<String, LockedPackage>,
}

impl Default for Lockfile {
    fn default() -> Self {
        Self {
            lockfile_version: LOCKFILE_VERSION,
            packages: BTreeMap::new(),
        }
    }
}

impl Lockfile {
    /// Build an empty v1 lockfile.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse from TOML-compatible `otter.lock` text.
    pub fn parse_toml(text: &str) -> Result<Self, LockfileError> {
        toml::from_str(text).map_err(|err| LockfileError::Parse(err.to_string()))
    }

    /// Parse one lockfile text using an explicit on-disk format.
    pub fn parse_format(format: LockfileFormat, text: &str) -> Result<Self, LockfileError> {
        match format {
            LockfileFormat::Otter => Self::parse_toml(text),
            LockfileFormat::PackageLock | LockfileFormat::NpmShrinkwrap => {
                parse_package_lock_json(text)
            }
            LockfileFormat::Pnpm => parse_pnpm_lock_yaml(text),
        }
    }

    /// Serialize to deterministic TOML-compatible text.
    pub fn to_toml_string(&self) -> Result<String, LockfileError> {
        toml::to_string_pretty(self)
            .map(|mut text| {
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                text
            })
            .map_err(|err| LockfileError::Serialize(err.to_string()))
    }
}

#[derive(Debug, Deserialize)]
struct RawNpmLockfile {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    packages: BTreeMap<String, RawNpmPackage>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawNpmPackage {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    resolved: Option<String>,
    #[serde(default)]
    integrity: Option<String>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    peer_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    link: bool,
}

fn parse_package_lock_json(text: &str) -> Result<Lockfile, LockfileError> {
    let raw: RawNpmLockfile = serde_json::from_str(text)
        .map_err(|err| LockfileError::Parse(format!("invalid package-lock.json: {err}")))?;
    let root = raw.packages.get("");
    let root_name = root
        .and_then(|package| package.name.clone())
        .or(raw.name)
        .unwrap_or_else(|| "root".to_string());
    let root_version = root
        .and_then(|package| package.version.clone())
        .or(raw.version)
        .unwrap_or_else(|| "0.0.0".to_string());
    let mut version_by_name = BTreeMap::<String, String>::new();
    for (path, package) in &raw.packages {
        if path.is_empty() || package.link {
            continue;
        }
        let Some(name) = npm_package_name_from_path(path) else {
            continue;
        };
        if let Some(version) = &package.version {
            version_by_name
                .entry(name)
                .or_insert_with(|| version.clone());
        }
    }
    let mut preferred_ids = BTreeMap::<(String, String), String>::new();
    if let Some(root) = root {
        record_npm_dependency_ids(root, &version_by_name, &mut preferred_ids);
    }
    for package in raw.packages.values() {
        record_npm_dependency_ids(package, &version_by_name, &mut preferred_ids);
    }

    let mut lockfile = Lockfile::new();
    let root_id = workspace_root_id(&root_name);
    let root_deps = root
        .map(|package| npm_dependency_targets(package, &version_by_name, &preferred_ids))
        .unwrap_or_default();
    lockfile.packages.insert(
        root_id,
        LockedPackage {
            name: root_name,
            version: root_version,
            dependencies: root_deps,
            integrity: None,
            resolved: Some(ResolvedSource {
                kind: ResolvedSourceKind::Workspace,
                reference: ".".to_string(),
            }),
            lifecycle: LifecycleMetadata::default(),
        },
    );

    let mut inserted = BTreeSet::new();
    for (path, package) in &raw.packages {
        if path.is_empty() || package.link {
            continue;
        }
        let (Some(name), Some(version)) = (npm_package_name_from_path(path), &package.version)
        else {
            continue;
        };
        let id = preferred_ids
            .get(&(name.clone(), version.clone()))
            .cloned()
            .unwrap_or_else(|| registry_id(&name, version));
        if !inserted.insert(id.clone()) {
            continue;
        }
        lockfile.packages.insert(
            id,
            LockedPackage {
                name: name.clone(),
                version: version.clone(),
                dependencies: npm_dependency_targets(package, &version_by_name, &preferred_ids),
                integrity: package.integrity.clone(),
                resolved: Some(ResolvedSource {
                    kind: ResolvedSourceKind::Registry,
                    reference: package.resolved.clone().unwrap_or_default(),
                }),
                lifecycle: LifecycleMetadata::default(),
            },
        );
    }
    Ok(lockfile)
}

fn record_npm_dependency_ids(
    package: &RawNpmPackage,
    version_by_name: &BTreeMap<String, String>,
    preferred_ids: &mut BTreeMap<(String, String), String>,
) {
    for (name, range) in npm_dependency_specs(package) {
        if let Some(version) = version_by_name.get(name) {
            preferred_ids
                .entry((name.clone(), version.clone()))
                .or_insert_with(|| registry_id(name, range));
        }
    }
}

fn npm_dependency_targets(
    package: &RawNpmPackage,
    version_by_name: &BTreeMap<String, String>,
    preferred_ids: &BTreeMap<(String, String), String>,
) -> BTreeMap<String, String> {
    npm_dependency_specs(package)
        .into_iter()
        .filter_map(|(name, range)| {
            let version = version_by_name.get(name)?;
            let id = preferred_ids
                .get(&(name.clone(), version.clone()))
                .cloned()
                .unwrap_or_else(|| registry_id(name, range));
            Some((name.clone(), id))
        })
        .collect()
}

fn npm_dependency_specs(package: &RawNpmPackage) -> Vec<(&String, &String)> {
    package
        .dependencies
        .iter()
        .chain(package.dev_dependencies.iter())
        .chain(package.optional_dependencies.iter())
        .chain(package.peer_dependencies.iter())
        .collect()
}

fn npm_package_name_from_path(path: &str) -> Option<String> {
    let mut name = None;
    for segment in path.split("node_modules/") {
        let segment = segment.trim_matches('/');
        if !segment.is_empty() {
            name = Some(segment.to_string());
        }
    }
    name
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPnpmLockfile {
    #[serde(default)]
    importers: BTreeMap<String, RawPnpmImporter>,
    #[serde(default)]
    packages: BTreeMap<String, RawPnpmPackage>,
    #[serde(default)]
    snapshots: BTreeMap<String, RawPnpmSnapshot>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPnpmImporter {
    #[serde(default)]
    dependencies: BTreeMap<String, RawPnpmImporterDependency>,
    #[serde(default)]
    dev_dependencies: BTreeMap<String, RawPnpmImporterDependency>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, RawPnpmImporterDependency>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawPnpmImporterDependency {
    Version(String),
    Detailed {
        #[serde(default)]
        specifier: Option<String>,
        #[serde(default)]
        version: Option<String>,
    },
}

#[derive(Debug, Default, Deserialize)]
struct RawPnpmPackage {
    #[serde(default)]
    resolution: RawPnpmResolution,
}

#[derive(Debug, Default, Deserialize)]
struct RawPnpmResolution {
    #[serde(default)]
    integrity: Option<String>,
    #[serde(default)]
    tarball: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawPnpmSnapshot {
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "optionalDependencies")]
    optional_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "peerDependencies")]
    peer_dependencies: BTreeMap<String, String>,
}

fn parse_pnpm_lock_yaml(text: &str) -> Result<Lockfile, LockfileError> {
    let raw: RawPnpmLockfile = serde_yaml::from_str(text)
        .map_err(|err| LockfileError::Parse(format!("invalid pnpm-lock.yaml: {err}")))?;
    let mut lockfile = Lockfile::new();
    let root_id = workspace_root_id("root");
    let root_importer = raw.importers.get(".");
    let mut preferred_ids = BTreeMap::<(String, String), String>::new();
    if let Some(importer) = root_importer {
        for (name, dep) in pnpm_importer_dependencies(importer) {
            if let Some((specifier, version)) = dep.specifier_and_version() {
                preferred_ids.insert(
                    (name.clone(), strip_pnpm_peer_suffix(&version).to_string()),
                    registry_id(name, &specifier),
                );
            }
        }
    }
    for snapshot in raw.snapshots.values() {
        for (name, reference) in pnpm_snapshot_dependencies(snapshot) {
            let version = strip_pnpm_peer_suffix(reference);
            preferred_ids
                .entry((name.clone(), version.to_string()))
                .or_insert_with(|| registry_id(name, version));
        }
    }
    let root_deps = root_importer
        .map(|importer| {
            pnpm_importer_dependencies(importer)
                .into_iter()
                .filter_map(|(name, dep)| {
                    let (_specifier, version) = dep.specifier_and_version()?;
                    let version = strip_pnpm_peer_suffix(&version).to_string();
                    let id = preferred_ids
                        .get(&(name.clone(), version.clone()))
                        .cloned()
                        .unwrap_or_else(|| registry_id(name, &version));
                    Some((name.clone(), id))
                })
                .collect()
        })
        .unwrap_or_default();
    lockfile.packages.insert(
        root_id,
        LockedPackage {
            name: "root".to_string(),
            version: "0.0.0".to_string(),
            dependencies: root_deps,
            integrity: None,
            resolved: Some(ResolvedSource {
                kind: ResolvedSourceKind::Workspace,
                reference: ".".to_string(),
            }),
            lifecycle: LifecycleMetadata::default(),
        },
    );
    for (key, package) in raw.packages {
        let Some((name, version)) = parse_pnpm_package_key(&key) else {
            continue;
        };
        let id = preferred_ids
            .get(&(name.clone(), version.clone()))
            .cloned()
            .unwrap_or_else(|| registry_id(&name, &version));
        let snapshot = raw
            .snapshots
            .get(&key)
            .or_else(|| raw.snapshots.get(key.trim_start_matches('/')));
        let dependencies = snapshot
            .map(|snapshot| pnpm_dependency_targets(snapshot, &preferred_ids))
            .unwrap_or_default();
        lockfile.packages.insert(
            id,
            LockedPackage {
                name,
                version,
                dependencies,
                integrity: package.resolution.integrity,
                resolved: Some(ResolvedSource {
                    kind: ResolvedSourceKind::Registry,
                    reference: package.resolution.tarball.unwrap_or_default(),
                }),
                lifecycle: LifecycleMetadata::default(),
            },
        );
    }
    Ok(lockfile)
}

impl RawPnpmImporterDependency {
    fn specifier_and_version(&self) -> Option<(String, String)> {
        match self {
            Self::Version(version) => Some((version.clone(), version.clone())),
            Self::Detailed { specifier, version } => {
                let version = version.clone()?;
                Some((
                    specifier.clone().unwrap_or_else(|| version.clone()),
                    version,
                ))
            }
        }
    }
}

fn pnpm_importer_dependencies(
    importer: &RawPnpmImporter,
) -> Vec<(&String, &RawPnpmImporterDependency)> {
    importer
        .dependencies
        .iter()
        .chain(importer.dev_dependencies.iter())
        .chain(importer.optional_dependencies.iter())
        .collect()
}

fn pnpm_snapshot_dependencies(snapshot: &RawPnpmSnapshot) -> Vec<(&String, &String)> {
    snapshot
        .dependencies
        .iter()
        .chain(snapshot.optional_dependencies.iter())
        .chain(snapshot.peer_dependencies.iter())
        .collect()
}

fn pnpm_dependency_targets(
    snapshot: &RawPnpmSnapshot,
    preferred_ids: &BTreeMap<(String, String), String>,
) -> BTreeMap<String, String> {
    pnpm_snapshot_dependencies(snapshot)
        .into_iter()
        .map(|(name, reference)| {
            let version = strip_pnpm_peer_suffix(reference).to_string();
            let id = preferred_ids
                .get(&(name.clone(), version.clone()))
                .cloned()
                .unwrap_or_else(|| registry_id(name, &version));
            (name.clone(), id)
        })
        .collect()
}

fn parse_pnpm_package_key(key: &str) -> Option<(String, String)> {
    let key = strip_pnpm_peer_suffix(key.trim_start_matches('/'));
    let at = key.rfind('@')?;
    if at == 0 {
        return None;
    }
    Some((key[..at].to_string(), key[at + 1..].to_string()))
}

fn strip_pnpm_peer_suffix(value: &str) -> &str {
    value.split_once('(').map_or(value, |(version, _)| version)
}

fn workspace_root_id(name: &str) -> String {
    format!("{name}@workspace:.")
}

fn registry_id(name: &str, reference: &str) -> String {
    format!("{name}@npm:{reference}")
}

/// One resolved package entry in `otter.lock`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedPackage {
    /// Package name.
    pub name: String,
    /// Resolved package version or opaque source version.
    pub version: String,
    /// Dependencies keyed by dependency name, value is target package id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, String>,
    /// Integrity string, usually an SRI hash.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integrity: Option<String>,
    /// Resolved package source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<ResolvedSource>,
    /// Lifecycle metadata and trust state.
    #[serde(default)]
    pub lifecycle: LifecycleMetadata,
}

/// Source recorded for a locked package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedSource {
    /// Source kind.
    pub kind: ResolvedSourceKind,
    /// Source reference, for example a registry tarball URL or workspace path.
    pub reference: String,
}

/// Supported source kinds for the minimal lock graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedSourceKind {
    /// npm registry source.
    Registry,
    /// Local workspace package.
    Workspace,
    /// Local file dependency.
    File,
    /// Tarball URL or path.
    Tarball,
}

/// Recorded lifecycle script policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleMetadata {
    /// Lifecycle scripts present in the package manifest.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub scripts: BTreeMap<String, String>,
    /// Trust state for future lifecycle execution.
    pub trust: TrustState,
}

impl Default for LifecycleMetadata {
    fn default() -> Self {
        Self {
            scripts: BTreeMap::new(),
            trust: TrustState::Untrusted,
        }
    }
}

/// Lifecycle script trust state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustState {
    /// Package lifecycle scripts are trusted by policy.
    Trusted,
    /// Package lifecycle scripts are present but not trusted.
    Untrusted,
    /// Lifecycle scripts are disabled for this package.
    Disabled,
    /// Trust state has not been decided yet.
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_lockfile() -> Lockfile {
        let mut packages = BTreeMap::new();
        let mut app_deps = BTreeMap::new();
        app_deps.insert("alpha".to_string(), "alpha@1.0.0".to_string());
        app_deps.insert("zeta".to_string(), "zeta@2.0.0".to_string());
        let mut scripts = BTreeMap::new();
        scripts.insert("postinstall".to_string(), "node setup.js".to_string());
        packages.insert(
            "app@workspace:.".to_string(),
            LockedPackage {
                name: "app".to_string(),
                version: "0.1.0".to_string(),
                dependencies: app_deps,
                integrity: None,
                resolved: Some(ResolvedSource {
                    kind: ResolvedSourceKind::Workspace,
                    reference: ".".to_string(),
                }),
                lifecycle: LifecycleMetadata {
                    scripts,
                    trust: TrustState::Trusted,
                },
            },
        );
        packages.insert(
            "alpha@1.0.0".to_string(),
            LockedPackage {
                name: "alpha".to_string(),
                version: "1.0.0".to_string(),
                dependencies: BTreeMap::new(),
                integrity: Some("sha512-alpha".to_string()),
                resolved: Some(ResolvedSource {
                    kind: ResolvedSourceKind::Registry,
                    reference: "https://registry.npmjs.org/alpha/-/alpha-1.0.0.tgz".to_string(),
                }),
                lifecycle: LifecycleMetadata::default(),
            },
        );
        Lockfile {
            lockfile_version: LOCKFILE_VERSION,
            packages,
        }
    }

    #[test]
    fn lockfile_roundtrips_stably() {
        let lockfile = sample_lockfile();
        let text = lockfile.to_toml_string().unwrap();
        let reparsed = Lockfile::parse_toml(&text).unwrap();
        assert_eq!(lockfile, reparsed);
        assert_eq!(text, reparsed.to_toml_string().unwrap());
        assert!(text.contains("lockfile_version = 1"));
        assert!(text.contains("[packages.\"alpha@1.0.0\"]"));
        assert!(text.contains("[packages.\"app@workspace:.\".dependencies]"));
    }

    #[test]
    fn lockfile_name_is_otter_dot_lock() {
        assert_eq!(LOCKFILE_NAME, "otter.lock");
    }

    #[test]
    fn package_lock_json_imports_root_and_package_edges() {
        let lockfile = Lockfile::parse_format(
            LockfileFormat::PackageLock,
            r#"{
  "name": "app",
  "version": "0.1.0",
  "lockfileVersion": 3,
  "packages": {
    "": {
      "name": "app",
      "version": "0.1.0",
      "dependencies": {
        "left-pad": "^1.3.0"
      }
    },
    "node_modules/left-pad": {
      "version": "1.3.0",
      "resolved": "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz",
      "integrity": "sha512-left",
      "dependencies": {
        "repeat-string": "^1.6.1"
      }
    },
    "node_modules/repeat-string": {
      "version": "1.6.1",
      "resolved": "https://registry.npmjs.org/repeat-string/-/repeat-string-1.6.1.tgz",
      "integrity": "sha512-repeat"
    }
  }
}"#,
        )
        .unwrap();

        let root = lockfile.packages.get("app@workspace:.").unwrap();
        assert_eq!(
            root.dependencies.get("left-pad").map(String::as_str),
            Some("left-pad@npm:^1.3.0")
        );
        let left_pad = lockfile.packages.get("left-pad@npm:^1.3.0").unwrap();
        assert_eq!(left_pad.version, "1.3.0");
        assert_eq!(left_pad.integrity.as_deref(), Some("sha512-left"));
        assert_eq!(
            left_pad
                .dependencies
                .get("repeat-string")
                .map(String::as_str),
            Some("repeat-string@npm:^1.6.1")
        );
    }

    #[test]
    fn pnpm_lock_yaml_imports_root_and_snapshot_edges() {
        let lockfile = Lockfile::parse_format(
            LockfileFormat::Pnpm,
            r#"
lockfileVersion: '9.0'
importers:
  .:
    dependencies:
      left-pad:
        specifier: ^1.3.0
        version: 1.3.0
packages:
  left-pad@1.3.0:
    resolution:
      integrity: sha512-left
      tarball: https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz
  repeat-string@1.6.1:
    resolution:
      integrity: sha512-repeat
snapshots:
  left-pad@1.3.0:
    dependencies:
      repeat-string: 1.6.1
  repeat-string@1.6.1: {}
"#,
        )
        .unwrap();

        let root = lockfile.packages.get("root@workspace:.").unwrap();
        assert_eq!(
            root.dependencies.get("left-pad").map(String::as_str),
            Some("left-pad@npm:^1.3.0")
        );
        let left_pad = lockfile.packages.get("left-pad@npm:^1.3.0").unwrap();
        assert_eq!(left_pad.version, "1.3.0");
        assert_eq!(left_pad.integrity.as_deref(), Some("sha512-left"));
        assert_eq!(
            left_pad
                .dependencies
                .get("repeat-string")
                .map(String::as_str),
            Some("repeat-string@npm:1.6.1")
        );
    }
}
