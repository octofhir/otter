//! Deterministic `otter-lock` support for Otter package management.
//!
//! This crate owns the active lockfile wire model. The format is
//! TOML-compatible text and intentionally diffable from the first PM slice.
//! It records enough graph and lifecycle metadata for install/run integration,
//! while leaving registry fetch and tarball extraction to later slices.
//!
//! # Contents
//! - [`LOCKFILE_NAME`] — canonical filename, `otter-lock`.
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
//! - Lifecycle scripts are recorded, not executed.
//!
//! # See also
//! - [`otter-pm-manifest`](../../otter-pm-manifest/src/lib.rs)
//! - [`otter-pm`](../../otter-pm/src/lib.rs)

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Canonical lockfile filename.
pub const LOCKFILE_NAME: &str = "otter-lock";

/// Current lockfile schema version.
pub const LOCKFILE_VERSION: u32 = 1;

/// Parse/serialize errors for `otter-lock`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LockfileError {
    /// TOML parsing failed.
    #[error("invalid otter-lock TOML: {0}")]
    Parse(String),
    /// TOML serialization failed.
    #[error("cannot serialize otter-lock: {0}")]
    Serialize(String),
}

/// A deterministic `otter-lock` package graph.
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

    /// Parse from TOML-compatible `otter-lock` text.
    pub fn parse_toml(text: &str) -> Result<Self, LockfileError> {
        toml::from_str(text).map_err(|err| LockfileError::Parse(err.to_string()))
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

/// One resolved package entry in `otter-lock`.
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
    fn lockfile_name_is_otter_lock() {
        assert_eq!(LOCKFILE_NAME, "otter-lock");
    }
}
