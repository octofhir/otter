//! Tarball fetch clients and content-addressed cache.
//!
//! This module owns the async tarball byte path: download or fixture read,
//! Subresource Integrity verification, and cache-first persistence. Archive
//! extraction and project materialization live in the install layer.
//!
//! # Contents
//! - [`TarballSource`] describes one registry-selected tarball.
//! - [`TarballFetchClient`] abstracts async byte fetching.
//! - [`FsTarballCache`] stores verified tarballs by deterministic key.
//! - [`FileTarballClient`] and [`HttpTarballClient`] provide fixture-backed and
//!   network-backed byte clients.
//!
//! # Invariants
//! - All APIs are async-only and cache-first.
//! - Cached bytes are verified before reuse whenever integrity is present.
//! - Supported SRI algorithms are `sha512` and `sha256`.
//!
//! # See also
//! - [`crate::FsPackageStore`] for extraction and install materialization.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use base64::Engine;
use sha2::{Digest, Sha256, Sha512};

use crate::{PackageManagerError, cache_key};

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
            if let Some(path) = url.strip_prefix("file://") {
                let path = PathBuf::from(path);
                return tokio::fs::read(&path)
                    .await
                    .map_err(|err| PackageManagerError::Io {
                        path,
                        message: err.to_string(),
                    });
            }
            if !url.starts_with("http://") && !url.starts_with("https://") {
                let path = PathBuf::from(url);
                return tokio::fs::read(&path)
                    .await
                    .map_err(|err| PackageManagerError::Io {
                        path,
                        message: err.to_string(),
                    });
            }
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

pub(crate) fn tarball_cache_key(source: &TarballSource) -> String {
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
