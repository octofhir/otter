//! Package extraction and project materialization.
//!
//! This module owns the install layout side of package management. It
//! fetches the tarballs a lockfile names, unpacks each once into the
//! machine's package store, materializes the project's package directories
//! from the stored trees, and links package binaries.
//!
//! A tarball is unpacked once across every project on the machine: a second
//! project installing the same package reads no archive and writes no byte of
//! it, only its own copy of the stored tree.
//!
//! # Contents
//! - [`FsPackageStore`] is the package store and materializer.
//! - [`ExtractedPackage`] names a package's stored tree.
//! - [`InstalledPackage`] describes one project-local materialized package.
//!
//! # Invariants
//! - Lifecycle scripts are not executed.
//! - Install roots are refreshed through temporary directories before rename.
//! - Archive paths are normalized and may not escape the package root.
//! - A tarball is verified before it is unpacked, and unpacked from memory.
//! - Bin links point at the project-local `node_modules/.bin` layout.
//! - A package version materializes into its one directory of
//!   [`crate::layout`]; no two packages share a directory or a temporary one.
//!
//! # See also
//! - [`crate::install_local_project`] for the full resolve/fetch/write flow.
//! - [`crate::store`] for the package store.
//! - [`crate::tarball`] for byte fetch and integrity verification.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use futures_util::{StreamExt, TryStreamExt, stream};

use flate2::read::GzDecoder;
use otter_pm_lockfile::{LockedPackage, Lockfile, ResolvedSource, ResolvedSourceKind};
use otter_pm_manifest::{PACKAGE_JSON, PackageBinManifest, PackageManifest};

use crate::store::{StagedFile, TreeStore};
use crate::tarball::{tarball_cache_key, verify_tarball_integrity};
use crate::{
    PackageBin, PackageId, PackageManagerError, TarballFetchClient, TarballSource,
    binary_name_from_package_name, cache_key,
};

/// Distinct tarballs fetched and extracted at once.
pub const FETCH_CONCURRENCY: usize = 64;

/// Package directories materialized at once.
pub const MATERIALIZE_CONCURRENCY: usize = 16;

/// The machine's store of unpacked package tarballs, and the project
/// materializer over it.
#[derive(Debug, Clone)]
pub struct FsPackageStore {
    trees: TreeStore,
}

impl FsPackageStore {
    /// Create a package store under `cache_root`.
    #[must_use]
    pub fn new(cache_root: impl Into<PathBuf>) -> Self {
        let cache_root = cache_root.into();
        Self {
            trees: TreeStore::new(cache_root.join("store")),
        }
    }

    /// Create the package store shared by every project for this user.
    ///
    /// Sharing is the point: a package unpacked for one checkout is already
    /// stored for the next one, and their copies share its bytes.
    #[must_use]
    pub fn user_default() -> Self {
        Self::new(crate::user_cache_root())
    }

    /// Whether the tarball `source` names is already unpacked in the store.
    #[must_use]
    pub fn has_package(&self, source: &TarballSource) -> bool {
        self.trees.has_tree(&tarball_cache_key(source))
    }

    /// Fetch, verify, and unpack one package into the store.
    ///
    /// A package already in the store is reused without its tarball: its
    /// tree is named by the tarball's integrity. A fetched tarball is
    /// verified and unpacked from memory; it is never written to disk.
    pub async fn get_or_fetch_and_extract(
        &self,
        source: &TarballSource,
        client: &impl TarballFetchClient,
    ) -> Result<ExtractedPackage, PackageManagerError> {
        let key = tarball_cache_key(source);
        if self.trees.has_tree(&key) {
            return Ok(ExtractedPackage { key, reused: true });
        }
        let bytes = client.fetch_tarball(&source.url).await?;
        let trees = self.trees.clone();
        let source = source.clone();
        let url = source.url.clone();
        let stored_key = key.clone();
        tokio::task::spawn_blocking(move || {
            verify_tarball_integrity(&source, &bytes)?;
            let files = unpack_tgz(&bytes, &source.url)?;
            trees.store_tree(&stored_key, files)
        })
        .await
        .map_err(|err| PackageManagerError::Archive {
            path: PathBuf::from(&url),
            message: err.to_string(),
        })??;
        Ok(ExtractedPackage { key, reused: false })
    }

    /// Fetch and extract into the store every tarball announced on
    /// `announced`, up to [`FETCH_CONCURRENCY`] at a time, until the channel
    /// closes. Failures are left for materialization, which fetches what is
    /// still missing and reports why it cannot.
    pub async fn prefetch(
        &self,
        announced: tokio::sync::mpsc::UnboundedReceiver<TarballSource>,
        client: &impl TarballFetchClient,
    ) {
        let mut seen = std::collections::HashSet::new();
        stream::unfold(announced, |mut announced| async move {
            announced.recv().await.map(|source| (source, announced))
        })
        .filter(move |source| std::future::ready(seen.insert(tarball_cache_key(source))))
        .map(|source| async move {
            let _ = self.get_or_fetch_and_extract(&source, client).await;
        })
        .buffer_unordered(FETCH_CONCURRENCY)
        .collect::<()>()
        .await;
    }

    /// Materialize all registry tarballs from `lockfile` into `node_modules`.
    ///
    /// Each distinct tarball is fetched and extracted once, up to
    /// [`FETCH_CONCURRENCY`] at a time; package directories then materialize
    /// up to [`MATERIALIZE_CONCURRENCY`] at a time. Bins link afterwards, one
    /// package at a time, so two packages naming the same bin never race.
    pub async fn materialize_registry_packages(
        &self,
        project_root: impl AsRef<Path>,
        lockfile: &Lockfile,
        client: &impl TarballFetchClient,
    ) -> Result<Vec<InstalledPackage>, PackageManagerError> {
        let project_root = project_root.as_ref();
        let packages = tarball_packages_for_project(project_root, lockfile);
        let sources: BTreeMap<String, TarballSource> = packages
            .iter()
            .map(|(_, _, source)| (source.url.clone(), source.clone()))
            .collect();
        let extracted: HashMap<String, ExtractedPackage> = stream::iter(sources.into_values())
            .map(|source| async move {
                let extracted = self.get_or_fetch_and_extract(&source, client).await?;
                Ok::<_, PackageManagerError>((source.url, extracted))
            })
            .buffer_unordered(FETCH_CONCURRENCY)
            .try_collect()
            .await?;
        let extracted = &extracted;
        let mut installed: Vec<InstalledPackage> = stream::iter(packages)
            .map(|(id, package, source)| async move {
                let extracted = &extracted[&source.url];
                let install_root = crate::layout::package_dir(project_root, &id, &package.name);
                let marker = project_root
                    .join("node_modules")
                    .join(".otter-state")
                    .join(format!("{}.source", cache_key(&id)));
                // The tarball's integrity names exactly what it unpacks to.
                let fingerprint = &extracted.key;
                let reused_install =
                    existing_install_matches(&install_root, &marker, fingerprint).await?;
                if !reused_install {
                    self.materialize_install_root(
                        &extracted.key,
                        &install_root,
                        &marker,
                        fingerprint,
                    )
                    .await?;
                }
                Ok::<_, PackageManagerError>(InstalledPackage {
                    package_id: id,
                    name: package.name,
                    source,
                    installed_root: install_root,
                    reused_cache: extracted.reused,
                    reused_install,
                    linked_bins: 0,
                })
            })
            .buffer_unordered(MATERIALIZE_CONCURRENCY)
            .try_collect()
            .await?;
        // The project's own dependencies name its bins first; another
        // package's bin takes a name only while no earlier one holds it.
        let direct: BTreeSet<&str> = lockfile
            .packages
            .iter()
            .filter(|(id, _)| !crate::layout::is_stored(lockfile, id))
            .flat_map(|(_, package)| package.dependencies.values().map(String::as_str))
            .collect();
        installed.sort_by(|a, b| {
            (!direct.contains(a.package_id.as_str()), &a.package_id)
                .cmp(&(!direct.contains(b.package_id.as_str()), &b.package_id))
        });
        let mut taken = BTreeSet::new();
        for package in &mut installed {
            package.linked_bins = link_package_bins(
                project_root,
                &PackageId::new(&package.package_id),
                &package.installed_root,
                &mut taken,
            )
            .await?
            .len();
        }
        installed.sort_by(|a, b| a.package_id.cmp(&b.package_id));
        Ok(installed)
    }

    async fn materialize_install_root(
        &self,
        key: &str,
        install_root: &Path,
        marker: &Path,
        fingerprint: &str,
    ) -> Result<(), PackageManagerError> {
        let trees = self.trees.clone();
        let key = key.to_string();
        let install_root = install_root.to_path_buf();
        let install_root_for_error = install_root.clone();
        let marker = marker.to_path_buf();
        let fingerprint = fingerprint.to_string();
        tokio::task::spawn_blocking(move || {
            let temporary = install_root
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(format!(".otter-install-tmp-{}", std::process::id()));
            // The temporary directory sits beside the package's own
            // directory, which no other package shares.
            if temporary.exists() {
                std::fs::remove_dir_all(&temporary).map_err(|err| PackageManagerError::Io {
                    path: temporary.clone(),
                    message: err.to_string(),
                })?;
            }
            trees.materialize(&key, &temporary)?;
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
    /// The store key its tree is filed under.
    pub key: String,
    /// `true` when the package was already in the store.
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

/// The regular files of a package archive, with npm's leading root directory
/// stripped. Directory entries are not kept: a file's path implies every
/// directory above it. An archive naming a path twice keeps its last entry,
/// as unpacking it in order would.
fn unpack_tgz(archive: &[u8], url: &str) -> Result<Vec<StagedFile>, PackageManagerError> {
    let archive_path = Path::new(url);
    let archive_error = |err: std::io::Error| PackageManagerError::Archive {
        path: archive_path.to_path_buf(),
        message: err.to_string(),
    };
    let mut archive = tar::Archive::new(GzDecoder::new(archive));
    let mut files: Vec<StagedFile> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for entry in archive.entries().map_err(archive_error)? {
        let mut entry = entry.map_err(archive_error)?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let relative = archive_relative_path(&entry.path().map_err(archive_error)?)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let executable = entry.header().mode().is_ok_and(|mode| mode & 0o111 != 0);
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut bytes).map_err(archive_error)?;
        let file = StagedFile {
            path: package_path(&relative),
            bytes,
            executable,
        };
        match at.get(&file.path) {
            Some(&index) => files[index] = file,
            None => {
                at.insert(file.path.clone(), files.len());
                files.push(file);
            }
        }
    }
    Ok(files)
}

/// A package-relative path, always `/`-separated.
fn package_path(relative: &Path) -> String {
    relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// An entry's path inside the package. The archive's first directory is the
/// package root whatever it is called: `package/` by convention, the bare
/// name in some (`@types/*` ship `node/`, `estree/`, ...), as npm unpacks it.
fn archive_relative_path(path: &Path) -> Result<PathBuf, PackageManagerError> {
    let mut out = PathBuf::new();
    let mut root = true;
    for component in path.components() {
        match component {
            Component::Normal(_) if root => root = false,
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

/// Link the bins of the package at `package_root` whose names are not in
/// `taken`, and take them.
async fn link_package_bins(
    project_root: &Path,
    package_id: &PackageId,
    package_root: &Path,
    taken: &mut BTreeSet<String>,
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
    let mut bins = bin_entries(package_id, package_root, &manifest, bin_manifest);
    bins.retain(|bin| taken.insert(bin.name.clone()));
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
        // Windows will not execute a JavaScript file by name, and creating a
        // symbolic link needs a privilege an ordinary install does not have.
        // Two entries stand in for one link: the script itself, so a caller
        // that reads the entry finds the code, and a `.cmd` launcher beside
        // it, so a shell can run the tool by name the way npm's launchers do.
        tokio::fs::copy(&source, target)
            .await
            .map_err(|err| PackageManagerError::Io {
                path: target.to_path_buf(),
                message: err.to_string(),
            })?;
        let launcher = target.with_extension("cmd");
        let script = format!(
            "@ECHO off\r\nSETLOCAL\r\notter run \"%~dp0\\{}\" %*\r\n",
            target
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
        );
        tokio::fs::write(&launcher, script)
            .await
            .map_err(|err| PackageManagerError::Io {
                path: launcher,
                message: err.to_string(),
            })?;
        Ok(())
    }
}
