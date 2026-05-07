//! npm registry metadata clients and cache.
//!
//! This module owns metadata fetch and cache plumbing only. It does not choose
//! package versions, mutate lockfiles, download tarballs, or execute lifecycle
//! scripts.
//!
//! # Contents
//! - [`NpmRegistryMetadata`] and [`NpmPackageVersion`] model the npm metadata
//!   subset needed by Otter installs.
//! - [`RegistryMetadataClient`] abstracts async metadata fetch.
//! - [`FsRegistryMetadataCache`] stores deterministic on-disk metadata JSON.
//! - [`FileRegistryMetadataClient`] and [`HttpRegistryMetadataClient`] provide
//!   fixture-backed and network-backed client implementations.
//!
//! # Invariants
//! - All client/cache APIs are async-only.
//! - Metadata JSON cache filenames use crate-level deterministic escaping.
//! - HTTP clients are transport only; lockfile trust state is decided by the
//!   install resolver.
//!
//! # See also
//! - [`crate::resolve_local_project_with_registry_metadata`] for lockfile
//!   enrichment using this metadata.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use otter_pm_manifest::{DependencySet, PackageBinManifest};
use serde::{Deserialize, Serialize};

use crate::{PackageManagerError, cache_key};

const DEFAULT_NPM_REGISTRY: &str = "https://registry.npmjs.org";

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

fn parse_registry_metadata(
    package: &str,
    text: &str,
) -> Result<NpmRegistryMetadata, PackageManagerError> {
    serde_json::from_str(text).map_err(|err| PackageManagerError::RegistryMetadata {
        package: package.to_string(),
        message: err.to_string(),
    })
}

fn npm_registry_package_path(package: &str) -> String {
    if let Some((scope, name)) = package.split_once('/')
        && scope.starts_with('@')
    {
        return format!("{scope}%2f{name}");
    }
    cache_key(package)
}
