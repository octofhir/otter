//! Package and workspace manifest support for Otter package management.
//!
//! This crate owns the active-stack representation of `package.json`,
//! `package.json#workspaces`, and `pnpm-workspace.yaml`. It performs parsing,
//! deterministic serialization, and workspace package discovery. It does not
//! resolve registry metadata, mutate installs, or execute lifecycle scripts.
//!
//! # Contents
//! - [`PackageManifest`] — typed `package.json` surface.
//! - [`DependencySet`] — dependency buckets from npm manifests.
//! - [`PackageBinManifest`] — `package.json#bin` representation.
//! - [`WorkspacePackage`] — discovered workspace package.
//! - [`discover_workspaces`] — combined npm + pnpm workspace discovery.
//!
//! # Invariants
//! - Observable manifest maps use [`std::collections::BTreeMap`] so serialized
//!   output is stable across platforms and process runs.
//! - Workspace discovery returns packages in deterministic path order.
//! - This crate is filesystem/manifest only; runtime capability checks apply
//!   when user code or runtime APIs consume the package graph.
//!
//! # See also
//! - [`otter-pm-lockfile`](../../otter-pm-lockfile/src/lib.rs)
//! - [`otter-pm`](../../otter-pm/src/lib.rs)

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

/// `package.json` filename.
pub const PACKAGE_JSON: &str = "package.json";

/// `pnpm-workspace.yaml` filename.
pub const PNPM_WORKSPACE_YAML: &str = "pnpm-workspace.yaml";

/// Parse/discovery error for package manifests.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ManifestError {
    /// Filesystem read/write operation failed.
    #[error("manifest I/O failed for `{path}`: {message}")]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying error message.
        message: String,
    },
    /// `package.json` JSON parse failed.
    #[error("invalid package.json at `{path}`: {message}")]
    Json {
        /// Path involved in the failed parse.
        path: PathBuf,
        /// Underlying error message.
        message: String,
    },
    /// `pnpm-workspace.yaml` YAML parse failed.
    #[error("invalid pnpm-workspace.yaml at `{path}`: {message}")]
    Yaml {
        /// Path involved in the failed parse.
        path: PathBuf,
        /// Underlying error message.
        message: String,
    },
    /// Workspace glob could not be compiled.
    #[error("invalid workspace pattern `{pattern}`: {message}")]
    Glob {
        /// Raw pattern.
        pattern: String,
        /// Underlying error message.
        message: String,
    },
    /// Path stripping failed while making workspace paths relative.
    #[error("cannot make `{path}` relative to `{base}`")]
    RelativePath {
        /// Base directory.
        base: PathBuf,
        /// Child path.
        path: PathBuf,
    },
}

/// Manifest validation diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestDiagnostic {
    /// Stable diagnostic code.
    pub code: String,
    /// Human-readable message.
    pub message: String,
}

/// Dependency bucket from a package manifest.
pub type DependencySet = BTreeMap<String, String>;

/// JavaScript package module mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageType {
    /// ECMAScript module package scope.
    Module,
    /// CommonJS package scope.
    #[serde(rename = "commonjs")]
    CommonJs,
}

/// `package.json#bin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PackageBinManifest {
    /// Single executable path. The package name supplies the binary name.
    Path(String),
    /// Multiple executable names to paths.
    Map(BTreeMap<String, String>),
}

/// Workspace patterns from `package.json#workspaces`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PackageJsonWorkspaces {
    /// npm shorthand: `"workspaces": ["packages/*"]`.
    Patterns(Vec<String>),
    /// npm object form: `"workspaces": { "packages": ["packages/*"] }`.
    Object {
        /// Package include/exclude patterns.
        packages: Vec<String>,
    },
}

impl PackageJsonWorkspaces {
    fn patterns(&self) -> &[String] {
        match self {
            Self::Patterns(patterns) | Self::Object { packages: patterns } => patterns,
        }
    }
}

/// Typed `package.json` representation for the active package manager.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageManifest {
    /// Package name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Package version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Package module mode.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub package_type: Option<PackageType>,
    /// Legacy package entrypoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main: Option<String>,
    /// ESM-oriented package entrypoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    /// Package exports map/string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exports: Option<serde_json::Value>,
    /// Package imports map.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imports: Option<serde_json::Value>,
    /// Runtime dependency ranges.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: DependencySet,
    /// Development dependency ranges.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dev_dependencies: DependencySet,
    /// Peer dependency ranges.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub peer_dependencies: DependencySet,
    /// Optional dependency ranges.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub optional_dependencies: DependencySet,
    /// Package scripts. Recorded by PM; execution policy belongs to later
    /// lifecycle/capability slices.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub scripts: BTreeMap<String, String>,
    /// Package binaries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bin: Option<PackageBinManifest>,
    /// Workspace patterns in npm format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspaces: Option<PackageJsonWorkspaces>,
}

impl PackageManifest {
    /// Parse a manifest from JSON text.
    pub fn parse_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// Read `package.json` from a directory.
    pub async fn read_from_dir(root: impl AsRef<Path>) -> Result<Self, ManifestError> {
        let path = root.as_ref().join(PACKAGE_JSON);
        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|err| ManifestError::Io {
                path: path.clone(),
                message: err.to_string(),
            })?;
        Self::parse_json(&text).map_err(|err| ManifestError::Json {
            path,
            message: err.to_string(),
        })
    }

    /// Write `package.json` into a directory using stable JSON formatting.
    pub async fn write_to_dir(&self, root: impl AsRef<Path>) -> Result<(), ManifestError> {
        let path = root.as_ref().join(PACKAGE_JSON);
        let text = self.to_stable_json().map_err(|err| ManifestError::Json {
            path: path.clone(),
            message: err.to_string(),
        })?;
        tokio::fs::write(&path, text)
            .await
            .map_err(|err| ManifestError::Io {
                path,
                message: err.to_string(),
            })
    }

    /// Serialize as stable, pretty JSON.
    pub fn to_stable_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self).map(|mut text| {
            text.push('\n');
            text
        })
    }

    /// Return non-fatal validation diagnostics for common manifest mistakes.
    #[must_use]
    pub fn validate(&self) -> Vec<ManifestDiagnostic> {
        let mut diagnostics = Vec::new();
        if matches!(self.name.as_deref(), Some("")) {
            diagnostics.push(ManifestDiagnostic {
                code: "PM_MANIFEST_EMPTY_NAME".to_string(),
                message: "package name must not be empty when present".to_string(),
            });
        }
        if matches!(self.version.as_deref(), Some("")) {
            diagnostics.push(ManifestDiagnostic {
                code: "PM_MANIFEST_EMPTY_VERSION".to_string(),
                message: "package version must not be empty when present".to_string(),
            });
        }
        for (bucket_name, bucket) in self.dependency_buckets() {
            for (name, range) in bucket {
                if name.trim().is_empty() {
                    diagnostics.push(ManifestDiagnostic {
                        code: "PM_MANIFEST_EMPTY_DEPENDENCY_NAME".to_string(),
                        message: format!("{bucket_name} contains an empty dependency name"),
                    });
                }
                if range.trim().is_empty() {
                    diagnostics.push(ManifestDiagnostic {
                        code: "PM_MANIFEST_EMPTY_DEPENDENCY_RANGE".to_string(),
                        message: format!("{bucket_name}.{name} must not use an empty range"),
                    });
                }
            }
        }
        diagnostics
    }

    /// Borrow all dependency buckets in deterministic bucket order.
    #[must_use]
    pub fn dependency_buckets(&self) -> [(&'static str, &DependencySet); 4] {
        [
            ("dependencies", &self.dependencies),
            ("devDependencies", &self.dev_dependencies),
            ("peerDependencies", &self.peer_dependencies),
            ("optionalDependencies", &self.optional_dependencies),
        ]
    }
}

/// `pnpm-workspace.yaml` representation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PnpmWorkspace {
    /// Workspace include/exclude patterns.
    #[serde(default)]
    pub packages: Vec<String>,
}

impl PnpmWorkspace {
    /// Parse workspace config from YAML text.
    pub fn parse_yaml(text: &str) -> Result<Self, serde_yaml::Error> {
        serde_yaml::from_str(text)
    }

    /// Read `pnpm-workspace.yaml` from a directory.
    pub async fn read_from_dir(root: impl AsRef<Path>) -> Result<Option<Self>, ManifestError> {
        let path = root.as_ref().join(PNPM_WORKSPACE_YAML);
        if !tokio::fs::try_exists(&path)
            .await
            .map_err(|err| ManifestError::Io {
                path: path.clone(),
                message: err.to_string(),
            })?
        {
            return Ok(None);
        }
        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|err| ManifestError::Io {
                path: path.clone(),
                message: err.to_string(),
            })?;
        Self::parse_yaml(&text)
            .map(Some)
            .map_err(|err| ManifestError::Yaml {
                path,
                message: err.to_string(),
            })
    }
}

/// One package discovered from workspace patterns.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspacePackage {
    /// Absolute package root.
    pub root: PathBuf,
    /// Path relative to the workspace root, using platform path separators.
    pub relative_root: PathBuf,
    /// Parsed package manifest.
    pub manifest: PackageManifest,
}

/// Discover workspace packages from both `package.json#workspaces` and
/// `pnpm-workspace.yaml`.
pub async fn discover_workspaces(
    root: impl AsRef<Path>,
) -> Result<Vec<WorkspacePackage>, ManifestError> {
    let root = root.as_ref();
    let mut patterns = Vec::new();
    let root_manifest_path = root.join(PACKAGE_JSON);
    if tokio::fs::try_exists(&root_manifest_path)
        .await
        .map_err(|err| ManifestError::Io {
            path: root_manifest_path.clone(),
            message: err.to_string(),
        })?
    {
        let manifest = PackageManifest::read_from_dir(root).await?;
        if let Some(workspaces) = manifest.workspaces {
            patterns.extend(workspaces.patterns().iter().cloned());
        }
    }
    if let Some(pnpm) = PnpmWorkspace::read_from_dir(root).await? {
        patterns.extend(pnpm.packages);
    }
    discover_workspace_patterns(root, &patterns).await
}

/// Discover workspace packages from explicit glob patterns.
pub async fn discover_workspace_patterns(
    root: impl AsRef<Path>,
    patterns: &[String],
) -> Result<Vec<WorkspacePackage>, ManifestError> {
    if patterns.is_empty() {
        return Ok(Vec::new());
    }
    let root = root.as_ref();
    let matcher = WorkspaceMatcher::new(patterns)?;
    let manifests = collect_package_json_paths(root).await?;
    let mut packages = Vec::new();
    for manifest_path in manifests {
        let package_root = manifest_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.to_path_buf());
        if package_root == root {
            continue;
        }
        let relative = package_root
            .strip_prefix(root)
            .map_err(|_| ManifestError::RelativePath {
                base: root.to_path_buf(),
                path: package_root.clone(),
            })?
            .to_path_buf();
        if matcher.matches(&relative) {
            packages.push(WorkspacePackage {
                manifest: PackageManifest::read_from_dir(&package_root).await?,
                root: package_root,
                relative_root: relative,
            });
        }
    }
    packages.sort_by(|a, b| a.relative_root.cmp(&b.relative_root));
    Ok(packages)
}

struct WorkspaceMatcher {
    include: GlobSet,
    exclude: GlobSet,
}

impl WorkspaceMatcher {
    fn new(patterns: &[String]) -> Result<Self, ManifestError> {
        let mut include = GlobSetBuilder::new();
        let mut exclude = GlobSetBuilder::new();
        let mut has_include = false;
        for raw in patterns {
            let (is_exclude, pattern) = raw
                .strip_prefix('!')
                .map_or((false, raw.as_str()), |pattern| (true, pattern));
            let glob = Glob::new(pattern).map_err(|err| ManifestError::Glob {
                pattern: raw.clone(),
                message: err.to_string(),
            })?;
            if is_exclude {
                exclude.add(glob);
            } else {
                has_include = true;
                include.add(glob);
            }
        }
        if !has_include {
            include.add(Glob::new("**").map_err(|err| ManifestError::Glob {
                pattern: "**".to_string(),
                message: err.to_string(),
            })?);
        }
        Ok(Self {
            include: include.build().map_err(|err| ManifestError::Glob {
                pattern: "<include-set>".to_string(),
                message: err.to_string(),
            })?,
            exclude: exclude.build().map_err(|err| ManifestError::Glob {
                pattern: "<exclude-set>".to_string(),
                message: err.to_string(),
            })?,
        })
    }

    fn matches(&self, relative: &Path) -> bool {
        self.include.is_match(relative) && !self.exclude.is_match(relative)
    }
}

async fn collect_package_json_paths(root: &Path) -> Result<Vec<PathBuf>, ManifestError> {
    let mut output = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&dir)
            .await
            .map_err(|err| ManifestError::Io {
                path: dir.clone(),
                message: err.to_string(),
            })?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|err| ManifestError::Io {
                path: dir.clone(),
                message: err.to_string(),
            })?
        {
            let path = entry.path();
            let file_type = entry.file_type().await.map_err(|err| ManifestError::Io {
                path: path.clone(),
                message: err.to_string(),
            })?;
            if file_type.is_dir() {
                let name = entry.file_name();
                if matches!(
                    name.to_str(),
                    Some("node_modules" | ".git" | "target" | ".turbo")
                ) {
                    continue;
                }
                stack.push(path);
            } else if file_type.is_file() && entry.file_name() == PACKAGE_JSON {
                output.push(path);
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(path, text).await.unwrap();
    }

    #[test]
    fn package_json_roundtrips_deterministically() {
        let text = r##"{
          "name": "@scope/app",
          "version": "1.2.3",
          "type": "module",
          "main": "./dist/index.cjs",
          "module": "./dist/index.mjs",
          "exports": {
            ".": {
              "import": "./dist/index.mjs",
              "default": "./dist/index.js"
            }
          },
          "imports": {
            "#internal": "./src/internal.ts"
          },
          "dependencies": {
            "zeta": "^1.0.0",
            "alpha": "^2.0.0"
          },
          "devDependencies": {
            "typescript": "^5.0.0"
          },
          "peerDependencies": {
            "react": "^19.0.0"
          },
          "optionalDependencies": {
            "fsevents": "^2.0.0"
          },
          "scripts": {
            "test": "otter test",
            "build": "tsc"
          },
          "bin": {
            "app": "./bin/app.js"
          },
          "workspaces": {
            "packages": ["packages/*"]
          }
        }"##;
        let manifest = PackageManifest::parse_json(text).unwrap();
        assert_eq!(manifest.name.as_deref(), Some("@scope/app"));
        assert_eq!(manifest.package_type, Some(PackageType::Module));
        assert_eq!(
            manifest.dependencies.keys().collect::<Vec<_>>(),
            ["alpha", "zeta"]
        );
        let stable = manifest.to_stable_json().unwrap();
        let reparsed = PackageManifest::parse_json(&stable).unwrap();
        assert_eq!(manifest, reparsed);
        assert_eq!(stable, reparsed.to_stable_json().unwrap());
    }

    #[test]
    fn supports_string_bin_and_workspace_array() {
        let manifest = PackageManifest::parse_json(
            r#"{
              "name": "tool",
              "bin": "./cli.js",
              "workspaces": ["packages/*", "!packages/skip"]
            }"#,
        )
        .unwrap();
        assert_eq!(
            manifest.bin,
            Some(PackageBinManifest::Path("./cli.js".to_string()))
        );
        assert_eq!(
            manifest.workspaces.unwrap().patterns(),
            ["packages/*", "!packages/skip"]
        );
    }

    #[tokio::test]
    async fn discovers_package_json_workspaces_in_stable_order() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join("package.json"),
            r#"{"workspaces":["packages/*","!packages/skip"]}"#,
        )
        .await;
        write(
            &tmp.path().join("packages/b/package.json"),
            r#"{"name":"b","version":"1.0.0"}"#,
        )
        .await;
        write(
            &tmp.path().join("packages/a/package.json"),
            r#"{"name":"a","version":"1.0.0"}"#,
        )
        .await;
        write(
            &tmp.path().join("packages/skip/package.json"),
            r#"{"name":"skip","version":"1.0.0"}"#,
        )
        .await;

        let workspaces = discover_workspaces(tmp.path()).await.unwrap();
        let names = workspaces
            .iter()
            .map(|pkg| pkg.manifest.name.as_deref().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["a", "b"]);
    }

    #[tokio::test]
    async fn discovers_pnpm_workspace_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("package.json"), r#"{"name":"root"}"#).await;
        write(
            &tmp.path().join("pnpm-workspace.yaml"),
            "packages:\n  - apps/*\n  - packages/*\n  - '!packages/private'\n",
        )
        .await;
        write(
            &tmp.path().join("apps/web/package.json"),
            r#"{"name":"web"}"#,
        )
        .await;
        write(
            &tmp.path().join("packages/lib/package.json"),
            r#"{"name":"lib"}"#,
        )
        .await;
        write(
            &tmp.path().join("packages/private/package.json"),
            r#"{"name":"private"}"#,
        )
        .await;

        let workspaces = discover_workspaces(tmp.path()).await.unwrap();
        let rels = workspaces
            .iter()
            .map(|pkg| pkg.relative_root.to_string_lossy().replace('\\', "/"))
            .collect::<Vec<_>>();
        assert_eq!(rels, ["apps/web", "packages/lib"]);
    }

    #[test]
    fn validation_reports_empty_dependency_range() {
        let manifest = PackageManifest::parse_json(r#"{"dependencies":{"left-pad":""}}"#).unwrap();
        let diagnostics = manifest.validate();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "PM_MANIFEST_EMPTY_DEPENDENCY_RANGE");
    }
}
