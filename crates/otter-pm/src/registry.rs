//! npm registry client

use crate::manifest_cache::{CachedManifest, ManifestCache};
use base64::{engine::general_purpose, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// Maximum number of retries for network requests
const MAX_RETRIES: u32 = 3;
/// Base delay for exponential backoff (ms)
const RETRY_BASE_DELAY_MS: u64 = 100;

/// npm registry client (thread-safe, cloneable)
#[derive(Clone)]
pub struct NpmRegistry {
    registry_url: String,
    client: reqwest::Client,
    cache: Arc<RwLock<HashMap<String, PackageMetadata>>>,
    manifest_cache: Arc<ManifestCache>,
}

impl NpmRegistry {
    pub fn new() -> Self {
        Self::with_registry("https://registry.npmjs.org")
    }

    pub fn with_registry(url: &str) -> Self {
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(50) // More connections for parallel downloads
            .pool_idle_timeout(Duration::from_secs(90))
            .http2_adaptive_window(true) // HTTP/2 flow control optimization
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .tcp_nodelay(true) // Disable Nagle's algorithm for lower latency
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            registry_url: url.trim_end_matches('/').to_string(),
            client,
            cache: Arc::new(RwLock::new(HashMap::new())),
            manifest_cache: Arc::new(ManifestCache::new()),
        }
    }

    /// Fetch package metadata from registry (with ETag caching)
    pub async fn get_package(&self, name: &str) -> Result<PackageMetadata, RegistryError> {
        // 1. Check memory cache first
        {
            let cache = self.cache.read().await;
            if let Some(pkg) = cache.get(name) {
                return Ok(pkg.clone());
            }
        }

        // 2. Check disk cache for ETag
        let disk_cached = self.manifest_cache.get(name);
        let (etag, cached_data) = match &disk_cached {
            Some(cached) => (cached.etag.clone(), Some(cached.data.clone())),
            None => (None, None),
        };

        let url = format!("{}/{}", self.registry_url, encode_package_name(name));

        // 3. Send request with conditional ETag header
        let response = self
            .request_with_etag(&url, etag.as_deref())
            .await
            .map_err(|e| RegistryError::Network(e.to_string()))?;

        // 5. Handle 304 Not Modified - use cached data
        if response.status() == 304 {
            if let Some(data) = cached_data {
                // Update memory cache
                let mut cache = self.cache.write().await;
                cache.insert(name.to_string(), data.clone());
                return Ok(data);
            }
            // Shouldn't happen - 304 without cached data
        }

        if response.status() == 404 {
            return Err(RegistryError::NotFound(name.to_string()));
        }

        if !response.status().is_success() {
            return Err(RegistryError::Http(response.status().as_u16()));
        }

        // 6. Extract new ETag from response
        let new_etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let metadata: PackageMetadata = response
            .json()
            .await
            .map_err(|e| RegistryError::Parse(e.to_string()))?;

        // 7. Save to disk cache
        self.manifest_cache
            .set(
                name,
                &CachedManifest {
                    etag: new_etag,
                    last_modified: None,
                    data: metadata.clone(),
                },
            )
            .ok(); // Ignore disk cache errors

        // 8. Update memory cache
        {
            let mut cache = self.cache.write().await;
            cache.insert(name.to_string(), metadata.clone());
        }

        Ok(metadata)
    }

    /// Resolve a package version to a specific version string
    pub async fn resolve_version(
        &self,
        name: &str,
        version_req: &str,
    ) -> Result<String, RegistryError> {
        let metadata = self.get_package(name).await?;

        // Handle dist-tags (latest, next, etc.)
        if let Some(resolved) = metadata.dist_tags.get(version_req) {
            return Ok(resolved.clone());
        }

        // Normalize npm version requirement to semver format
        let normalized = normalize_version_req(version_req);

        // Parse version requirement
        let req = semver::VersionReq::parse(&normalized)
            .map_err(|e| RegistryError::InvalidVersion(e.to_string()))?;

        // Find best matching version
        let mut versions: Vec<semver::Version> = metadata
            .versions
            .keys()
            .filter_map(|v| semver::Version::parse(v).ok())
            .filter(|v| req.matches(v))
            .collect();

        versions.sort();
        versions.reverse();

        versions
            .first()
            .map(|v| v.to_string())
            .ok_or_else(|| RegistryError::NoMatchingVersion {
                name: name.to_string(),
                req: version_req.to_string(),
            })
    }

    /// Download package tarball with retry
    pub async fn download_tarball(
        &self,
        name: &str,
        version: &str,
    ) -> Result<Vec<u8>, RegistryError> {
        let (tarball_url, integrity) = {
            let cache = self.cache.read().await;
            let metadata = cache
                .get(name)
                .ok_or_else(|| RegistryError::NotFound(name.to_string()))?;

            let version_info = metadata
                .versions
                .get(version)
                .ok_or_else(|| RegistryError::NotFound(format!("{}@{}", name, version)))?;

            (
                version_info.dist.tarball.clone(),
                version_info.dist.integrity.clone(),
            )
        };

        let bytes = self.download_with_retry(&tarball_url).await?;

        // Verify integrity if available
        if let Some(integrity) = &integrity {
            verify_integrity(&bytes, integrity)?;
        }

        Ok(bytes)
    }

    /// Download raw bytes with retry
    pub async fn download_with_retry(&self, url: &str) -> Result<Vec<u8>, RegistryError> {
        let mut last_error = None;

        for attempt in 0..MAX_RETRIES {
            match self.client.get(url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    match resp.bytes().await {
                        Ok(bytes) => return Ok(bytes.to_vec()),
                        Err(e) => {
                            last_error = Some(RegistryError::Network(e.to_string()));
                        }
                    }
                }
                Ok(resp) => {
                    last_error = Some(RegistryError::Http(resp.status().as_u16()));
                }
                Err(e) => {
                    last_error = Some(RegistryError::Network(e.to_string()));
                }
            }

            // Exponential backoff
            if attempt < MAX_RETRIES - 1 {
                let delay = Duration::from_millis(RETRY_BASE_DELAY_MS * 2u64.pow(attempt));
                tokio::time::sleep(delay).await;
            }
        }

        Err(last_error.unwrap_or_else(|| RegistryError::Network("Unknown error".to_string())))
    }

    /// Make HTTP request with retry (kept for potential future use)
    #[allow(dead_code)]
    async fn request_with_retry(
        &self,
        url: &str,
        accept: &str,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let mut last_error = None;

        for attempt in 0..MAX_RETRIES {
            match self.client.get(url).header("Accept", accept).send().await {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    last_error = Some(e);
                }
            }

            // Exponential backoff
            if attempt < MAX_RETRIES - 1 {
                let delay = Duration::from_millis(RETRY_BASE_DELAY_MS * 2u64.pow(attempt));
                tokio::time::sleep(delay).await;
            }
        }

        Err(last_error.expect("Should have at least one error"))
    }

    /// Make HTTP request with ETag conditional header and retry
    async fn request_with_etag(
        &self,
        url: &str,
        etag: Option<&str>,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let mut last_error = None;

        for attempt in 0..MAX_RETRIES {
            let mut request = self
                .client
                .get(url)
                .header("Accept", "application/json");

            if let Some(etag) = etag {
                request = request.header("If-None-Match", etag);
            }

            match request.send().await {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    last_error = Some(e);
                }
            }

            // Exponential backoff
            if attempt < MAX_RETRIES - 1 {
                let delay = Duration::from_millis(RETRY_BASE_DELAY_MS * 2u64.pow(attempt));
                tokio::time::sleep(delay).await;
            }
        }

        Err(last_error.expect("Should have at least one error"))
    }

    /// Get cached package metadata (without network request)
    pub async fn get_cached(&self, name: &str) -> Option<PackageMetadata> {
        let cache = self.cache.read().await;
        cache.get(name).cloned()
    }

    /// Clear the package cache
    pub async fn clear_cache(&self) {
        let mut cache = self.cache.write().await;
        cache.clear();
    }

    /// Get version info for a specific package version (from cache)
    pub async fn get_version_info(
        &self,
        name: &str,
        version: &str,
    ) -> Option<VersionInfo> {
        let cache = self.cache.read().await;
        cache
            .get(name)
            .and_then(|m| m.versions.get(version).cloned())
    }
}

impl Default for NpmRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Package metadata from npm registry
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PackageMetadata {
    pub name: String,
    #[serde(rename = "dist-tags", default)]
    pub dist_tags: HashMap<String, String>,
    #[serde(default)]
    pub versions: HashMap<String, VersionInfo>,
}

/// Version-specific package info
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VersionInfo {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub dependencies: Option<HashMap<String, String>>,
    #[serde(rename = "devDependencies", default)]
    pub dev_dependencies: Option<HashMap<String, String>>,
    #[serde(rename = "peerDependencies", default)]
    pub peer_dependencies: Option<HashMap<String, String>>,
    #[serde(rename = "optionalDependencies", default)]
    pub optional_dependencies: Option<HashMap<String, String>>,
    pub dist: DistInfo,
}

/// Distribution info (tarball, integrity)
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DistInfo {
    pub tarball: String,
    #[serde(default)]
    pub shasum: Option<String>,
    #[serde(default)]
    pub integrity: Option<String>,
}

/// Registry errors
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("Package not found: {0}")]
    NotFound(String),

    #[error("Network error: {0}")]
    Network(String),

    #[error("HTTP error: {0}")]
    Http(u16),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Invalid version: {0}")]
    InvalidVersion(String),

    #[error("No matching version for {name}@{req}")]
    NoMatchingVersion { name: String, req: String },

    #[error("Integrity check failed")]
    IntegrityFailed,
}

/// Encode package name for URL (handle scoped packages)
fn encode_package_name(name: &str) -> String {
    if name.starts_with('@') {
        name.replace('/', "%2f")
    } else {
        name.to_string()
    }
}

/// Normalize npm version requirement to semver format
///
/// npm allows various formats that semver crate doesn't understand:
/// - `>= 2.1.2 < 3` (space between comparators) -> `>=2.1.2, <3`
/// - `1.x` -> `1.*`
/// - `*` stays as `*`
fn normalize_version_req(req: &str) -> String {
    let req = req.trim();

    // Handle empty or wildcard
    if req.is_empty() || req == "*" || req == "latest" {
        return "*".to_string();
    }

    // Handle x-ranges: 1.x, 1.2.x -> use wildcards
    let req = req.replace(".x", ".*");

    // Handle space-separated comparators like ">= 2.1.2 < 3"
    // Need to convert to comma-separated for semver crate
    let mut result = String::new();
    let mut chars = req.chars().peekable();
    let mut last_was_version = false;

    while let Some(c) = chars.next() {
        if c == ' ' {
            // Check if next char starts a new comparator
            if let Some(&next) = chars.peek() {
                if next == '<' || next == '>' || next == '=' || next == '^' || next == '~' {
                    // This space separates comparators, use comma
                    if last_was_version {
                        result.push_str(", ");
                        last_was_version = false;
                    }
                    continue;
                }
            }
            // Regular space, skip if after operator
            if !last_was_version {
                continue;
            }
            result.push(c);
        } else {
            result.push(c);
            // Track if we just added a version character (digit or dot)
            last_was_version = c.is_ascii_digit() || c == '.';
        }
    }

    result
}

/// Verify package integrity using SHA-512
fn verify_integrity(data: &[u8], integrity: &str) -> Result<(), RegistryError> {
    // Format: sha512-base64hash
    let parts: Vec<&str> = integrity.splitn(2, '-').collect();
    if parts.len() != 2 || parts[0] != "sha512" {
        return Ok(()); // Unknown format, skip verification
    }

    let expected = general_purpose::STANDARD
        .decode(parts[1])
        .map_err(|_| RegistryError::IntegrityFailed)?;

    let mut hasher = Sha512::new();
    hasher.update(data);
    let actual = hasher.finalize();

    if actual.as_slice() != expected.as_slice() {
        return Err(RegistryError::IntegrityFailed);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_package_name() {
        assert_eq!(encode_package_name("lodash"), "lodash");
        assert_eq!(encode_package_name("@types/node"), "@types%2fnode");
        assert_eq!(encode_package_name("@babel/core"), "@babel%2fcore");
    }

    #[test]
    fn test_normalize_version_req() {
        // Basic versions
        assert_eq!(normalize_version_req("^1.0.0"), "^1.0.0");
        assert_eq!(normalize_version_req("~1.0.0"), "~1.0.0");
        assert_eq!(normalize_version_req("1.0.0"), "1.0.0");

        // Wildcards
        assert_eq!(normalize_version_req("*"), "*");
        assert_eq!(normalize_version_req(""), "*");
        assert_eq!(normalize_version_req("1.x"), "1.*");
        assert_eq!(normalize_version_req("1.2.x"), "1.2.*");

        // Space-separated comparators (npm style)
        assert_eq!(normalize_version_req(">= 2.1.2 < 3"), ">=2.1.2, <3");
        assert_eq!(normalize_version_req(">=1.0.0 <2.0.0"), ">=1.0.0, <2.0.0");
        assert_eq!(normalize_version_req(">= 1.0.0 < 2.0.0"), ">=1.0.0, <2.0.0");

        // Already comma-separated (comma is preserved, spacing may vary)
        let result = normalize_version_req(">=1.0.0, <2.0.0");
        assert!(result.contains(">=1.0.0") && result.contains("<2.0.0"));
    }

    #[test]
    fn test_registry_default() {
        let registry = NpmRegistry::default();
        assert_eq!(registry.registry_url, "https://registry.npmjs.org");
    }

    #[test]
    fn test_registry_custom_url() {
        let registry = NpmRegistry::with_registry("https://npm.pkg.github.com/");
        assert_eq!(registry.registry_url, "https://npm.pkg.github.com");
    }

    #[test]
    fn test_registry_clone() {
        let registry = NpmRegistry::new();
        let cloned = registry.clone();
        assert_eq!(registry.registry_url, cloned.registry_url);
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_get_package() {
        let registry = NpmRegistry::new();
        let result = registry.get_package("lodash").await;
        if let Ok(pkg) = result {
            assert_eq!(pkg.name, "lodash");
            assert!(!pkg.versions.is_empty());
            assert!(pkg.dist_tags.contains_key("latest"));
        }
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_resolve_version() {
        let registry = NpmRegistry::new();
        // First fetch the package
        let _ = registry.get_package("lodash").await;
        // Then resolve a version
        let result = registry.resolve_version("lodash", "^4.0.0").await;
        if let Ok(version) = result {
            assert!(version.starts_with("4."));
        }
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_resolve_latest_tag() {
        let registry = NpmRegistry::new();
        let result = registry.resolve_version("lodash", "latest").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_scoped_package() {
        let registry = NpmRegistry::new();
        let result = registry.get_package("@types/node").await;
        if let Ok(pkg) = result {
            assert_eq!(pkg.name, "@types/node");
        }
    }
}
