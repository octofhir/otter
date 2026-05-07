//! Package extraction and project materialization.
//!
//! This module owns the install layout side of package management. It consumes
//! already enriched lockfile registry tarball sources, reuses the tarball cache,
//! extracts package archives into a content-addressed store, materializes
//! `node_modules`, and links package binaries.
//!
//! # Contents
//! - [`FsPackageStore`] is the extracted package cache plus materializer.
//! - [`ExtractedPackage`] describes a package cache entry.
//! - [`InstalledPackage`] describes one project-local materialized package.
//!
//! # Invariants
//! - Lifecycle scripts are not executed.
//! - Install roots are refreshed through temporary directories before rename.
//! - Archive paths are normalized and may not escape the package root.
//! - Bin links point at the project-local `node_modules/.bin` layout.
//!
//! # See also
//! - [`crate::install_local_project`] for the full resolve/fetch/write flow.
//! - [`crate::tarball`] for byte fetch and integrity verification.

use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use otter_pm_lockfile::{LockedPackage, Lockfile, ResolvedSource, ResolvedSourceKind};
use otter_pm_manifest::{PACKAGE_JSON, PackageBinManifest, PackageManifest};

use crate::tarball::tarball_cache_key;
use crate::{
    CachedTarball, FsTarballCache, PackageBin, PackageId, PackageManagerError, TarballFetchClient,
    TarballSource, binary_name_from_package_name, cache_key, install_fingerprint,
    package_name_path,
};

/// Extracted package cache and project install materializer.
#[derive(Debug, Clone)]
pub struct FsPackageStore {
    tarballs: FsTarballCache,
    packages_root: PathBuf,
}

impl FsPackageStore {
    /// Create a package store under `cache_root`.
    #[must_use]
    pub fn new(cache_root: impl Into<PathBuf>) -> Self {
        let cache_root = cache_root.into();
        Self {
            tarballs: FsTarballCache::new(cache_root.join("tarballs")),
            packages_root: cache_root.join("packages"),
        }
    }

    /// Return the deterministic extracted package cache root for a tarball.
    #[must_use]
    pub fn extracted_package_path(&self, source: &TarballSource) -> PathBuf {
        self.packages_root.join(tarball_cache_key(source))
    }

    /// Fetch, verify, and extract one package into the content-addressed cache.
    pub async fn get_or_fetch_and_extract(
        &self,
        source: &TarballSource,
        client: &impl TarballFetchClient,
    ) -> Result<ExtractedPackage, PackageManagerError> {
        let cached_tarball = self.tarballs.get_or_fetch(source, client).await?;
        tokio::fs::create_dir_all(&self.packages_root)
            .await
            .map_err(|err| PackageManagerError::Io {
                path: self.packages_root.clone(),
                message: err.to_string(),
            })?;
        let root = self.extracted_package_path(source);
        if tokio::fs::try_exists(root.join(PACKAGE_JSON))
            .await
            .map_err(|err| PackageManagerError::Io {
                path: root.clone(),
                message: err.to_string(),
            })?
        {
            return Ok(ExtractedPackage {
                root,
                tarball: cached_tarball,
                reused: true,
            });
        }

        let tmp = self.packages_root.join(format!(
            ".tmp-{}-{}",
            tarball_cache_key(source),
            std::process::id()
        ));
        let archive_path = cached_tarball.path.clone();
        let root_for_task = root.clone();
        let tmp_for_task = tmp.clone();
        tokio::task::spawn_blocking(move || {
            extract_tgz_package(&archive_path, &tmp_for_task, &root_for_task)
        })
        .await
        .map_err(|err| PackageManagerError::Archive {
            path: root.clone(),
            message: err.to_string(),
        })??;

        Ok(ExtractedPackage {
            root,
            tarball: cached_tarball,
            reused: false,
        })
    }

    /// Materialize all registry tarballs from `lockfile` into `node_modules`.
    pub async fn materialize_registry_packages(
        &self,
        project_root: impl AsRef<Path>,
        lockfile: &Lockfile,
        client: &impl TarballFetchClient,
    ) -> Result<Vec<InstalledPackage>, PackageManagerError> {
        let project_root = project_root.as_ref();
        let mut packages = tarball_packages_for_project(project_root, lockfile);
        packages.sort_by(|a, b| a.0.cmp(&b.0));
        let mut installed = Vec::with_capacity(packages.len());
        for (id, package, source) in packages {
            let extracted = self.get_or_fetch_and_extract(&source, client).await?;
            let install_root = project_root
                .join("node_modules")
                .join(package_name_path(&package.name));
            let state_root = project_root.join("node_modules").join(".otter-state");
            let marker = state_root.join(format!("{}.source", cache_key(&id)));
            let fingerprint = install_fingerprint(&source);
            let reused_install =
                existing_install_matches(&install_root, &marker, &fingerprint).await?;
            if !reused_install {
                materialize_install_root(&extracted.root, &install_root, &marker, &fingerprint)
                    .await?;
            }
            let linked_bins = link_package_bins(project_root, &PackageId::new(&id), &install_root)
                .await?
                .len();
            installed.push(InstalledPackage {
                package_id: id,
                name: package.name,
                source,
                cache_root: extracted.root,
                installed_root: install_root,
                reused_cache: extracted.reused && extracted.tarball.reused,
                reused_install,
                linked_bins,
            });
        }
        Ok(installed)
    }
}

fn tarball_packages_for_project(
    project_root: &Path,
    lockfile: &Lockfile,
) -> Vec<(String, LockedPackage, TarballSource)> {
    lockfile
        .packages
        .iter()
        .filter_map(|(id, package)| match &package.resolved {
            Some(ResolvedSource { kind, reference })
                if matches!(
                    kind,
                    ResolvedSourceKind::Registry | ResolvedSourceKind::Tarball
                ) && crate::is_tarball_reference(reference) =>
            {
                Some((
                    id.clone(),
                    package.clone(),
                    TarballSource {
                        url: materialization_tarball_url(project_root, *kind, reference),
                        integrity: package.integrity.clone(),
                    },
                ))
            }
            _ => None,
        })
        .collect()
}

fn materialization_tarball_url(
    project_root: &Path,
    kind: ResolvedSourceKind,
    reference: &str,
) -> String {
    if kind == ResolvedSourceKind::Tarball
        && let Some(path) = reference.strip_prefix("file:")
    {
        return project_root.join(path).to_string_lossy().into_owned();
    }
    reference.to_string()
}

/// Extracted package cache entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedPackage {
    /// Extracted package root with npm's leading `package/` prefix stripped.
    pub root: PathBuf,
    /// Backing cached tarball.
    pub tarball: CachedTarball,
    /// `true` when extraction was already present.
    pub reused: bool,
}

/// Project-local installed package entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPackage {
    /// Lockfile package id.
    pub package_id: String,
    /// Package name.
    pub name: String,
    /// Tarball source.
    pub source: TarballSource,
    /// Content-addressed extracted cache root.
    pub cache_root: PathBuf,
    /// Project-local install root.
    pub installed_root: PathBuf,
    /// `true` when both tarball and extracted package cache were reused.
    pub reused_cache: bool,
    /// `true` when `node_modules` materialization was already current.
    pub reused_install: bool,
    /// Number of linked binaries for this package.
    pub linked_bins: usize,
}

async fn existing_install_matches(
    install_root: &Path,
    marker: &Path,
    fingerprint: &str,
) -> Result<bool, PackageManagerError> {
    if !tokio::fs::try_exists(install_root)
        .await
        .map_err(|err| PackageManagerError::Io {
            path: install_root.to_path_buf(),
            message: err.to_string(),
        })?
    {
        return Ok(false);
    }
    match tokio::fs::read_to_string(marker).await {
        Ok(text) => Ok(text == fingerprint),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(PackageManagerError::Io {
            path: marker.to_path_buf(),
            message: err.to_string(),
        }),
    }
}

async fn materialize_install_root(
    source_root: &Path,
    install_root: &Path,
    marker: &Path,
    fingerprint: &str,
) -> Result<(), PackageManagerError> {
    let tmp_root = install_root
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".otter-install-tmp-{}", std::process::id()));
    let source_root = source_root.to_path_buf();
    let install_root = install_root.to_path_buf();
    let install_root_for_error = install_root.clone();
    let marker = marker.to_path_buf();
    let fingerprint = fingerprint.to_string();
    tokio::task::spawn_blocking(move || {
        copy_package_tree(&source_root, &tmp_root, &install_root)?;
        let marker_parent = marker
            .parent()
            .ok_or_else(|| PackageManagerError::Archive {
                path: marker.clone(),
                message: "marker has no parent directory".to_string(),
            })?;
        std::fs::create_dir_all(marker_parent).map_err(|err| PackageManagerError::Io {
            path: marker_parent.to_path_buf(),
            message: err.to_string(),
        })?;
        std::fs::write(&marker, fingerprint).map_err(|err| PackageManagerError::Io {
            path: marker,
            message: err.to_string(),
        })
    })
    .await
    .map_err(|err| PackageManagerError::Archive {
        path: install_root_for_error,
        message: err.to_string(),
    })?
}

fn copy_package_tree(
    source_root: &Path,
    tmp_root: &Path,
    install_root: &Path,
) -> Result<(), PackageManagerError> {
    if tmp_root.exists() {
        std::fs::remove_dir_all(tmp_root).map_err(|err| PackageManagerError::Io {
            path: tmp_root.to_path_buf(),
            message: err.to_string(),
        })?;
    }
    if let Some(parent) = tmp_root.parent() {
        std::fs::create_dir_all(parent).map_err(|err| PackageManagerError::Io {
            path: parent.to_path_buf(),
            message: err.to_string(),
        })?;
    }
    copy_dir_recursive(source_root, tmp_root)?;
    if install_root.exists() {
        std::fs::remove_dir_all(install_root).map_err(|err| PackageManagerError::Io {
            path: install_root.to_path_buf(),
            message: err.to_string(),
        })?;
    }
    if let Some(parent) = install_root.parent() {
        std::fs::create_dir_all(parent).map_err(|err| PackageManagerError::Io {
            path: parent.to_path_buf(),
            message: err.to_string(),
        })?;
    }
    std::fs::rename(tmp_root, install_root).map_err(|err| PackageManagerError::Io {
        path: install_root.to_path_buf(),
        message: err.to_string(),
    })
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<(), PackageManagerError> {
    std::fs::create_dir_all(destination).map_err(|err| PackageManagerError::Io {
        path: destination.to_path_buf(),
        message: err.to_string(),
    })?;
    for entry in std::fs::read_dir(source).map_err(|err| PackageManagerError::Io {
        path: source.to_path_buf(),
        message: err.to_string(),
    })? {
        let entry = entry.map_err(|err| PackageManagerError::Io {
            path: source.to_path_buf(),
            message: err.to_string(),
        })?;
        let file_type = entry.file_type().map_err(|err| PackageManagerError::Io {
            path: entry.path(),
            message: err.to_string(),
        })?;
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &target).map_err(|err| PackageManagerError::Io {
                path: target,
                message: err.to_string(),
            })?;
        }
    }
    Ok(())
}

fn extract_tgz_package(
    archive_path: &Path,
    tmp_root: &Path,
    final_root: &Path,
) -> Result<(), PackageManagerError> {
    if tmp_root.exists() {
        std::fs::remove_dir_all(tmp_root).map_err(|err| PackageManagerError::Io {
            path: tmp_root.to_path_buf(),
            message: err.to_string(),
        })?;
    }
    if final_root.exists() {
        std::fs::remove_dir_all(final_root).map_err(|err| PackageManagerError::Io {
            path: final_root.to_path_buf(),
            message: err.to_string(),
        })?;
    }
    std::fs::create_dir_all(tmp_root).map_err(|err| PackageManagerError::Io {
        path: tmp_root.to_path_buf(),
        message: err.to_string(),
    })?;
    let file = std::fs::File::open(archive_path).map_err(|err| PackageManagerError::Io {
        path: archive_path.to_path_buf(),
        message: err.to_string(),
    })?;
    let decoder = GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|err| PackageManagerError::Archive {
            path: archive_path.to_path_buf(),
            message: err.to_string(),
        })?;
    for entry in entries {
        let mut entry = entry.map_err(|err| PackageManagerError::Archive {
            path: archive_path.to_path_buf(),
            message: err.to_string(),
        })?;
        let path = entry.path().map_err(|err| PackageManagerError::Archive {
            path: archive_path.to_path_buf(),
            message: err.to_string(),
        })?;
        let relative = archive_relative_path(&path)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let destination = tmp_root.join(relative);
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            std::fs::create_dir_all(&destination).map_err(|err| PackageManagerError::Io {
                path: destination,
                message: err.to_string(),
            })?;
        } else if entry_type.is_file() {
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent).map_err(|err| PackageManagerError::Io {
                    path: parent.to_path_buf(),
                    message: err.to_string(),
                })?;
            }
            entry
                .unpack(&destination)
                .map_err(|err| PackageManagerError::Archive {
                    path: destination,
                    message: err.to_string(),
                })?;
        }
    }
    if let Some(parent) = final_root.parent() {
        std::fs::create_dir_all(parent).map_err(|err| PackageManagerError::Io {
            path: parent.to_path_buf(),
            message: err.to_string(),
        })?;
    }
    std::fs::rename(tmp_root, final_root).map_err(|err| PackageManagerError::Io {
        path: final_root.to_path_buf(),
        message: err.to_string(),
    })
}

fn archive_relative_path(path: &Path) -> Result<PathBuf, PackageManagerError> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) if part == "package" && out.as_os_str().is_empty() => {}
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(PackageManagerError::Archive {
                    path: path.to_path_buf(),
                    message: "archive path escapes package root".to_string(),
                });
            }
        }
    }
    Ok(out)
}

async fn link_package_bins(
    project_root: &Path,
    package_id: &PackageId,
    package_root: &Path,
) -> Result<Vec<PackageBin>, PackageManagerError> {
    let manifest_path = package_root.join(PACKAGE_JSON);
    if !tokio::fs::try_exists(&manifest_path)
        .await
        .map_err(|err| PackageManagerError::Io {
            path: manifest_path.clone(),
            message: err.to_string(),
        })?
    {
        return Ok(Vec::new());
    }
    let manifest = PackageManifest::read_from_dir(package_root).await?;
    let Some(bin_manifest) = &manifest.bin else {
        return Ok(Vec::new());
    };
    let bin_root = project_root.join("node_modules").join(".bin");
    tokio::fs::create_dir_all(&bin_root)
        .await
        .map_err(|err| PackageManagerError::Io {
            path: bin_root.clone(),
            message: err.to_string(),
        })?;
    let bins = bin_entries(package_id, package_root, &manifest, bin_manifest);
    for bin in &bins {
        link_bin_file(&bin.path, &bin_root.join(&bin.name)).await?;
    }
    Ok(bins
        .into_iter()
        .map(|bin| PackageBin {
            package: bin.package,
            name: bin.name.clone(),
            path: bin_root.join(bin.name),
        })
        .collect())
}

fn bin_entries(
    package_id: &PackageId,
    package_root: &Path,
    manifest: &PackageManifest,
    bin: &PackageBinManifest,
) -> Vec<PackageBin> {
    match bin {
        PackageBinManifest::Path(path) => manifest
            .name
            .as_ref()
            .map(|name| {
                let binary_name = binary_name_from_package_name(name);
                PackageBin {
                    package: package_id.clone(),
                    name: binary_name.to_string(),
                    path: package_root.join(path),
                }
            })
            .into_iter()
            .collect(),
        PackageBinManifest::Map(bins) => bins
            .iter()
            .map(|(name, path)| PackageBin {
                package: package_id.clone(),
                name: name.clone(),
                path: package_root.join(path),
            })
            .collect(),
    }
}

async fn link_bin_file(source: &Path, target: &Path) -> Result<(), PackageManagerError> {
    let source = tokio::fs::canonicalize(source)
        .await
        .map_err(|err| PackageManagerError::Io {
            path: source.to_path_buf(),
            message: err.to_string(),
        })?;
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|err| PackageManagerError::Io {
                path: parent.to_path_buf(),
                message: err.to_string(),
            })?;
    }
    match tokio::fs::remove_file(target).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(PackageManagerError::Io {
                path: target.to_path_buf(),
                message: err.to_string(),
            });
        }
    }
    #[cfg(unix)]
    {
        let source_for_error = source.clone();
        let target = target.to_path_buf();
        tokio::task::spawn_blocking(move || {
            std::os::unix::fs::symlink(&source, &target).map_err(|err| PackageManagerError::Io {
                path: target,
                message: err.to_string(),
            })
        })
        .await
        .map_err(|err| PackageManagerError::Io {
            path: source_for_error,
            message: err.to_string(),
        })??;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tokio::fs::copy(&source, target)
            .await
            .map_err(|err| PackageManagerError::Io {
                path: target.to_path_buf(),
                message: err.to_string(),
            })?;
        Ok(())
    }
}
