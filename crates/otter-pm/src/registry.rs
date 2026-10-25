//! npm registry metadata clients and cache.
//!
//! This module owns metadata fetch and cache plumbing only. It does not choose
//! package versions, mutate lockfiles, download tarballs, or execute lifecycle
//! scripts.
//!
//! # Contents
//! - [`NpmRegistryMetadata`] and [`NpmPackageVersion`] model the npm metadata
//!   subset needed by Otter installs.
//! - [`RegistryMetadataClient`] abstracts async metadata fetch, conditional
//!   on a cached document's entity tag.
//! - [`FsRegistryMetadataCache`] stores deterministic on-disk metadata JSON
//!   and the entity tag beside it.
//! - [`FileRegistryMetadataClient`] and [`HttpRegistryMetadataClient`] provide
//!   fixture-backed and network-backed client implementations.
//!
//! # Invariants
//! - All client/cache APIs are async-only.
//! - Metadata JSON cache filenames use crate-level deterministic escaping.
//! - A cached document is used as it is for [`METADATA_FRESH_FOR`] after it
//!   was fetched or last confirmed current; after that the registry is asked,
//!   and a `304` renews it. An unreachable registry leaves it in use.
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
    /// Publish timestamps in RFC 3339, keyed by version.
    ///
    /// npm also puts its own `created` / `modified` bookkeeping keys in this
    /// map; they are kept verbatim and ignored by version lookups.
    #[serde(default)]
    pub time: BTreeMap<String, String>,
}

/// One entry of `peerDependenciesMeta`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NpmPeerMeta {
    /// The package works without the peer.
    #[serde(default)]
    pub optional: bool,
}

/// `peerDependenciesMeta` as published: entries that are no object are left
/// out.
fn peer_meta<'de, D>(deserializer: D) -> Result<BTreeMap<String, NpmPeerMeta>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Object(entries) => entries
            .into_iter()
            .filter_map(|(name, meta)| Some((name, serde_json::from_value(meta).ok()?)))
            .collect(),
        _ => BTreeMap::new(),
    })
}

/// An `os` / `cpu` list as published: a single string is a list of one, and
/// anything else names no platform restriction.
fn platform_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::String(entry) => vec![entry],
        serde_json::Value::Array(entries) => entries
            .into_iter()
            .filter_map(|entry| entry.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    })
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
    /// Per-peer settings; a peer marked `optional` is never installed.
    #[serde(
        rename = "peerDependenciesMeta",
        default,
        deserialize_with = "peer_meta"
    )]
    pub peer_dependencies_meta: BTreeMap<String, NpmPeerMeta>,
    /// Operating systems the package runs on, npm's `os` list.
    #[serde(default, deserialize_with = "platform_list")]
    pub os: Vec<String>,
    /// CPU architectures the package runs on, npm's `cpu` list.
    #[serde(default, deserialize_with = "platform_list")]
    pub cpu: Vec<String>,
    /// Package binaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<PackageBinManifest>,
    /// Lifecycle scripts.
    #[serde(default, deserialize_with = "otter_pm_manifest::string_entries")]
    pub scripts: BTreeMap<String, String>,
    /// Distribution metadata.
    #[serde(default)]
    pub dist: NpmDist,
}

impl NpmPackageVersion {
    /// `true` when the registry published build provenance for this version.
    #[must_use]
    pub fn has_provenance(&self) -> bool {
        self.dist.attestations.as_ref().is_some_and(|attestations| {
            attestations.url.is_some()
                || attestations
                    .provenance
                    .as_ref()
                    .is_some_and(|provenance| provenance.predicate_type.is_some())
        })
    }
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
    /// Publish attestations recorded by the registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestations: Option<NpmAttestations>,
}

/// Publish attestations recorded by the registry for one version.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NpmAttestations {
    /// Location the attestation bundle is served from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Build provenance summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<NpmProvenance>,
}

/// Build provenance summary for one version.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NpmProvenance {
    /// Attestation predicate type, for example the SLSA provenance predicate.
    #[serde(
        rename = "predicateType",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub predicate_type: Option<String>,
}

/// What a metadata request found.
#[derive(Debug, Clone, PartialEq)]
pub enum MetadataFetch {
    /// The current document, with the entity tag that names it.
    Fresh {
        /// The package's metadata.
        metadata: NpmRegistryMetadata,
        /// The registry's `ETag` for this document.
        etag: Option<String>,
    },
    /// The document the request's entity tag names is still current.
    NotModified,
}

/// Source of npm registry metadata.
pub trait RegistryMetadataClient {
    /// Fetch metadata for `package` without blocking the async runtime.
    /// With `etag`, the registry may answer that the document it names is
    /// still current instead of sending it again.
    fn fetch_metadata<'a>(
        &'a self,
        package: &'a str,
        etag: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<MetadataFetch, PackageManagerError>> + Send + 'a>>;
}

/// How long a cached document is used without asking the registry: the
/// public registry's own `max-age`.
pub const METADATA_FRESH_FOR: std::time::Duration = std::time::Duration::from_secs(300);

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

    fn etag_path(&self, package: &str) -> PathBuf {
        self.root.join(format!("{}.etag", cache_key(package)))
    }

    /// The cached document while it is younger than [`METADATA_FRESH_FOR`];
    /// otherwise the registry's answer to a request conditional on the cached
    /// document's entity tag. A registry that cannot be reached leaves the
    /// cached document in use.
    pub async fn get_or_fetch(
        &self,
        package: &str,
        client: &impl RegistryMetadataClient,
    ) -> Result<NpmRegistryMetadata, PackageManagerError> {
        let path = self.metadata_path(package);
        let age = tokio::fs::metadata(&path)
            .await
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .map(|modified| modified.elapsed().unwrap_or_default());
        if age.is_some_and(|age| age < METADATA_FRESH_FOR)
            && let Some(metadata) = self.read(package).await?
        {
            return Ok(metadata);
        }
        let etag = match age {
            Some(_) => tokio::fs::read_to_string(self.etag_path(package))
                .await
                .ok(),
            None => None,
        };
        match client.fetch_metadata(package, etag.as_deref()).await {
            Ok(MetadataFetch::NotModified) => {
                if let Some(metadata) = self.read(package).await? {
                    touch(&path).await;
                    return Ok(metadata);
                }
                // The cached document went missing in between: ask again.
                match client.fetch_metadata(package, None).await? {
                    MetadataFetch::Fresh { metadata, etag } => {
                        self.store(&metadata, etag.as_deref()).await?;
                        Ok(metadata)
                    }
                    MetadataFetch::NotModified => Err(PackageManagerError::RegistryMetadata {
                        package: package.to_string(),
                        message: "registry reported an unrequested document as current".to_string(),
                    }),
                }
            }
            Ok(MetadataFetch::Fresh { metadata, etag }) => {
                self.store(&metadata, etag.as_deref()).await?;
                Ok(metadata)
            }
            Err(error) => match self.read(package).await? {
                Some(metadata) => Ok(metadata),
                None => Err(error),
            },
        }
    }

    async fn store(
        &self,
        metadata: &NpmRegistryMetadata,
        etag: Option<&str>,
    ) -> Result<(), PackageManagerError> {
        self.write(metadata).await?;
        let path = self.etag_path(&metadata.name);
        let written = match etag {
            Some(etag) => tokio::fs::write(&path, etag).await,
            None => match tokio::fs::remove_file(&path).await {
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            },
        };
        written.map_err(|err| PackageManagerError::Io {
            path,
            message: err.to_string(),
        })
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
        _etag: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<MetadataFetch, PackageManagerError>> + Send + 'a>> {
        Box::pin(async move {
            let path = self.path(package);
            let text =
                tokio::fs::read_to_string(&path)
                    .await
                    .map_err(|err| PackageManagerError::Io {
                        path,
                        message: err.to_string(),
                    })?;
            Ok(MetadataFetch::Fresh {
                metadata: parse_registry_metadata(package, &text)?,
                etag: None,
            })
        })
    }
}

/// Async HTTP-backed npm registry metadata client.
#[derive(Debug, Clone)]
pub struct HttpRegistryMetadataClient {
    client: otter_http_client::Client,
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
            client: otter_http_client::Client::new(),
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
        etag: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<MetadataFetch, PackageManagerError>> + Send + 'a>> {
        Box::pin(async move {
            let url = self.metadata_url(package);
            let mut request = self
                .client
                .get(&url)
                .header(otter_http_client::header::ACCEPT, "application/json");
            if let Some(etag) = etag {
                request = request.header(otter_http_client::header::IF_NONE_MATCH, etag);
            }
            let response = request
                .send()
                .await
                .map_err(|err| PackageManagerError::Http {
                    url: url.clone(),
                    message: err.to_string(),
                })?;
            let status = response.status();
            if status == otter_http_client::StatusCode::NOT_MODIFIED {
                return Ok(MetadataFetch::NotModified);
            }
            let etag = response
                .headers()
                .get(otter_http_client::header::ETAG)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
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
            Ok(MetadataFetch::Fresh {
                metadata: parse_registry_metadata(package, &text)?,
                etag,
            })
        })
    }
}

/// Mark a cached document as just confirmed current.
async fn touch(path: &std::path::Path) {
    let path = path.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || {
        std::fs::File::options()
            .append(true)
            .open(&path)
            .and_then(|file| file.set_modified(std::time::SystemTime::now()))
    })
    .await;
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
