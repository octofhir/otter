//! Installed package graph reconstruction and prune support.
//!
//! This module is intentionally small and contributor-facing: it answers two
//! questions for CLI/runtime integration without performing registry fetches or
//! lifecycle execution.
//!
//! # Contents
//! - [`resolve_installed_project`] rebuilds a read-only [`PackageGraph`] from
//!   `package.json`, `otter.lock` or compatible npm/pnpm lockfiles, and
//!   already materialized `node_modules`.
//! - [`prune_removed_registry_packages`] removes registry package roots and
//!   project-local bin links that disappeared from the lockfile.
//!
//! # Invariants
//! - The graph is reconstructed from deterministic lockfile state plus the
//!   package manifests present on disk; missing install roots are skipped.
//! - A package is found in Otter's virtual store, or else where a flat
//!   `node_modules` keeps it.
//! - Prune only removes registry packages absent from the current lockfile.
//!   Workspace and `file:` packages are never deleted by this module.
//! - Lifecycle scripts are only metadata here. They are not executed.
//!
//! # See also
//! - [`crate::install_local_project`] for fetch/extract/materialization.
//! - [`crate::PackageGraph`] for the read-only graph consumed by `otter run`.

use std::path::Path;

use otter_pm_lockfile::{
    LockedPackage, Lockfile, ResolvedSource, ResolvedSourceKind, project_lockfile_candidates,
};
use otter_pm_manifest::{PACKAGE_JSON, PackageBinManifest, PackageManifest};

use crate::{
    LocalResolution, PackageBin, PackageDependencyKind, PackageGraph, PackageId,
    PackageManagerError, PackageRoot, binary_name_from_package_name, cache_key, package_name_path,
    resolve_local_project,
};

/// Resolve local/workspace/file packages plus already installed registry
/// packages from the first supported project lockfile and `node_modules`.
pub async fn resolve_installed_project(
    project_root: impl AsRef<Path>,
) -> Result<LocalResolution, PackageManagerError> {
    let project_root = project_root.as_ref();
    let mut resolution = resolve_local_project(project_root).await?;
    let Some(lockfile) = read_project_lockfile(project_root).await? else {
        return Ok(resolution);
    };
    for (id, package) in &lockfile.packages {
        let Some(ResolvedSource {
            kind: ResolvedSourceKind::Registry,
            ..
        }) = &package.resolved
        else {
            continue;
        };
        let package_id = PackageId::new(id);
        // Otter's own layout first; a tree another package manager laid out
        // keeps each package at `node_modules/<name>`.
        let stored = crate::layout::package_dir(project_root, id, &package.name);
        let package_root = if tokio::fs::try_exists(&stored).await.unwrap_or(false) {
            stored
        } else {
            project_root
                .join("node_modules")
                .join(package_name_path(&package.name))
        };
        let manifest_path = package_root.join(PACKAGE_JSON);
        if !tokio::fs::try_exists(&manifest_path)
            .await
            .map_err(|err| PackageManagerError::Io {
                path: manifest_path.clone(),
                message: err.to_string(),
            })?
        {
            continue;
        }
        let manifest = PackageManifest::read_from_dir(&package_root).await?;
        resolution.graph.insert_package(PackageRoot {
            id: package_id.clone(),
            name: package.name.clone(),
            version: package.version.clone(),
            root: package_root,
            manifest: manifest.clone(),
        });
        insert_existing_linked_bins_for_manifest(
            &mut resolution.graph,
            &package_id,
            project_root,
            &manifest,
        )
        .await?;
        for (dependency_name, target_id) in &package.dependencies {
            resolution.graph.insert_dependency_with_kind(
                package_id.clone(),
                dependency_name.clone(),
                PackageId::new(target_id.clone()),
                dependency_kind_from_manifest(&manifest, dependency_name),
            );
        }
    }
    // The local graph names registry dependencies by their ranges; the
    // lockfile records which installed package each one resolved to.
    let placeholders: Vec<PackageId> = resolution
        .lockfile
        .packages
        .iter()
        .filter(|(id, package)| {
            !lockfile.packages.contains_key(id.as_str())
                && package
                    .resolved
                    .as_ref()
                    .is_some_and(|source| source.kind == ResolvedSourceKind::Registry)
        })
        .map(|(id, _)| PackageId::new(id))
        .collect();
    for (from, dependencies) in resolution.graph.dependencies.clone() {
        let Some(locked) = lockfile.packages.get(from.as_str()) else {
            continue;
        };
        for (name, target) in dependencies {
            if !placeholders.contains(&target) {
                continue;
            }
            let Some(resolved) = locked.dependencies.get(&name) else {
                continue;
            };
            let kind = resolution
                .graph
                .dependency_kind(&from, &name)
                .unwrap_or(PackageDependencyKind::Runtime);
            resolution.graph.insert_dependency_with_kind(
                from.clone(),
                name,
                PackageId::new(resolved.clone()),
                kind,
            );
        }
    }
    for id in &placeholders {
        resolution.graph.packages.remove(id);
        resolution.graph.dependencies.remove(id);
        resolution.graph.dependency_kinds.remove(id);
    }
    resolution.lockfile = lockfile;
    Ok(resolution)
}

async fn read_project_lockfile(
    project_root: &Path,
) -> Result<Option<Lockfile>, PackageManagerError> {
    for (path, format) in project_lockfile_candidates(project_root) {
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(PackageManagerError::Io {
                    path,
                    message: err.to_string(),
                });
            }
        };
        return Lockfile::parse_format(format, &text)
            .map(Some)
            .map_err(PackageManagerError::from);
    }
    Ok(None)
}

/// Remove registry package roots and `.bin` links that existed in a previous
/// lockfile but are absent from the current lockfile.
pub async fn prune_removed_registry_packages(
    project_root: impl AsRef<Path>,
    previous: &Lockfile,
    current: &Lockfile,
) -> Result<crate::PruneReport, PackageManagerError> {
    let project_root = project_root.as_ref();
    let mut report = crate::PruneReport {
        removed_packages: 0,
        removed_bins: 0,
    };
    for (id, package) in &previous.packages {
        if current.packages.contains_key(id) || !is_installed_registry_package(package) {
            continue;
        }
        let package_root = crate::layout::package_dir(project_root, id, &package.name);
        report.removed_bins += remove_linked_bins(project_root, &package_root).await?;
        let entry = crate::layout::store_entry_dir(project_root, id);
        if tokio::fs::try_exists(&entry)
            .await
            .map_err(|err| PackageManagerError::Io {
                path: entry.clone(),
                message: err.to_string(),
            })?
        {
            tokio::fs::remove_dir_all(&entry)
                .await
                .map_err(|err| PackageManagerError::Io {
                    path: entry.clone(),
                    message: err.to_string(),
                })?;
            report.removed_packages += 1;
        }
        // The project's link to it, if it had one, now leads nowhere.
        let link = project_root
            .join("node_modules")
            .join(package_name_path(&package.name));
        if tokio::fs::symlink_metadata(&link)
            .await
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
            && !tokio::fs::try_exists(&link).await.unwrap_or(false)
        {
            remove_file_if_exists(&link).await?;
        }
        let marker = project_root
            .join("node_modules")
            .join(".otter-state")
            .join(format!("{}.source", cache_key(id)));
        remove_file_if_exists(&marker).await?;
    }
    Ok(report)
}

async fn insert_existing_linked_bins_for_manifest(
    graph: &mut PackageGraph,
    package_id: &PackageId,
    project_root: &Path,
    manifest: &PackageManifest,
) -> Result<(), PackageManagerError> {
    let Some(bin) = &manifest.bin else {
        return Ok(());
    };
    let bin_root = project_root.join("node_modules").join(".bin");
    match bin {
        PackageBinManifest::Path(_) => {
            if let Some(name) = &manifest.name {
                let binary_name = binary_name_from_package_name(name);
                insert_existing_linked_bin(graph, package_id, binary_name, &bin_root).await?;
            }
        }
        PackageBinManifest::Map(bins) => {
            for name in bins.keys() {
                insert_existing_linked_bin(graph, package_id, name, &bin_root).await?;
            }
        }
    }
    Ok(())
}

async fn insert_existing_linked_bin(
    graph: &mut PackageGraph,
    package_id: &PackageId,
    name: &str,
    bin_root: &Path,
) -> Result<(), PackageManagerError> {
    let path = bin_root.join(name);
    if tokio::fs::try_exists(&path)
        .await
        .map_err(|err| PackageManagerError::Io {
            path: path.clone(),
            message: err.to_string(),
        })?
    {
        graph.insert_bin(PackageBin {
            package: package_id.clone(),
            name: name.to_string(),
            path,
        });
    }
    Ok(())
}

async fn remove_linked_bins(
    project_root: &Path,
    package_root: &Path,
) -> Result<usize, PackageManagerError> {
    let manifest_path = package_root.join(PACKAGE_JSON);
    if !tokio::fs::try_exists(&manifest_path)
        .await
        .map_err(|err| PackageManagerError::Io {
            path: manifest_path.clone(),
            message: err.to_string(),
        })?
    {
        return Ok(0);
    }
    let manifest = PackageManifest::read_from_dir(package_root).await?;
    let Some(bin_manifest) = &manifest.bin else {
        return Ok(0);
    };
    let bin_root = project_root.join("node_modules").join(".bin");
    let mut removed = 0usize;
    match bin_manifest {
        PackageBinManifest::Path(_) => {
            if let Some(name) = &manifest.name {
                removed += usize::from(
                    remove_file_if_exists(&bin_root.join(binary_name_from_package_name(name)))
                        .await?,
                );
            }
        }
        PackageBinManifest::Map(bins) => {
            for name in bins.keys() {
                removed += usize::from(remove_file_if_exists(&bin_root.join(name)).await?);
            }
        }
    }
    Ok(removed)
}

async fn remove_file_if_exists(path: &Path) -> Result<bool, PackageManagerError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(PackageManagerError::Io {
            path: path.to_path_buf(),
            message: err.to_string(),
        }),
    }
}

fn is_installed_registry_package(package: &LockedPackage) -> bool {
    matches!(
        &package.resolved,
        Some(ResolvedSource {
            kind: ResolvedSourceKind::Registry,
            ..
        })
    )
}

fn dependency_kind_from_manifest(
    manifest: &PackageManifest,
    dependency_name: &str,
) -> PackageDependencyKind {
    if manifest.peer_dependencies.contains_key(dependency_name) {
        PackageDependencyKind::Peer
    } else if manifest.optional_dependencies.contains_key(dependency_name) {
        PackageDependencyKind::Optional
    } else if manifest.dev_dependencies.contains_key(dependency_name) {
        PackageDependencyKind::Development
    } else {
        PackageDependencyKind::Runtime
    }
}
