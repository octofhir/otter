//! `pnpm-workspace.yaml` / `otter-workspace.yaml` parsing and
//! workspace-root package glob expansion.
//!
//! Precedence for `packages:` patterns:
//! 1. `otter-workspace.yaml` (if present — Otter-native file)
//! 2. `pnpm-workspace.yaml`
//! 3. `<project>/package.json#workspaces` (npm / yarn / bun shape)

use crate::Error;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Parsed workspace-yaml configuration. Fields mirror the subset of
/// pnpm's schema Otter understands today; unknown keys are ignored.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceConfig {
    /// Package glob patterns (relative to the project dir).
    #[serde(default)]
    pub packages: Vec<String>,

    /// Default unnamed catalog.
    #[serde(default)]
    pub catalog: BTreeMap<String, String>,

    /// Named catalogs.
    #[serde(default)]
    pub catalogs: BTreeMap<String, BTreeMap<String, String>>,

    /// `"isolated"` / `"hoisted"` / `"pnp"`. Honored by the linker
    /// (`pnp` rejected with a clear error for now).
    #[serde(default)]
    pub node_linker: Option<String>,

    /// Global virtual store toggle — if None, the linker picks its
    /// own default (currently: per-project store for compatibility).
    #[serde(default)]
    pub enable_global_virtual_store: Option<bool>,

    /// `"auto"` / `"hardlink"` / `"copy"` / `"clone"` / `"clone-or-copy"`.
    #[serde(default)]
    pub package_import_method: Option<String>,

    /// Path to the virtual store dir (default: `node_modules/.otter`).
    #[serde(default)]
    pub virtual_store_dir: Option<String>,

    /// Shamefully-hoist all packages to the root `node_modules/`.
    #[serde(default)]
    pub shamefully_hoist: Option<bool>,

    /// Master switch for `node_modules/.otter/node_modules/` hidden
    /// tree. Default true.
    #[serde(default)]
    pub hoist: Option<bool>,

    /// Globs hoisted into the hidden tree.
    #[serde(default)]
    pub hoist_pattern: Option<Vec<String>>,

    /// Globs hoisted into the top-level `node_modules/`.
    #[serde(default)]
    pub public_hoist_pattern: Option<Vec<String>>,

    /// CAS root override (default: `~/.cache/otter-pm-store/`).
    #[serde(default)]
    pub store_dir: Option<String>,

    /// Whether to write a lockfile at all (default: true).
    #[serde(default)]
    pub lockfile: Option<bool>,

    /// Whether to prefer the on-disk lockfile over re-resolving (default: true).
    #[serde(default)]
    pub prefer_frozen_lockfile: Option<bool>,

    /// Write a branch-suffixed lockfile to reduce merge conflicts.
    #[serde(default)]
    pub git_branch_lockfile: Option<bool>,
}

impl WorkspaceConfig {
    /// Load the workspace yaml for `project_dir`. Tries
    /// `otter-workspace.yaml` first, then `pnpm-workspace.yaml`. A
    /// missing file is not an error — returns `Ok(Default::default())`.
    pub fn load(project_dir: &Path) -> Result<Self, Error> {
        for name in ["otter-workspace.yaml", "pnpm-workspace.yaml"] {
            let p = project_dir.join(name);
            if p.is_file() {
                let bytes = std::fs::read(&p).map_err(|e| Error::Io(p.clone(), e.to_string()))?;
                let cfg: Self = serde_yaml::from_slice(&bytes)
                    .map_err(|e| Error::YamlParse(p.clone(), e.to_string()))?;
                return Ok(cfg);
            }
        }
        Ok(Self::default())
    }
}

/// Raw workspace-yaml as a string→yaml map, for tooling that needs
/// access to fields not yet represented in the typed [`WorkspaceConfig`]
/// (e.g. forward-compat settings round-tripping).
pub fn load_raw(project_dir: &Path) -> Result<BTreeMap<String, serde_yaml::Value>, Error> {
    for name in ["otter-workspace.yaml", "pnpm-workspace.yaml"] {
        let p = project_dir.join(name);
        if p.is_file() {
            let bytes = std::fs::read(&p).map_err(|e| Error::Io(p.clone(), e.to_string()))?;
            let raw: BTreeMap<String, serde_yaml::Value> = serde_yaml::from_slice(&bytes)
                .map_err(|e| Error::YamlParse(p.clone(), e.to_string()))?;
            return Ok(raw);
        }
    }
    Ok(BTreeMap::new())
}

/// Expand `packages:` globs (from `pnpm-workspace.yaml`/`otter-workspace.yaml`),
/// falling back to `package.json#workspaces`. Returns the directories
/// containing each matched `package.json` (not the `package.json` paths
/// themselves).
///
/// Missing files are not an error — an empty vec is returned, matching
/// the single-project happy path.
pub fn find_workspace_packages(project_dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let cfg = WorkspaceConfig::load(project_dir)?;
    let patterns: Vec<String> = if !cfg.packages.is_empty() {
        cfg.packages
    } else {
        let pj = project_dir.join("package.json");
        if !pj.is_file() {
            return Ok(Vec::new());
        }
        let pkg = crate::PackageJson::from_path(&pj)?;
        pkg.workspaces
            .as_ref()
            .map(|w| w.patterns().to_vec())
            .unwrap_or_default()
    };

    if patterns.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for pat in &patterns {
        let full = project_dir.join(pat).join("package.json");
        let glob_str = full.to_string_lossy();
        if let Ok(entries) = glob::glob(&glob_str) {
            for entry in entries.flatten() {
                if let Some(parent) = entry.parent() {
                    out.push(parent.to_path_buf());
                }
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn loads_pnpm_workspace_yaml() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n  - 'apps/*'\n",
        );
        let cfg = WorkspaceConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.packages, vec!["packages/*", "apps/*"]);
    }

    #[test]
    fn otter_workspace_yaml_wins_over_pnpm() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("otter-workspace.yaml"),
            "packages:\n  - 'otter-pkg/*'\n",
        );
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'pnpm-pkg/*'\n",
        );
        let cfg = WorkspaceConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.packages, vec!["otter-pkg/*"]);
    }

    #[test]
    fn missing_workspace_yaml_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = WorkspaceConfig::load(dir.path()).unwrap();
        assert!(cfg.packages.is_empty());
    }

    #[test]
    fn find_packages_uses_pnpm_yaml() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n",
        );
        write(&dir.path().join("packages/a/package.json"), "{}");
        write(&dir.path().join("packages/b/package.json"), "{}");
        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn find_packages_falls_back_to_package_json_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("package.json"),
            r#"{"name":"root","workspaces":["apps/*"]}"#,
        );
        write(&dir.path().join("apps/web/package.json"), "{}");
        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with("apps/web"));
    }

    #[test]
    fn find_packages_yaml_beats_package_json() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'yaml-pkgs/*'\n",
        );
        write(
            &dir.path().join("package.json"),
            r#"{"name":"root","workspaces":["json-pkgs/*"]}"#,
        );
        write(&dir.path().join("yaml-pkgs/y/package.json"), "{}");
        write(&dir.path().join("json-pkgs/j/package.json"), "{}");
        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with("yaml-pkgs/y"));
    }

    #[test]
    fn find_packages_missing_files_is_empty_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let found = find_workspace_packages(dir.path()).unwrap();
        assert!(found.is_empty());
    }
}
