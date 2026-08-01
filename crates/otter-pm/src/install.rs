//! Package extraction and project materialization.
//!
//! This module owns the install layout side of package management. It consumes
//! already enriched lockfile registry tarball sources, reuses the tarball
//! cache, extracts package archives into the content-addressed store,
//! materializes `node_modules` by linking to stored content, and links package
//! binaries.
//!
//! Extraction happens once per tarball across every project on the machine:
//! the archive becomes hashed files plus an index describing the tree, and
//! installing is then a walk over that index. A second project installing the
//! same package writes no package bytes at all.
//!
//! # Contents
//! - [`FsPackageStore`] is the tarball cache, content store, and materializer.
//! - [`ExtractedPackage`] describes a package's stored tree.
//! - [`InstalledPackage`] describes one project-local materialized package.
//!
//! # Invariants
//! - Lifecycle scripts are not executed.
//! - Install roots are refreshed through temporary directories before rename.
//! - Archive paths are normalized and may not escape the package root.
//! - A materialized file is a link to stored content wherever the filesystem
//!   allows it, and a copy of the same bytes where it does not.
//! - Bin links point at the project-local `node_modules/.bin` layout.
//!
//! # See also
//! - [`crate::install_local_project`] for the full resolve/fetch/write flow.
//! - [`crate::store`] for the content store and package index.
//! - [`crate::tarball`] for byte fetch and integrity verification.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use otter_pm_lockfile::{LockedPackage, Lockfile, ResolvedSource, ResolvedSourceKind};
use otter_pm_manifest::{PACKAGE_JSON, PackageBinManifest, PackageManifest};

use crate::store::{ContentStore, PackageIndex};
use crate::tarball::tarball_cache_key;
use crate::{
    CachedTarball, FsTarballCache, PackageBin, PackageId, PackageManagerError, TarballFetchClient,
    TarballSource, binary_name_from_package_name, cache_key, package_name_path,
};

/// Tarball cache, content-addressed package store, and project materializer.
#[derive(Debug, Clone)]
pub struct FsPackageStore {
    tarballs: FsTarballCache,
    content: ContentStore,
}

impl FsPackageStore {
    /// Create a package store under `cache_root`.
    #[must_use]
    pub fn new(cache_root: impl Into<PathBuf>) -> Self {
        let cache_root = cache_root.into();
        Self {
            tarballs: FsTarballCache::new(cache_root.join("tarballs")),
            content: ContentStore::new(cache_root.join("store")),
        }
    }

    /// Create the package store shared by every project for this user.
    ///
    /// Sharing is the point: a package extracted for one checkout is already
    /// stored for the next one, and the two trees link to the same bytes.
    #[must_use]
    pub fn user_default() -> Self {
        Self::new(crate::user_cache_root())
    }

    /// Borrow the content store backing this package store.
    #[must_use]
    pub fn content_store(&self) -> &ContentStore {
        &self.content
    }

    /// Fetch, verify, and extract one package into the content store.
    pub async fn get_or_fetch_and_extract(
        &self,
        source: &TarballSource,
        client: &impl TarballFetchClient,
    ) -> Result<ExtractedPackage, PackageManagerError> {
        let cached_tarball = self.tarballs.get_or_fetch(source, client).await?;
        let key = tarball_cache_key(source);
        if let Some(index) = self.content.read_index(&key).await? {
            return Ok(ExtractedPackage {
                index,
                tarball: cached_tarball,
                reused: true,
            });
        }

        let archive_path = cached_tarball.path.clone();
        let content = self.content.clone();
        let key_for_task = key.clone();
        let index = tokio::task::spawn_blocking(move || {
            let index = store_tgz_package(&content, &archive_path)?;
            content.write_index(&key_for_task, &index)?;
            Ok::<_, PackageManagerError>(index)
        })
        .await
        .map_err(|err| PackageManagerError::Archive {
            path: cached_tarball.path.clone(),
            message: err.to_string(),
        })??;

        Ok(ExtractedPackage {
            index,
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
            let fingerprint = extracted.index.fingerprint();
            let reused_install =
                existing_install_matches(&install_root, &marker, &fingerprint).await?;
            if !reused_install {
                self.materialize_install_root(
                    &extracted.index,
                    &install_root,
                    &marker,
                    &fingerprint,
                )
                .await?;
            }
            let linked_bins = link_package_bins(project_root, &PackageId::new(&id), &install_root)
                .await?
                .len();
            installed.push(InstalledPackage {
                package_id: id,
                name: package.name,
                source,
                installed_root: install_root,
                reused_cache: extracted.reused && extracted.tarball.reused,
                reused_install,
                linked_bins,
            });
        }
        Ok(installed)
    }

    async fn materialize_install_root(
        &self,
        index: &PackageIndex,
        install_root: &Path,
        marker: &Path,
        fingerprint: &str,
    ) -> Result<(), PackageManagerError> {
        let content = self.content.clone();
        let index = index.clone();
        let install_root = install_root.to_path_buf();
        let install_root_for_error = install_root.clone();
        let marker = marker.to_path_buf();
        let fingerprint = fingerprint.to_string();
        tokio::task::spawn_blocking(move || {
            let temporary = install_root
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(format!(".otter-install-tmp-{}", std::process::id()));
            if temporary.exists() {
                std::fs::remove_dir_all(&temporary).map_err(|err| PackageManagerError::Io {
                    path: temporary.clone(),
                    message: err.to_string(),
                })?;
            }
            content.materialize(&index, &temporary)?;
            replace_directory(&temporary, &install_root)?;
            write_marker(&marker, &fingerprint)
        })
        .await
        .map_err(|err| PackageManagerError::Archive {
            path: install_root_for_error,
            message: err.to_string(),
        })?
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
    if matches!(
        kind,
        ResolvedSourceKind::Registry | ResolvedSourceKind::Tarball
    ) && let Some(path) = reference.strip_prefix("file:")
    {
        return project_root.join(path).to_string_lossy().into_owned();
    }
    reference.to_string()
}

/// One package's stored tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedPackage {
    /// Package tree, with npm's leading `package/` prefix stripped.
    pub index: PackageIndex,
    /// Backing cached tarball.
    pub tarball: CachedTarball,
    /// `true` when the package was already in the content store.
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

fn write_marker(marker: &Path, fingerprint: &str) -> Result<(), PackageManagerError> {
    let parent = marker
        .parent()
        .ok_or_else(|| PackageManagerError::Archive {
            path: marker.to_path_buf(),
            message: "marker has no parent directory".to_string(),
        })?;
    std::fs::create_dir_all(parent).map_err(|err| PackageManagerError::Io {
        path: parent.to_path_buf(),
        message: err.to_string(),
    })?;
    std::fs::write(marker, fingerprint).map_err(|err| PackageManagerError::Io {
        path: marker.to_path_buf(),
        message: err.to_string(),
    })
}

fn replace_directory(temporary: &Path, install_root: &Path) -> Result<(), PackageManagerError> {
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
    std::fs::rename(temporary, install_root).map_err(|err| PackageManagerError::Io {
        path: install_root.to_path_buf(),
        message: err.to_string(),
    })
}

/// Read a package archive into the content store and describe its tree.
///
/// Directory entries are not recorded: a file's path implies every directory
/// above it, and materialization creates them.
fn store_tgz_package(
    content: &ContentStore,
    archive_path: &Path,
) -> Result<PackageIndex, PackageManagerError> {
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
    let mut index = PackageIndex::default();
    let mut bytes = Vec::new();
    for entry in entries {
        let mut entry = entry.map_err(|err| PackageManagerError::Archive {
            path: archive_path.to_path_buf(),
            message: err.to_string(),
        })?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().map_err(|err| PackageManagerError::Archive {
            path: archive_path.to_path_buf(),
            message: err.to_string(),
        })?;
        let relative = archive_relative_path(&path)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let executable = entry.header().mode().is_ok_and(|mode| mode & 0o111 != 0);
        bytes.clear();
        entry
            .read_to_end(&mut bytes)
            .map_err(|err| PackageManagerError::Archive {
                path: archive_path.to_path_buf(),
                message: err.to_string(),
            })?;
        let stored = content.add_file(&bytes, executable)?;
        index.files.insert(index_key(&relative), stored);
    }
    Ok(index)
}

/// Package-relative path as an index key, always `/`-separated so an index
/// written on one platform materializes the same tree on another.
fn index_key(relative: &Path) -> String {
    relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
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
