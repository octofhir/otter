//! Active package-manager graph and installer interfaces for Otter.
//!
//! This crate defines the public PM data model used by the runtime and CLI:
//! package graph roots, package ids, local package binaries, async registry
//! metadata/cache, registry HTTP clients, tarball cache/extraction, and the
//! initial deterministic install materializer.
//!
//! # Contents
//! - [`PackageId`] — stable package graph key.
//! - [`PackageRoot`] — resolved package root metadata.
//! - [`PackageBin`] — local executable exposed by a package.
//! - [`PackageGraph`] — read-only package graph model.
//! - [`PackageResolver`], [`PackageCache`], [`PackageInstaller`] — backend
//!   traits for later install slices.
//!
//! # Invariants
//! - Graph maps are [`std::collections::BTreeMap`] values for deterministic
//!   traversal and diagnostics.
//! - This crate has no dependency on `crates-legacy/*`, `otter-runtime`, or
//!   `otter-vm`.
//! - Capability gates are not part of first-party install command plumbing;
//!   they apply when runtime execution or future lifecycle script execution
//!   consumes the graph.
//!
//! # See also
//! - [`otter-pm-manifest`](../../otter-pm-manifest/src/lib.rs)
//! - [`otter-pm-lockfile`](../../otter-pm-lockfile/src/lib.rs)

mod installed_graph;

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Component;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use base64::Engine;
use flate2::read::GzDecoder;
use otter_pm_lockfile::{
    LifecycleMetadata, LockedPackage, Lockfile, ResolvedSource, ResolvedSourceKind, TrustState,
};
use otter_pm_manifest::{
    DependencySet, PACKAGE_JSON, PackageBinManifest, PackageManifest, discover_workspaces,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};

pub use installed_graph::{prune_removed_registry_packages, resolve_installed_project};

const DEFAULT_NPM_REGISTRY: &str = "https://registry.npmjs.org";

/// Package-manager error type.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PackageManagerError {
    /// Filesystem operation failed.
    #[error("package-manager I/O failed for `{path}`: {message}")]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying error message.
        message: String,
    },
    /// Manifest parse/discovery failed.
    #[error(transparent)]
    Manifest(#[from] otter_pm_manifest::ManifestError),
    /// Lockfile parse/serialize failed.
    #[error(transparent)]
    Lockfile(#[from] otter_pm_lockfile::LockfileError),
    /// Registry metadata JSON parse failed.
    #[error("invalid registry metadata for `{package}`: {message}")]
    RegistryMetadata {
        /// Package name.
        package: String,
        /// Parse or validation message.
        message: String,
    },
    /// Registry/tarball HTTP operation failed.
    #[error("HTTP fetch failed for `{url}`: {message}")]
    Http {
        /// Request URL.
        url: String,
        /// Error or HTTP status message.
        message: String,
    },
    /// Version range did not match any metadata entry.
    #[error("no registry version for `{package}` satisfies `{range}`")]
    NoMatchingVersion {
        /// Package name.
        package: String,
        /// Requested range.
        range: String,
    },
    /// Tarball bytes failed integrity verification.
    #[error("integrity verification failed for `{url}`: {message}")]
    Integrity {
        /// Tarball URL or source label.
        url: String,
        /// Failure message.
        message: String,
    },
    /// Archive extraction or package materialization failed.
    #[error("package archive operation failed for `{path}`: {message}")]
    Archive {
        /// Archive, cache, or install path involved.
        path: PathBuf,
        /// Failure message.
        message: String,
    },
    /// A requested package id is absent from the graph/cache.
    #[error("unknown package id `{0}`")]
    UnknownPackage(String),
    /// A requested binary is absent.
    #[error("unknown package binary `{0}`")]
    UnknownBinary(String),
    /// Backend implementation failed.
    #[error("package-manager backend `{backend}` failed: {message}")]
    Backend {
        /// Backend name.
        backend: &'static str,
        /// Failure message.
        message: String,
    },
}

/// Stable package graph key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PackageId(String);

impl PackageId {
    /// Build a package id from a stable string.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Build the root workspace package id.
    #[must_use]
    pub fn root_workspace(name: &str) -> Self {
        Self(format!("{name}@workspace:."))
    }

    /// Build a workspace package id.
    #[must_use]
    pub fn workspace(name: &str, relative_root: &Path) -> Self {
        Self(format!(
            "{name}@workspace:{}",
            relative_root.to_string_lossy().replace('\\', "/")
        ))
    }

    /// Build a local file package id.
    #[must_use]
    pub fn file(name: &str, relative_root: &Path) -> Self {
        Self(format!(
            "{name}@file:{}",
            relative_root.to_string_lossy().replace('\\', "/")
        ))
    }

    /// Build a registry range package id.
    #[must_use]
    pub fn registry(name: &str, range: &str) -> Self {
        Self(format!("{name}@npm:{range}"))
    }

    /// Borrow the id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PackageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A package root known to the package graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PackageRoot {
    /// Stable package id.
    pub id: PackageId,
    /// Package name.
    pub name: String,
    /// Package version or opaque source version.
    pub version: String,
    /// Filesystem root for this package.
    pub root: PathBuf,
    /// Parsed package manifest.
    pub manifest: PackageManifest,
}

/// A binary exposed by a package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageBin {
    /// Owning package id.
    pub package: PackageId,
    /// Binary command name.
    pub name: String,
    /// Executable path.
    pub path: PathBuf,
}

/// Read-only package graph model consumed by runtime resolution and CLI run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PackageGraph {
    /// Packages keyed by id.
    pub packages: BTreeMap<PackageId, PackageRoot>,
    /// Package dependencies keyed by source package id, then dependency name.
    pub dependencies: BTreeMap<PackageId, BTreeMap<String, PackageId>>,
    /// Local package binaries keyed by binary name.
    pub bins: BTreeMap<String, Vec<PackageBin>>,
}

impl PackageGraph {
    /// Construct an empty package graph.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a package root.
    pub fn insert_package(&mut self, root: PackageRoot) {
        self.packages.insert(root.id.clone(), root);
    }

    /// Insert a dependency edge.
    pub fn insert_dependency(
        &mut self,
        from: PackageId,
        name: impl Into<String>,
        target: PackageId,
    ) {
        self.dependencies
            .entry(from)
            .or_default()
            .insert(name.into(), target);
    }

    /// Insert a package binary.
    pub fn insert_bin(&mut self, bin: PackageBin) {
        let bins = self.bins.entry(bin.name.clone()).or_default();
        if !bins
            .iter()
            .any(|existing| existing.package == bin.package && existing.path == bin.path)
        {
            bins.push(bin);
        }
        for bins in self.bins.values_mut() {
            bins.sort_by(|a, b| a.package.cmp(&b.package).then(a.path.cmp(&b.path)));
        }
    }

    /// Resolve one package id.
    #[must_use]
    pub fn package(&self, id: &PackageId) -> Option<&PackageRoot> {
        self.packages.get(id)
    }

    /// Resolve binaries by command name.
    #[must_use]
    pub fn resolve_bin(&self, name: &str) -> &[PackageBin] {
        self.bins.get(name).map_or(&[], Vec::as_slice)
    }
}

/// Package-resolution request.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolveRequest {
    /// Project root.
    pub project_root: PathBuf,
    /// Root manifest.
    pub manifest: PackageManifest,
    /// Existing lockfile, when present.
    pub lockfile: Option<Lockfile>,
}

/// Package install request.
#[derive(Debug, Clone, PartialEq)]
pub struct InstallRequest {
    /// Project root.
    pub project_root: PathBuf,
    /// Graph to materialize.
    pub graph: PackageGraph,
    /// Lockfile to write or verify.
    pub lockfile: Lockfile,
}

/// Result of an install operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    /// Number of packages newly materialized.
    pub added_packages: usize,
    /// Number of packages reused from cache or existing install.
    pub reused_packages: usize,
    /// Number of package binaries linked into the project-local bin directory.
    pub linked_bins: usize,
    /// Whether the lockfile changed.
    pub lockfile_changed: bool,
}

/// Result of pruning packages no longer present in the lockfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneReport {
    /// Number of project-local package roots removed.
    pub removed_packages: usize,
    /// Number of project-local binary links removed.
    pub removed_bins: usize,
}

/// Result of local project graph/lockfile resolution.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalResolution {
    /// Read-only graph for runtime/CLI consumers.
    pub graph: PackageGraph,
    /// Deterministic lockfile model.
    pub lockfile: Lockfile,
}

/// npm registry metadata subset needed by Otter package resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NpmRegistryMetadata {
    /// Package name.
    pub name: String,
    /// npm dist-tags, for example `latest`.
    #[serde(rename = "dist-tags", default)]
    pub dist_tags: BTreeMap<String, String>,
    /// Published versions keyed by semver version.
    #[serde(default)]
    pub versions: BTreeMap<String, NpmPackageVersion>,
}

/// One npm package version metadata entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NpmPackageVersion {
    /// Package name.
    pub name: String,
    /// Package version.
    pub version: String,
    /// Runtime dependencies.
    #[serde(default)]
    pub dependencies: DependencySet,
    /// Peer dependencies.
    #[serde(rename = "peerDependencies", default)]
    pub peer_dependencies: DependencySet,
    /// Optional dependencies.
    #[serde(rename = "optionalDependencies", default)]
    pub optional_dependencies: DependencySet,
    /// Package binaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<PackageBinManifest>,
    /// Lifecycle scripts.
    #[serde(default)]
    pub scripts: BTreeMap<String, String>,
    /// Distribution metadata.
    #[serde(default)]
    pub dist: NpmDist,
}

/// npm `dist` metadata for a version.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NpmDist {
    /// Tarball URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tarball: Option<String>,
    /// Subresource integrity string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrity: Option<String>,
    /// Legacy SHA1 shasum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shasum: Option<String>,
}

/// Source of npm registry metadata.
pub trait RegistryMetadataClient {
    /// Fetch metadata for `package` without blocking the async runtime.
    fn fetch_metadata<'a>(
        &'a self,
        package: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<NpmRegistryMetadata, PackageManagerError>> + Send + 'a>>;
}

/// Deterministic filesystem cache for npm registry metadata.
#[derive(Debug, Clone)]
pub struct FsRegistryMetadataCache {
    root: PathBuf,
}

impl FsRegistryMetadataCache {
    /// Create a metadata cache rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Return the cache file path for one package.
    #[must_use]
    pub fn metadata_path(&self, package: &str) -> PathBuf {
        self.root.join(format!("{}.json", cache_key(package)))
    }

    /// Read cached metadata if present.
    pub async fn read(
        &self,
        package: &str,
    ) -> Result<Option<NpmRegistryMetadata>, PackageManagerError> {
        let path = self.metadata_path(package);
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(PackageManagerError::Io {
                    path,
                    message: err.to_string(),
                });
            }
        };
        parse_registry_metadata(package, &text).map(Some)
    }

    /// Write metadata into the cache using stable JSON formatting.
    pub async fn write(&self, metadata: &NpmRegistryMetadata) -> Result<(), PackageManagerError> {
        tokio::fs::create_dir_all(&self.root)
            .await
            .map_err(|err| PackageManagerError::Io {
                path: self.root.clone(),
                message: err.to_string(),
            })?;
        let path = self.metadata_path(&metadata.name);
        let mut text = serde_json::to_string_pretty(metadata).map_err(|err| {
            PackageManagerError::RegistryMetadata {
                package: metadata.name.clone(),
                message: err.to_string(),
            }
        })?;
        text.push('\n');
        tokio::fs::write(&path, text)
            .await
            .map_err(|err| PackageManagerError::Io {
                path,
                message: err.to_string(),
            })
    }

    /// Read from cache, otherwise fetch through `client` and cache the result.
    pub async fn get_or_fetch(
        &self,
        package: &str,
        client: &impl RegistryMetadataClient,
    ) -> Result<NpmRegistryMetadata, PackageManagerError> {
        if let Some(metadata) = self.read(package).await? {
            return Ok(metadata);
        }
        let metadata = client.fetch_metadata(package).await?;
        self.write(&metadata).await?;
        Ok(metadata)
    }
}

/// File-backed registry client for deterministic tests and offline fixtures.
#[derive(Debug, Clone)]
pub struct FileRegistryMetadataClient {
    root: PathBuf,
}

impl FileRegistryMetadataClient {
    /// Create a file-backed metadata client rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, package: &str) -> PathBuf {
        self.root.join(format!("{}.json", cache_key(package)))
    }
}

impl RegistryMetadataClient for FileRegistryMetadataClient {
    fn fetch_metadata<'a>(
        &'a self,
        package: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<NpmRegistryMetadata, PackageManagerError>> + Send + 'a>>
    {
        Box::pin(async move {
            let path = self.path(package);
            let text =
                tokio::fs::read_to_string(&path)
                    .await
                    .map_err(|err| PackageManagerError::Io {
                        path,
                        message: err.to_string(),
                    })?;
            parse_registry_metadata(package, &text)
        })
    }
}

/// Async HTTP-backed npm registry metadata client.
#[derive(Debug, Clone)]
pub struct HttpRegistryMetadataClient {
    client: reqwest::Client,
    registry_base: String,
}

impl HttpRegistryMetadataClient {
    /// Build a client against the default npm registry.
    #[must_use]
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_NPM_REGISTRY)
    }

    /// Build a client against an explicit registry base URL.
    #[must_use]
    pub fn with_base_url(registry_base: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            registry_base: registry_base.into().trim_end_matches('/').to_string(),
        }
    }

    fn metadata_url(&self, package: &str) -> String {
        format!(
            "{}/{}",
            self.registry_base,
            npm_registry_package_path(package)
        )
    }
}

impl Default for HttpRegistryMetadataClient {
    fn default() -> Self {
        Self::new()
    }
}

impl RegistryMetadataClient for HttpRegistryMetadataClient {
    fn fetch_metadata<'a>(
        &'a self,
        package: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<NpmRegistryMetadata, PackageManagerError>> + Send + 'a>>
    {
        Box::pin(async move {
            let url = self.metadata_url(package);
            let response = self
                .client
                .get(&url)
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await
                .map_err(|err| PackageManagerError::Http {
                    url: url.clone(),
                    message: err.to_string(),
                })?;
            let status = response.status();
            if !status.is_success() {
                return Err(PackageManagerError::Http {
                    url,
                    message: format!("registry returned {status}"),
                });
            }
            let text = response
                .text()
                .await
                .map_err(|err| PackageManagerError::Http {
                    url: url.clone(),
                    message: err.to_string(),
                })?;
            parse_registry_metadata(package, &text)
        })
    }
}

/// Tarball source selected from registry metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TarballSource {
    /// Tarball URL.
    pub url: String,
    /// Optional SRI integrity string.
    pub integrity: Option<String>,
}

/// Tarball cache result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedTarball {
    /// Content-addressed local path.
    pub path: PathBuf,
    /// Tarball size in bytes.
    pub bytes: u64,
    /// `true` when a cache hit avoided fetch/write work.
    pub reused: bool,
}

/// Source of tarball bytes.
pub trait TarballFetchClient {
    /// Fetch tarball bytes from `url` without blocking the async runtime.
    fn fetch_tarball<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, PackageManagerError>> + Send + 'a>>;
}

/// File-backed tarball client for deterministic offline tests.
#[derive(Debug, Clone)]
pub struct FileTarballClient {
    root: PathBuf,
}

impl FileTarballClient {
    /// Create a file-backed tarball client rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, url: &str) -> PathBuf {
        self.root.join(cache_key(url))
    }
}

impl TarballFetchClient for FileTarballClient {
    fn fetch_tarball<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, PackageManagerError>> + Send + 'a>> {
        Box::pin(async move {
            let path = self.path(url);
            tokio::fs::read(&path)
                .await
                .map_err(|err| PackageManagerError::Io {
                    path,
                    message: err.to_string(),
                })
        })
    }
}

/// Async HTTP-backed tarball fetch client.
#[derive(Debug, Clone, Default)]
pub struct HttpTarballClient {
    client: reqwest::Client,
}

impl HttpTarballClient {
    /// Build an HTTP tarball client.
    #[must_use]
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl TarballFetchClient for HttpTarballClient {
    fn fetch_tarball<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, PackageManagerError>> + Send + 'a>> {
        Box::pin(async move {
            let response =
                self.client
                    .get(url)
                    .send()
                    .await
                    .map_err(|err| PackageManagerError::Http {
                        url: url.to_string(),
                        message: err.to_string(),
                    })?;
            let status = response.status();
            if !status.is_success() {
                return Err(PackageManagerError::Http {
                    url: url.to_string(),
                    message: format!("registry returned {status}"),
                });
            }
            response
                .bytes()
                .await
                .map(|bytes| bytes.to_vec())
                .map_err(|err| PackageManagerError::Http {
                    url: url.to_string(),
                    message: err.to_string(),
                })
        })
    }
}

/// Content-addressed tarball cache.
#[derive(Debug, Clone)]
pub struct FsTarballCache {
    root: PathBuf,
}

impl FsTarballCache {
    /// Create a tarball cache rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Return the deterministic cache path for a tarball source.
    #[must_use]
    pub fn tarball_path(&self, source: &TarballSource) -> PathBuf {
        self.root.join(tarball_cache_key(source))
    }

    /// Cache-first tarball fetch path. This is the install hot path:
    /// existing verified cache entries avoid fetches and writes.
    pub async fn get_or_fetch(
        &self,
        source: &TarballSource,
        client: &impl TarballFetchClient,
    ) -> Result<CachedTarball, PackageManagerError> {
        tokio::fs::create_dir_all(&self.root)
            .await
            .map_err(|err| PackageManagerError::Io {
                path: self.root.clone(),
                message: err.to_string(),
            })?;
        let path = self.tarball_path(source);
        if tokio::fs::try_exists(&path)
            .await
            .map_err(|err| PackageManagerError::Io {
                path: path.clone(),
                message: err.to_string(),
            })?
        {
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|err| PackageManagerError::Io {
                    path: path.clone(),
                    message: err.to_string(),
                })?;
            verify_tarball_integrity(source, &bytes)?;
            return Ok(CachedTarball {
                path,
                bytes: bytes.len() as u64,
                reused: true,
            });
        }

        let bytes = client.fetch_tarball(&source.url).await?;
        verify_tarball_integrity(source, &bytes)?;
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        tokio::fs::write(&tmp, &bytes)
            .await
            .map_err(|err| PackageManagerError::Io {
                path: tmp.clone(),
                message: err.to_string(),
            })?;
        tokio::fs::rename(&tmp, &path)
            .await
            .map_err(|err| PackageManagerError::Io {
                path: path.clone(),
                message: err.to_string(),
            })?;
        Ok(CachedTarball {
            path,
            bytes: bytes.len() as u64,
            reused: false,
        })
    }
}

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
        let mut packages = registry_tarball_packages(lockfile);
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

/// Resolve, cache metadata, download/extract registry tarballs, materialize
/// `node_modules`, and write a deterministic `otter-lock`.
pub async fn install_local_project(
    project_root: impl AsRef<Path>,
    metadata_cache: &FsRegistryMetadataCache,
    metadata_client: &impl RegistryMetadataClient,
    package_store: &FsPackageStore,
    tarball_client: &impl TarballFetchClient,
) -> Result<InstallReport, PackageManagerError> {
    let project_root = project_root.as_ref();
    let resolution =
        resolve_local_project_with_registry_metadata(project_root, metadata_cache, metadata_client)
            .await?;
    let lockfile_changed = write_lockfile_if_changed(project_root, &resolution.lockfile).await?;
    let installed = package_store
        .materialize_registry_packages(project_root, &resolution.lockfile, tarball_client)
        .await?;
    Ok(InstallReport {
        added_packages: installed
            .iter()
            .filter(|package| !package.reused_install)
            .count(),
        reused_packages: installed
            .iter()
            .filter(|package| package.reused_install)
            .count(),
        linked_bins: installed.iter().map(|package| package.linked_bins).sum(),
        lockfile_changed,
    })
}

/// Resolve the current local project into a deterministic graph and
/// lockfile. Registry dependencies are recorded as desired graph entries; no
/// network fetch or tarball extraction happens in this slice.
pub async fn resolve_local_project(
    project_root: impl AsRef<Path>,
) -> Result<LocalResolution, PackageManagerError> {
    let project_root = project_root.as_ref();
    let root_manifest = PackageManifest::read_from_dir(project_root).await?;
    let mut graph = PackageGraph::new();
    let mut lockfile = Lockfile::new();
    let root_name = root_manifest
        .name
        .clone()
        .unwrap_or_else(|| "root".to_string());
    let root_version = root_manifest
        .version
        .clone()
        .unwrap_or_else(|| "0.0.0".to_string());
    let root_id = PackageId::root_workspace(&root_name);
    let workspaces = discover_workspaces(project_root).await?;
    let mut workspace_by_name = BTreeMap::new();
    for workspace in &workspaces {
        if let Some(name) = &workspace.manifest.name {
            workspace_by_name.insert(name.clone(), workspace);
        }
    }

    graph.insert_package(PackageRoot {
        id: root_id.clone(),
        name: root_name.clone(),
        version: root_version.clone(),
        root: project_root.to_path_buf(),
        manifest: root_manifest.clone(),
    });
    insert_bins_for_manifest(&mut graph, &root_id, project_root, &root_manifest);
    lockfile.packages.insert(
        root_id.to_string(),
        locked_package(
            &root_name,
            &root_version,
            ResolvedSourceKind::Workspace,
            ".",
            &root_manifest,
        ),
    );

    for workspace in &workspaces {
        let Some(name) = workspace.manifest.name.clone() else {
            continue;
        };
        let version = workspace
            .manifest
            .version
            .clone()
            .unwrap_or_else(|| "0.0.0".to_string());
        let id = PackageId::workspace(&name, &workspace.relative_root);
        graph.insert_package(PackageRoot {
            id: id.clone(),
            name: name.clone(),
            version: version.clone(),
            root: workspace.root.clone(),
            manifest: workspace.manifest.clone(),
        });
        insert_bins_for_manifest(&mut graph, &id, &workspace.root, &workspace.manifest);
        lockfile.packages.insert(
            id.to_string(),
            locked_package(
                &name,
                &version,
                ResolvedSourceKind::Workspace,
                &workspace.relative_root.to_string_lossy().replace('\\', "/"),
                &workspace.manifest,
            ),
        );
    }

    resolve_manifest_dependencies(
        project_root,
        &mut graph,
        &mut lockfile,
        &root_id,
        &root_manifest,
        &workspace_by_name,
    )
    .await?;
    for workspace in &workspaces {
        if let Some(name) = &workspace.manifest.name {
            let id = PackageId::workspace(name, &workspace.relative_root);
            resolve_manifest_dependencies(
                project_root,
                &mut graph,
                &mut lockfile,
                &id,
                &workspace.manifest,
                &workspace_by_name,
            )
            .await?;
        }
    }

    Ok(LocalResolution { graph, lockfile })
}

/// Write a deterministic `otter-lock` for the local project and return whether
/// the bytes changed.
pub async fn write_local_lockfile(
    project_root: impl AsRef<Path>,
) -> Result<bool, PackageManagerError> {
    let project_root = project_root.as_ref();
    let resolution = resolve_local_project(project_root).await?;
    write_lockfile_if_changed(project_root, &resolution.lockfile).await
}

/// Resolve a local project with registry metadata enrichment, write
/// deterministic `otter-lock`, and return whether the bytes changed.
pub async fn write_local_lockfile_with_registry_metadata(
    project_root: impl AsRef<Path>,
    cache: &FsRegistryMetadataCache,
    client: &impl RegistryMetadataClient,
) -> Result<bool, PackageManagerError> {
    let project_root = project_root.as_ref();
    let resolution =
        resolve_local_project_with_registry_metadata(project_root, cache, client).await?;
    write_lockfile_if_changed(project_root, &resolution.lockfile).await
}

async fn write_lockfile_if_changed(
    project_root: &Path,
    lockfile: &Lockfile,
) -> Result<bool, PackageManagerError> {
    let text = lockfile.to_toml_string()?;
    let path = project_root.join(otter_pm_lockfile::LOCKFILE_NAME);
    let previous = match tokio::fs::read_to_string(&path).await {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(PackageManagerError::Io {
                path,
                message: err.to_string(),
            });
        }
    };
    if previous.as_deref() == Some(text.as_str()) {
        return Ok(false);
    }
    tokio::fs::write(&path, text)
        .await
        .map_err(|err| PackageManagerError::Io {
            path,
            message: err.to_string(),
        })?;
    Ok(true)
}

/// Resolve a local project, fetch/cache registry metadata, and enrich registry
/// lockfile entries with selected version, tarball, integrity, scripts, and
/// bin metadata. This does not download tarballs or execute lifecycle scripts.
pub async fn resolve_local_project_with_registry_metadata(
    project_root: impl AsRef<Path>,
    cache: &FsRegistryMetadataCache,
    client: &impl RegistryMetadataClient,
) -> Result<LocalResolution, PackageManagerError> {
    let mut resolution = resolve_local_project(project_root).await?;
    enrich_resolution_with_registry_metadata(&mut resolution, cache, client).await?;
    Ok(resolution)
}

/// Enrich an existing local resolution with registry metadata.
pub async fn enrich_resolution_with_registry_metadata(
    resolution: &mut LocalResolution,
    cache: &FsRegistryMetadataCache,
    client: &impl RegistryMetadataClient,
) -> Result<(), PackageManagerError> {
    let project_root = infer_project_root(&resolution.graph);
    let mut processed = BTreeMap::new();
    while let Some((id, name, range)) =
        next_unresolved_registry_package(&resolution.lockfile, &processed)
    {
        processed.insert(id.clone(), ());
        let metadata = cache.get_or_fetch(&name, client).await?;
        let version = select_registry_version(&metadata, &range)?;
        let graph_id = PackageId::new(id.clone());
        let mut dependency_edges = Vec::new();
        let package = resolution
            .lockfile
            .packages
            .get_mut(&id)
            .ok_or_else(|| PackageManagerError::UnknownPackage(id.clone()))?;
        package.version = version.version.clone();
        package.integrity = version
            .dist
            .integrity
            .clone()
            .or_else(|| version.dist.shasum.as_ref().map(|s| format!("sha1-{s}")));
        package.resolved = version.dist.tarball.as_ref().map(|tarball| ResolvedSource {
            kind: ResolvedSourceKind::Registry,
            reference: tarball.clone(),
        });
        package.lifecycle = LifecycleMetadata {
            scripts: version.scripts.clone(),
            trust: TrustState::Untrusted,
        };
        for (dep_name, dep_range) in &version.dependencies {
            let dep_id = PackageId::registry(dep_name, dep_range);
            package
                .dependencies
                .entry(dep_name.clone())
                .or_insert_with(|| dep_id.to_string());
            dependency_edges.push((dep_name.clone(), dep_range.clone(), dep_id));
        }

        if let Some(root) = resolution.graph.packages.get_mut(&graph_id) {
            root.version = version.version.clone();
            root.manifest.version = Some(version.version.clone());
            root.manifest.dependencies = version.dependencies.clone();
            root.manifest.peer_dependencies = version.peer_dependencies.clone();
            root.manifest.optional_dependencies = version.optional_dependencies.clone();
            root.manifest.bin = version.bin.clone();
            root.manifest.scripts = version.scripts.clone();
            let root_path = root.root.clone();
            let manifest = root.manifest.clone();
            insert_bins_for_manifest(&mut resolution.graph, &graph_id, &root_path, &manifest);
        }
        for (dep_name, dep_range, dep_id) in dependency_edges {
            ensure_registry_package(
                &project_root,
                &mut resolution.graph,
                &mut resolution.lockfile,
                &dep_id,
                &dep_name,
                &dep_range,
            );
            resolution
                .graph
                .insert_dependency(graph_id.clone(), dep_name, dep_id);
        }
    }
    Ok(())
}

/// Fetch all registry tarballs referenced by an enriched lockfile into a
/// content-addressed cache.
pub async fn cache_registry_tarballs(
    lockfile: &Lockfile,
    cache: &FsTarballCache,
    client: &impl TarballFetchClient,
) -> Result<Vec<CachedTarball>, PackageManagerError> {
    let mut sources = registry_tarball_packages(lockfile)
        .into_iter()
        .map(|(_, _, source)| source)
        .collect::<Vec<_>>();
    sources.sort_by(|a, b| a.url.cmp(&b.url));
    sources.dedup_by(|a, b| a.url == b.url && a.integrity == b.integrity);

    let mut cached = Vec::with_capacity(sources.len());
    for source in &sources {
        cached.push(cache.get_or_fetch(source, client).await?);
    }
    Ok(cached)
}

fn registry_tarball_packages(lockfile: &Lockfile) -> Vec<(String, LockedPackage, TarballSource)> {
    lockfile
        .packages
        .iter()
        .filter_map(|(id, package)| match &package.resolved {
            Some(ResolvedSource {
                kind: ResolvedSourceKind::Registry,
                reference,
            }) if is_tarball_reference(reference) => Some((
                id.clone(),
                package.clone(),
                TarballSource {
                    url: reference.clone(),
                    integrity: package.integrity.clone(),
                },
            )),
            _ => None,
        })
        .collect()
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

/// Resolver backend interface.
pub trait PackageResolver {
    /// Resolve a project manifest into a package graph and lockfile.
    fn resolve<'a>(
        &'a self,
        request: ResolveRequest,
    ) -> Pin<
        Box<dyn Future<Output = Result<(PackageGraph, Lockfile), PackageManagerError>> + Send + 'a>,
    >;
}

fn parse_registry_metadata(
    package: &str,
    text: &str,
) -> Result<NpmRegistryMetadata, PackageManagerError> {
    serde_json::from_str(text).map_err(|err| PackageManagerError::RegistryMetadata {
        package: package.to_string(),
        message: err.to_string(),
    })
}

fn select_registry_version(
    metadata: &NpmRegistryMetadata,
    range: &str,
) -> Result<NpmPackageVersion, PackageManagerError> {
    if let Some(version) = metadata.versions.get(range) {
        return Ok(version.clone());
    }
    if matches!(range, "*" | "latest") {
        if let Some(latest) = metadata.dist_tags.get("latest") {
            if let Some(version) = metadata.versions.get(latest) {
                return Ok(version.clone());
            }
        }
    }
    if let Some(version) = select_semver_version(metadata, range) {
        return Ok(version);
    }
    Err(PackageManagerError::NoMatchingVersion {
        package: metadata.name.clone(),
        range: range.to_string(),
    })
}

fn select_semver_version(metadata: &NpmRegistryMetadata, range: &str) -> Option<NpmPackageVersion> {
    let req = normalize_npm_range(range)
        .and_then(|normalized| semver::VersionReq::parse(&normalized).ok())?;
    let mut versions = metadata
        .versions
        .keys()
        .filter_map(|version| semver::Version::parse(version).ok())
        .filter(|version| req.matches(version))
        .collect::<Vec<_>>();
    versions.sort();
    versions
        .pop()
        .and_then(|version| metadata.versions.get(&version.to_string()).cloned())
}

fn normalize_npm_range(range: &str) -> Option<String> {
    let trimmed = range.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed {
        "*" | "latest" => Some("*".to_string()),
        value if value.starts_with('^') || value.starts_with('~') => Some(value.to_string()),
        value if value.chars().next().is_some_and(|c| c.is_ascii_digit()) => {
            Some(format!("={value}"))
        }
        value => Some(value.to_string()),
    }
}

fn next_unresolved_registry_package(
    lockfile: &Lockfile,
    processed: &BTreeMap<String, ()>,
) -> Option<(String, String, String)> {
    lockfile.packages.iter().find_map(|(id, package)| {
        if processed.contains_key(id) {
            return None;
        }
        match &package.resolved {
            Some(ResolvedSource {
                kind: ResolvedSourceKind::Registry,
                reference,
            }) if !is_tarball_reference(reference) => {
                Some((id.clone(), package.name.clone(), reference.clone()))
            }
            _ => None,
        }
    })
}

fn infer_project_root(graph: &PackageGraph) -> PathBuf {
    graph
        .packages
        .iter()
        .find_map(|(id, package)| {
            if id.as_str().ends_with("@workspace:.") {
                Some(package.root.clone())
            } else {
                None
            }
        })
        .or_else(|| {
            graph
                .packages
                .values()
                .next()
                .map(|package| package.root.clone())
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

fn is_tarball_reference(reference: &str) -> bool {
    reference.ends_with(".tgz")
        || reference.ends_with(".tar.gz")
        || reference.starts_with("http://")
        || reference.starts_with("https://")
}

fn npm_registry_package_path(package: &str) -> String {
    if let Some((scope, name)) = package.split_once('/') {
        if scope.starts_with('@') {
            return format!("{scope}%2f{name}");
        }
    }
    cache_key(package)
}

fn install_fingerprint(source: &TarballSource) -> String {
    format!(
        "{}\n{}\n",
        source.url,
        source.integrity.as_deref().unwrap_or("")
    )
}

pub(crate) fn cache_key(package: &str) -> String {
    let mut out = String::new();
    for byte in package.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'-' | b'_' => {
                out.push(char::from(byte));
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn tarball_cache_key(source: &TarballSource) -> String {
    if let Some(integrity) = &source.integrity {
        return cache_key(integrity);
    }
    format!("url-{}", cache_key(&source.url))
}

fn verify_tarball_integrity(
    source: &TarballSource,
    bytes: &[u8],
) -> Result<(), PackageManagerError> {
    let Some(integrity) = &source.integrity else {
        return Ok(());
    };
    for part in integrity.split_whitespace() {
        if verify_one_integrity(part, bytes).unwrap_or(false) {
            return Ok(());
        }
    }
    Err(PackageManagerError::Integrity {
        url: source.url.clone(),
        message: "no supported digest matched".to_string(),
    })
}

fn verify_one_integrity(integrity: &str, bytes: &[u8]) -> Result<bool, PackageManagerError> {
    let Some((algorithm, encoded)) = integrity.split_once('-') else {
        return Ok(false);
    };
    let expected = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|err| PackageManagerError::Integrity {
            url: integrity.to_string(),
            message: err.to_string(),
        })?;
    let actual = match algorithm {
        "sha512" => Sha512::digest(bytes).to_vec(),
        "sha256" => Sha256::digest(bytes).to_vec(),
        _ => return Ok(false),
    };
    Ok(actual == expected)
}

async fn resolve_manifest_dependencies(
    project_root: &Path,
    graph: &mut PackageGraph,
    lockfile: &mut Lockfile,
    from: &PackageId,
    manifest: &PackageManifest,
    workspace_by_name: &BTreeMap<String, &otter_pm_manifest::WorkspacePackage>,
) -> Result<(), PackageManagerError> {
    for (_, dependencies) in manifest.dependency_buckets() {
        resolve_dependency_bucket(
            project_root,
            graph,
            lockfile,
            from,
            dependencies,
            workspace_by_name,
        )
        .await?;
    }
    Ok(())
}

async fn resolve_dependency_bucket(
    project_root: &Path,
    graph: &mut PackageGraph,
    lockfile: &mut Lockfile,
    from: &PackageId,
    dependencies: &DependencySet,
    workspace_by_name: &BTreeMap<String, &otter_pm_manifest::WorkspacePackage>,
) -> Result<(), PackageManagerError> {
    for (name, range) in dependencies {
        let target = if range.starts_with("workspace:") {
            workspace_by_name
                .get(name)
                .map(|workspace| PackageId::workspace(name, &workspace.relative_root))
                .unwrap_or_else(|| PackageId::registry(name, range))
        } else if let Some(file_path) = range.strip_prefix("file:") {
            resolve_file_dependency(project_root, graph, lockfile, name, file_path).await?
        } else {
            let id = PackageId::registry(name, range);
            ensure_registry_package(project_root, graph, lockfile, &id, name, range);
            id
        };
        graph.insert_dependency(from.clone(), name.clone(), target.clone());
        if let Some(package) = lockfile.packages.get_mut(from.as_str()) {
            package
                .dependencies
                .insert(name.clone(), target.to_string());
        }
    }
    Ok(())
}

async fn resolve_file_dependency(
    project_root: &Path,
    graph: &mut PackageGraph,
    lockfile: &mut Lockfile,
    dependency_name: &str,
    file_path: &str,
) -> Result<PackageId, PackageManagerError> {
    let package_root = project_root.join(file_path);
    let manifest_path = package_root.join(PACKAGE_JSON);
    if !tokio::fs::try_exists(&manifest_path)
        .await
        .map_err(|err| PackageManagerError::Io {
            path: manifest_path.clone(),
            message: err.to_string(),
        })?
    {
        let id = PackageId::file(dependency_name, Path::new(file_path));
        ensure_file_package(
            graph,
            lockfile,
            &id,
            dependency_name,
            "0.0.0",
            &package_root,
            file_path,
            &PackageManifest::default(),
        );
        return Ok(id);
    }
    let manifest = PackageManifest::read_from_dir(&package_root).await?;
    let name = manifest
        .name
        .clone()
        .unwrap_or_else(|| dependency_name.to_string());
    let version = manifest
        .version
        .clone()
        .unwrap_or_else(|| "0.0.0".to_string());
    let id = PackageId::file(&name, Path::new(file_path));
    ensure_file_package(
        graph,
        lockfile,
        &id,
        &name,
        &version,
        &package_root,
        file_path,
        &manifest,
    );
    Ok(id)
}

fn ensure_registry_package(
    project_root: &Path,
    graph: &mut PackageGraph,
    lockfile: &mut Lockfile,
    id: &PackageId,
    name: &str,
    range: &str,
) {
    if graph.packages.contains_key(id) {
        return;
    }
    let manifest = PackageManifest {
        name: Some(name.to_string()),
        version: Some(range.to_string()),
        ..PackageManifest::default()
    };
    graph.insert_package(PackageRoot {
        id: id.clone(),
        name: name.to_string(),
        version: range.to_string(),
        root: project_root
            .join("node_modules")
            .join(package_name_path(name)),
        manifest: manifest.clone(),
    });
    lockfile.packages.insert(
        id.to_string(),
        locked_package(name, range, ResolvedSourceKind::Registry, range, &manifest),
    );
}

fn ensure_file_package(
    graph: &mut PackageGraph,
    lockfile: &mut Lockfile,
    id: &PackageId,
    name: &str,
    version: &str,
    root: &Path,
    reference: &str,
    manifest: &PackageManifest,
) {
    if !graph.packages.contains_key(id) {
        graph.insert_package(PackageRoot {
            id: id.clone(),
            name: name.to_string(),
            version: version.to_string(),
            root: root.to_path_buf(),
            manifest: manifest.clone(),
        });
        insert_bins_for_manifest(graph, id, root, manifest);
    }
    lockfile.packages.entry(id.to_string()).or_insert_with(|| {
        locked_package(name, version, ResolvedSourceKind::File, reference, manifest)
    });
}

fn locked_package(
    name: &str,
    version: &str,
    kind: ResolvedSourceKind,
    reference: &str,
    manifest: &PackageManifest,
) -> LockedPackage {
    LockedPackage {
        name: name.to_string(),
        version: version.to_string(),
        dependencies: BTreeMap::new(),
        integrity: None,
        resolved: Some(ResolvedSource {
            kind,
            reference: reference.to_string(),
        }),
        lifecycle: LifecycleMetadata {
            scripts: manifest.scripts.clone(),
            trust: TrustState::Untrusted,
        },
    }
}

fn insert_bins_for_manifest(
    graph: &mut PackageGraph,
    package_id: &PackageId,
    package_root: &Path,
    manifest: &PackageManifest,
) {
    let Some(bin) = &manifest.bin else {
        return;
    };
    match bin {
        PackageBinManifest::Path(path) => {
            if let Some(name) = &manifest.name {
                graph.insert_bin(PackageBin {
                    package: package_id.clone(),
                    name: binary_name_from_package_name(name).to_string(),
                    path: package_root.join(path),
                });
            }
        }
        PackageBinManifest::Map(bins) => {
            for (name, path) in bins {
                graph.insert_bin(PackageBin {
                    package: package_id.clone(),
                    name: name.clone(),
                    path: package_root.join(path),
                });
            }
        }
    }
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

pub(crate) fn binary_name_from_package_name(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

pub(crate) fn package_name_path(name: &str) -> PathBuf {
    let mut path = PathBuf::new();
    for segment in name.split('/') {
        path.push(segment);
    }
    path
}

/// Content-addressed package cache interface.
pub trait PackageCache {
    /// Return a cached package root for `id`, when present.
    fn get<'a>(
        &'a self,
        id: &'a PackageId,
    ) -> Pin<Box<dyn Future<Output = Result<Option<PackageRoot>, PackageManagerError>> + Send + 'a>>;

    /// Store a package root in the cache.
    fn put<'a>(
        &'a self,
        root: &'a PackageRoot,
    ) -> Pin<Box<dyn Future<Output = Result<(), PackageManagerError>> + Send + 'a>>;
}

/// Install pipeline backend interface.
pub trait PackageInstaller {
    /// Materialize an install request.
    fn install<'a>(
        &'a self,
        request: InstallRequest,
    ) -> Pin<Box<dyn Future<Output = Result<InstallReport, PackageManagerError>> + Send + 'a>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(name: &str) -> PackageManifest {
        PackageManifest {
            name: Some(name.to_string()),
            version: Some("1.0.0".to_string()),
            ..PackageManifest::default()
        }
    }

    #[test]
    fn graph_resolves_packages_and_bins_deterministically() {
        let mut graph = PackageGraph::new();
        let app = PackageId::new("app@workspace:.");
        let tool = PackageId::new("tool@1.0.0");
        graph.insert_package(PackageRoot {
            id: app.clone(),
            name: "app".to_string(),
            version: "0.1.0".to_string(),
            root: PathBuf::from("."),
            manifest: manifest("app"),
        });
        graph.insert_package(PackageRoot {
            id: tool.clone(),
            name: "tool".to_string(),
            version: "1.0.0".to_string(),
            root: PathBuf::from("node_modules/tool"),
            manifest: manifest("tool"),
        });
        graph.insert_dependency(app.clone(), "tool", tool.clone());
        graph.insert_bin(PackageBin {
            package: tool.clone(),
            name: "tool".to_string(),
            path: PathBuf::from("node_modules/.bin/tool"),
        });

        assert_eq!(graph.package(&app).unwrap().name, "app");
        assert_eq!(
            graph.dependencies[&app]["tool"],
            PackageId::new("tool@1.0.0")
        );
        assert_eq!(graph.resolve_bin("tool")[0].package, tool);
        assert!(graph.resolve_bin("missing").is_empty());
    }

    #[tokio::test]
    async fn local_project_resolution_records_workspace_file_and_registry_deps() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join("package.json"),
            r#"{
              "name": "app",
              "version": "0.1.0",
              "workspaces": ["packages/*"],
              "dependencies": {
                "lib": "workspace:*",
                "file-tool": "file:tools/file-tool",
                "left-pad": "^1.3.0"
              }
            }"#,
        )
        .await;
        write(
            &tmp.path().join("packages/lib/package.json"),
            r#"{"name":"lib","version":"1.0.0","bin":{"lib":"./bin.js"}}"#,
        )
        .await;
        write(
            &tmp.path().join("tools/file-tool/package.json"),
            r#"{"name":"file-tool","version":"2.0.0","bin":"./cli.js"}"#,
        )
        .await;

        let resolved = resolve_local_project(tmp.path()).await.unwrap();
        let app = PackageId::root_workspace("app");
        assert_eq!(
            resolved.graph.dependencies[&app]["lib"],
            PackageId::workspace("lib", Path::new("packages/lib"))
        );
        assert_eq!(
            resolved.graph.dependencies[&app]["file-tool"],
            PackageId::file("file-tool", Path::new("tools/file-tool"))
        );
        assert_eq!(
            resolved.graph.dependencies[&app]["left-pad"],
            PackageId::registry("left-pad", "^1.3.0")
        );
        assert_eq!(resolved.graph.resolve_bin("lib").len(), 1);
        assert_eq!(resolved.graph.resolve_bin("file-tool").len(), 1);
        let lock_text = resolved.lockfile.to_toml_string().unwrap();
        assert!(lock_text.contains("[packages.\"app@workspace:.\".dependencies]"));
        assert!(lock_text.contains("left-pad = \"left-pad@npm:^1.3.0\""));
    }

    #[tokio::test]
    async fn registry_metadata_enriches_lockfile_and_populates_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = tmp.path().join("registry");
        let cache = FsRegistryMetadataCache::new(tmp.path().join("cache"));
        write(
            &tmp.path().join("project/package.json"),
            r#"{"name":"app","dependencies":{"left-pad":"^1.0.0"}}"#,
        )
        .await;
        write(
            &fixture.join("left-pad.json"),
            r#"{
              "name": "left-pad",
              "dist-tags": { "latest": "1.3.0" },
              "versions": {
                "1.1.0": {
                  "name": "left-pad",
                  "version": "1.1.0",
                  "dist": {
                    "tarball": "https://registry.npmjs.org/left-pad/-/left-pad-1.1.0.tgz",
                    "integrity": "sha512-old"
                  }
                },
                "1.3.0": {
                  "name": "left-pad",
                  "version": "1.3.0",
                  "dependencies": { "repeat-string": "^1.6.1" },
                  "bin": { "left-pad": "./bin.js" },
                  "scripts": { "postinstall": "node postinstall.js" },
                  "dist": {
                    "tarball": "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz",
                    "integrity": "sha512-new"
                  }
                }
              }
            }"#,
        )
        .await;
        write(
            &fixture.join("repeat-string.json"),
            r#"{
              "name": "repeat-string",
              "dist-tags": { "latest": "1.6.1" },
              "versions": {
                "1.6.1": {
                  "name": "repeat-string",
                  "version": "1.6.1",
                  "dist": {
                    "tarball": "https://registry.npmjs.org/repeat-string/-/repeat-string-1.6.1.tgz",
                    "integrity": "sha512-repeat"
                  }
                }
              }
            }"#,
        )
        .await;

        let client = FileRegistryMetadataClient::new(&fixture);
        let mut resolution = resolve_local_project(tmp.path().join("project"))
            .await
            .unwrap();
        enrich_resolution_with_registry_metadata(&mut resolution, &cache, &client)
            .await
            .unwrap();

        assert!(cache.metadata_path("left-pad").is_file());
        let package = resolution
            .lockfile
            .packages
            .get("left-pad@npm:^1.0.0")
            .unwrap();
        assert_eq!(package.version, "1.3.0");
        assert_eq!(package.integrity.as_deref(), Some("sha512-new"));
        assert_eq!(
            package.resolved.as_ref().unwrap().reference,
            "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz"
        );
        assert_eq!(
            package.dependencies["repeat-string"],
            "repeat-string@npm:^1.6.1"
        );
        assert_eq!(
            package.lifecycle.scripts["postinstall"],
            "node postinstall.js"
        );
        assert_eq!(resolution.graph.resolve_bin("left-pad").len(), 1);
    }

    #[tokio::test]
    async fn async_registry_metadata_cache_reuses_cached_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = tmp.path().join("registry");
        let cache = FsRegistryMetadataCache::new(tmp.path().join("cache"));
        write(
            &fixture.join("left-pad.json"),
            r#"{
              "name": "left-pad",
              "dist-tags": { "latest": "1.0.0" },
              "versions": {
                "1.0.0": {
                  "name": "left-pad",
                  "version": "1.0.0"
                }
              }
            }"#,
        )
        .await;
        let client = FileRegistryMetadataClient::new(&fixture);

        let first = cache.get_or_fetch("left-pad", &client).await.unwrap();
        tokio::fs::remove_file(fixture.join("left-pad.json"))
            .await
            .unwrap();
        let second = cache.get_or_fetch("left-pad", &client).await.unwrap();

        assert_eq!(first, second);
        assert!(cache.metadata_path("left-pad").is_file());
    }

    #[tokio::test]
    async fn tarball_cache_is_integrity_checked_and_reused() {
        let tmp = tempfile::tempdir().unwrap();
        let source_bytes = b"pretend tgz bytes";
        let integrity = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha512::digest(source_bytes))
        );
        let url = "https://registry.npmjs.org/pkg/-/pkg-1.0.0.tgz";
        let fixture = tmp.path().join("tarballs");
        write_bytes(&fixture.join(cache_key(url)), source_bytes).await;
        let cache = FsTarballCache::new(tmp.path().join("cache"));
        let client = FileTarballClient::new(&fixture);
        let source = TarballSource {
            url: url.to_string(),
            integrity: Some(integrity),
        };

        let first = cache.get_or_fetch(&source, &client).await.unwrap();
        tokio::fs::remove_file(fixture.join(cache_key(url)))
            .await
            .unwrap();
        let second = cache.get_or_fetch(&source, &client).await.unwrap();

        assert!(!first.reused);
        assert!(second.reused);
        assert_eq!(first.path, second.path);
    }

    #[tokio::test]
    async fn registry_tarball_cache_uses_enriched_lockfile_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let bytes = b"cached package";
        let integrity = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes))
        );
        let url = "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz";
        let mut lockfile = Lockfile::new();
        lockfile.packages.insert(
            "left-pad@npm:^1.0.0".to_string(),
            LockedPackage {
                name: "left-pad".to_string(),
                version: "1.3.0".to_string(),
                dependencies: BTreeMap::new(),
                integrity: Some(integrity),
                resolved: Some(ResolvedSource {
                    kind: ResolvedSourceKind::Registry,
                    reference: url.to_string(),
                }),
                lifecycle: LifecycleMetadata::default(),
            },
        );
        let fixture = tmp.path().join("tarballs");
        write_bytes(&fixture.join(cache_key(url)), bytes).await;
        let cached = cache_registry_tarballs(
            &lockfile,
            &FsTarballCache::new(tmp.path().join("cache")),
            &FileTarballClient::new(&fixture),
        )
        .await
        .unwrap();

        assert_eq!(cached.len(), 1);
        assert!(!cached[0].reused);
    }

    #[tokio::test]
    async fn package_store_extracts_and_materializes_registry_tarballs() {
        let tmp = tempfile::tempdir().unwrap();
        let tarball = npm_tgz(&[
            (
                "package/package.json",
                r#"{"name":"left-pad","version":"1.0.0","bin":{"left-pad":"./index.js"}}"#,
            ),
            ("package/index.js", "module.exports = 1;\n"),
        ]);
        let integrity = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&tarball))
        );
        let url = "https://registry.npmjs.org/left-pad/-/left-pad-1.0.0.tgz";
        let fixture = tmp.path().join("tarballs");
        write_bytes(&fixture.join(cache_key(url)), &tarball).await;
        let mut lockfile = Lockfile::new();
        lockfile.packages.insert(
            "left-pad@npm:^1.0.0".to_string(),
            LockedPackage {
                name: "left-pad".to_string(),
                version: "1.0.0".to_string(),
                dependencies: BTreeMap::new(),
                integrity: Some(integrity),
                resolved: Some(ResolvedSource {
                    kind: ResolvedSourceKind::Registry,
                    reference: url.to_string(),
                }),
                lifecycle: LifecycleMetadata::default(),
            },
        );
        let store = FsPackageStore::new(tmp.path().join("cache"));
        let first = store
            .materialize_registry_packages(
                tmp.path().join("project"),
                &lockfile,
                &FileTarballClient::new(&fixture),
            )
            .await
            .unwrap();
        let second = store
            .materialize_registry_packages(
                tmp.path().join("project"),
                &lockfile,
                &FileTarballClient::new(&fixture),
            )
            .await
            .unwrap();

        assert_eq!(first.len(), 1);
        assert!(!first[0].reused_install);
        assert_eq!(first[0].linked_bins, 1);
        assert!(second[0].reused_install);
        assert_eq!(second[0].linked_bins, 1);
        assert!(
            tmp.path()
                .join("project/node_modules/left-pad/index.js")
                .is_file()
        );
        assert!(
            tmp.path()
                .join("project/node_modules/.bin/left-pad")
                .is_file()
        );
    }

    #[tokio::test]
    async fn install_local_project_is_lockfile_stable_on_second_run() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let registry = tmp.path().join("registry");
        let tarballs = tmp.path().join("tarballs");
        let tarball = npm_tgz(&[
            (
                "package/package.json",
                r#"{"name":"tool","version":"1.0.0","bin":{"tool":"./bin.js"}}"#,
            ),
            ("package/bin.js", "#!/usr/bin/env otter\nundefined;\n"),
        ]);
        let integrity = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&tarball))
        );
        let url = "https://registry.npmjs.org/tool/-/tool-1.0.0.tgz";
        write(
            &project.join("package.json"),
            r#"{"name":"app","dependencies":{"tool":"^1.0.0"}}"#,
        )
        .await;
        write(
            &registry.join("tool.json"),
            &format!(
                r#"{{
                  "name": "tool",
                  "dist-tags": {{ "latest": "1.0.0" }},
                  "versions": {{
                    "1.0.0": {{
                      "name": "tool",
                      "version": "1.0.0",
                      "bin": {{ "tool": "./bin.js" }},
                      "scripts": {{ "postinstall": "node setup.js" }},
                      "dist": {{
                        "tarball": "{url}",
                        "integrity": "{integrity}"
                      }}
                    }}
                  }}
                }}"#
            ),
        )
        .await;
        write_bytes(&tarballs.join(cache_key(url)), &tarball).await;
        let metadata_cache = FsRegistryMetadataCache::new(tmp.path().join("metadata-cache"));
        let package_store = FsPackageStore::new(tmp.path().join("package-cache"));
        let first = install_local_project(
            &project,
            &metadata_cache,
            &FileRegistryMetadataClient::new(&registry),
            &package_store,
            &FileTarballClient::new(&tarballs),
        )
        .await
        .unwrap();
        let lockfile_first = tokio::fs::read_to_string(project.join("otter-lock"))
            .await
            .unwrap();
        let second = install_local_project(
            &project,
            &metadata_cache,
            &FileRegistryMetadataClient::new(&registry),
            &package_store,
            &FileTarballClient::new(&tarballs),
        )
        .await
        .unwrap();
        let lockfile_second = tokio::fs::read_to_string(project.join("otter-lock"))
            .await
            .unwrap();

        assert!(first.lockfile_changed);
        assert_eq!(first.added_packages, 1);
        assert_eq!(first.linked_bins, 1);
        assert!(!second.lockfile_changed);
        assert_eq!(second.reused_packages, 1);
        assert_eq!(second.linked_bins, 1);
        assert_eq!(lockfile_first, lockfile_second);
        assert!(lockfile_first.contains("postinstall = \"node setup.js\""));
        assert!(project.join("node_modules/tool/bin.js").is_file());
        assert!(project.join("node_modules/.bin/tool").is_file());
        let graph = resolve_installed_project(&project).await.unwrap().graph;
        let bins = graph.resolve_bin("tool");
        assert_eq!(bins.len(), 1);
        assert!(bins[0].path.ends_with("node_modules/.bin/tool"));
    }

    #[tokio::test]
    async fn prune_removed_registry_packages_removes_package_root_and_bins() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        write(
            &project.join("node_modules/tool/package.json"),
            r#"{"name":"tool","version":"1.0.0","bin":{"tool":"./bin.js"}}"#,
        )
        .await;
        write(&project.join("node_modules/tool/bin.js"), "undefined;").await;
        write(&project.join("node_modules/.bin/tool"), "undefined;").await;
        write(
            &project.join("node_modules/.otter-state/tool%40npm%3A%5E1.0.0.source"),
            "source",
        )
        .await;
        let mut previous = Lockfile::new();
        previous.packages.insert(
            "tool@npm:^1.0.0".to_string(),
            LockedPackage {
                name: "tool".to_string(),
                version: "1.0.0".to_string(),
                dependencies: BTreeMap::new(),
                integrity: Some("sha512-test".to_string()),
                resolved: Some(ResolvedSource {
                    kind: ResolvedSourceKind::Registry,
                    reference: "https://registry.npmjs.org/tool/-/tool-1.0.0.tgz".to_string(),
                }),
                lifecycle: LifecycleMetadata::default(),
            },
        );

        let report = prune_removed_registry_packages(&project, &previous, &Lockfile::new())
            .await
            .unwrap();

        assert_eq!(report.removed_packages, 1);
        assert_eq!(report.removed_bins, 1);
        assert!(!project.join("node_modules/tool").exists());
        assert!(!project.join("node_modules/.bin/tool").exists());
    }

    #[tokio::test]
    async fn http_clients_fetch_registry_metadata_and_tarballs() {
        let metadata = r#"{
          "name": "pkg",
          "dist-tags": { "latest": "1.0.0" },
          "versions": {
            "1.0.0": {
              "name": "pkg",
              "version": "1.0.0"
            }
          }
        }"#;
        let metadata_url = serve_http_once(metadata.as_bytes().to_vec()).await;
        let metadata_client = HttpRegistryMetadataClient::with_base_url(metadata_url);
        let fetched = metadata_client.fetch_metadata("pkg").await.unwrap();
        assert_eq!(fetched.name, "pkg");

        let tarball_url = serve_http_once(b"tgz bytes".to_vec()).await;
        let tarball = HttpTarballClient::new()
            .fetch_tarball(&tarball_url)
            .await
            .unwrap();
        assert_eq!(tarball, b"tgz bytes");
    }

    async fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(path, text).await.unwrap();
    }

    async fn write_bytes(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(path, bytes).await.unwrap();
    }

    fn npm_tgz(files: &[(&str, &str)]) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            for (path, text) in files {
                let mut header = tar::Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_size(text.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append(&header, text.as_bytes()).unwrap();
            }
            builder.finish().unwrap();
        }
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &tar_bytes).unwrap();
        encoder.finish().unwrap()
    }

    async fn serve_http_once(body: Vec<u8>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
        });
        format!("http://{addr}")
    }
}
