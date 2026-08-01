//! Undeclared-dependency detection for Otter package management.
//!
//! A project can import a package it never declared and still work: the
//! package is in `node_modules` because something else depends on it. The
//! import breaks the day that other dependency drops it, changes its own
//! version range, or is hoisted differently — a failure with no connection to
//! anything the project changed. This crate finds those imports while they
//! still work.
//!
//! Detection parses the same way the engine loads code, so an import that is
//! erased at runtime — a type-only import, a computed specifier — is never
//! reported, and an import a `try` block already handles is reported as
//! guarded rather than required.
//!
//! # Contents
//! - [`ProjectScan`] — the result of scanning a project's own sources.
//! - [`UndeclaredDependency`] — one package imported but not declared.
//! - [`scan_project`] — run the scan.
//!
//! # Invariants
//! - Precision over recall. Every uncertainty — an unparseable file, a
//!   computed specifier, a subpath-imports entry — is dropped rather than
//!   reported, because a false report costs trust in every true one.
//! - Only the project's own sources are scanned. `node_modules` is somebody
//!   else's code and answers a different question.
//! - The scan reads; it never edits a manifest or installs anything.
//!
//! # See also
//! - [`otter_pm_manifest::PackageManifest`] for the declared surface.

mod builtins;
mod extract;
mod specifier;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use otter_pm_manifest::{ManifestError, PACKAGE_JSON, PackageManifest};
use otter_syntax::detect_source_kind;
use serde::{Deserialize, Serialize};

pub use builtins::is_builtin;
pub use extract::{Occurrence, extract_specifiers};
pub use specifier::package_name;

/// Directory names never descended into while scanning a project.
const SKIPPED_DIRECTORIES: &[&str] = &["node_modules", ".git", ".otter", "target"];

/// Scan failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PhantomError {
    /// The project manifest could not be read.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    /// A filesystem operation failed.
    #[error("dependency scan I/O failed for `{path}`: {message}")]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying error message.
        message: String,
    },
}

/// One package a project imports without declaring it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndeclaredDependency {
    /// Package name to declare.
    pub package: String,
    /// Files that import it, in path order.
    pub files: Vec<PathBuf>,
    /// `true` when every import of it is inside a `try` block.
    pub guarded: bool,
    /// `true` when the package is nonetheless present in `node_modules`.
    ///
    /// This is the dangerous case: the import works today because something
    /// else installed the package, and breaks when that stops being true.
    pub installed: bool,
}

/// Result of scanning a project's own sources.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectScan {
    /// Packages imported but not declared, in name order.
    pub undeclared: Vec<UndeclaredDependency>,
    /// Number of source files parsed.
    pub scanned_files: usize,
}

/// Scan `project_root`'s own sources for imports of undeclared packages.
pub async fn scan_project(project_root: &Path) -> Result<ProjectScan, PhantomError> {
    let manifest = PackageManifest::read_from_dir(project_root).await?;
    let declared = declared_packages(&manifest);
    let project_root = project_root.to_path_buf();

    let root_for_task = project_root.clone();
    let files = tokio::task::spawn_blocking(move || collect_source_files(&root_for_task))
        .await
        .map_err(|err| PhantomError::Io {
            path: project_root.clone(),
            message: err.to_string(),
        })??;

    let mut uses: BTreeMap<String, PackageUse> = BTreeMap::new();
    let mut scanned_files = 0usize;
    for file in &files {
        let Some(kind) = detect_source_kind(file) else {
            continue;
        };
        let Ok(source) = tokio::fs::read_to_string(file).await else {
            continue;
        };
        scanned_files += 1;
        for occurrence in extract_specifiers(&source, kind) {
            if is_builtin(&occurrence.specifier) {
                continue;
            }
            let Some(package) = package_name(&occurrence.specifier) else {
                continue;
            };
            if declared.contains(&package) {
                continue;
            }
            let entry = uses.entry(package).or_insert_with(|| PackageUse {
                files: BTreeSet::new(),
                guarded: true,
            });
            entry.files.insert(file.clone());
            entry.guarded &= occurrence.guarded;
        }
    }

    let mut undeclared = Vec::with_capacity(uses.len());
    for (package, package_use) in uses {
        let installed = is_installed(&project_root, &package).await;
        undeclared.push(UndeclaredDependency {
            package,
            files: package_use.files.into_iter().collect(),
            guarded: package_use.guarded,
            installed,
        });
    }
    Ok(ProjectScan {
        undeclared,
        scanned_files,
    })
}

struct PackageUse {
    files: BTreeSet<PathBuf>,
    guarded: bool,
}

/// Everything a project may import without declaring a new dependency: its
/// own dependency buckets and its own name.
fn declared_packages(manifest: &PackageManifest) -> BTreeSet<String> {
    let mut declared = BTreeSet::new();
    for bucket in [
        &manifest.dependencies,
        &manifest.dev_dependencies,
        &manifest.peer_dependencies,
        &manifest.optional_dependencies,
    ] {
        declared.extend(bucket.keys().cloned());
    }
    if let Some(name) = &manifest.name {
        declared.insert(name.clone());
    }
    declared
}

async fn is_installed(project_root: &Path, package: &str) -> bool {
    let root = package
        .split('/')
        .fold(project_root.join("node_modules"), |path, part| {
            path.join(part)
        });
    tokio::fs::try_exists(root.join(PACKAGE_JSON))
        .await
        .unwrap_or(false)
}

fn collect_source_files(root: &Path) -> Result<Vec<PathBuf>, PhantomError> {
    let mut files = Vec::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(PhantomError::Io {
                    path: directory,
                    message: err.to_string(),
                });
            }
        };
        for entry in entries {
            let entry = entry.map_err(|err| PhantomError::Io {
                path: directory.clone(),
                message: err.to_string(),
            })?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let file_type = entry.file_type().map_err(|err| PhantomError::Io {
                path: path.clone(),
                message: err.to_string(),
            })?;
            if file_type.is_dir() {
                if SKIPPED_DIRECTORIES.contains(&name.as_ref()) || name.starts_with('.') {
                    continue;
                }
                directories.push(path);
            } else if file_type.is_file() && detect_source_kind(&path).is_some() {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[tokio::test]
    async fn a_declared_import_is_not_reported() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join(PACKAGE_JSON),
            r#"{"name":"app","dependencies":{"left-pad":"^1.3.0"}}"#,
        );
        write(
            &tmp.path().join("src/index.ts"),
            "import pad from 'left-pad';\nimport fs from 'node:fs';\nimport './local.js';\nexport { pad, fs };\n",
        );
        let scan = scan_project(tmp.path()).await.unwrap();
        assert!(scan.undeclared.is_empty());
        assert_eq!(scan.scanned_files, 1);
    }

    #[tokio::test]
    async fn an_undeclared_import_is_reported_with_its_files() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join(PACKAGE_JSON), r#"{"name":"app"}"#);
        write(
            &tmp.path().join("src/a.ts"),
            "import { merge } from 'lodash/fp';\n",
        );
        write(&tmp.path().join("src/b.js"), "require('lodash');\n");
        let scan = scan_project(tmp.path()).await.unwrap();
        assert_eq!(scan.undeclared.len(), 1);
        assert_eq!(scan.undeclared[0].package, "lodash");
        assert_eq!(scan.undeclared[0].files.len(), 2);
        assert!(!scan.undeclared[0].guarded);
        assert!(!scan.undeclared[0].installed);
    }

    #[tokio::test]
    async fn an_undeclared_import_that_resolves_today_is_flagged_as_installed() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join(PACKAGE_JSON), r#"{"name":"app"}"#);
        write(
            &tmp.path().join("src/a.js"),
            "require('@scope/tool/sub');\n",
        );
        write(
            &tmp.path()
                .join("node_modules/@scope/tool")
                .join(PACKAGE_JSON),
            r#"{"name":"@scope/tool","version":"1.0.0"}"#,
        );
        let scan = scan_project(tmp.path()).await.unwrap();
        assert_eq!(scan.undeclared.len(), 1);
        assert_eq!(scan.undeclared[0].package, "@scope/tool");
        assert!(scan.undeclared[0].installed);
    }

    #[tokio::test]
    async fn a_guarded_import_is_reported_as_guarded() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join(PACKAGE_JSON), r#"{"name":"app"}"#);
        write(
            &tmp.path().join("src/a.js"),
            "try { require('optional-thing'); } catch {}\n",
        );
        let scan = scan_project(tmp.path()).await.unwrap();
        assert_eq!(scan.undeclared.len(), 1);
        assert!(scan.undeclared[0].guarded);
    }

    #[tokio::test]
    async fn a_package_importing_itself_is_not_undeclared() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join(PACKAGE_JSON), r#"{"name":"app"}"#);
        write(&tmp.path().join("src/a.js"), "require('app/helpers');\n");
        assert!(
            scan_project(tmp.path())
                .await
                .unwrap()
                .undeclared
                .is_empty()
        );
    }

    #[tokio::test]
    async fn installed_and_hidden_directories_are_not_scanned() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join(PACKAGE_JSON), r#"{"name":"app"}"#);
        write(
            &tmp.path().join("node_modules/dep/index.js"),
            "require('somebody-elses-problem');\n",
        );
        write(
            &tmp.path().join(".cache/gen.js"),
            "require('generated-thing');\n",
        );
        let scan = scan_project(tmp.path()).await.unwrap();
        assert!(scan.undeclared.is_empty());
        assert_eq!(scan.scanned_files, 0);
    }

    #[tokio::test]
    async fn type_only_imports_are_not_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join(PACKAGE_JSON), r#"{"name":"app"}"#);
        write(
            &tmp.path().join("src/a.ts"),
            "import type { Config } from 'some-types';\nexport type { Config };\n",
        );
        assert!(
            scan_project(tmp.path())
                .await
                .unwrap()
                .undeclared
                .is_empty()
        );
    }
}
