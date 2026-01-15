//! npm registry client

use base64::{Engine as _, engine::general_purpose};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use std::collections::HashMap;

/// npm registry client
pub struct NpmRegistry {
    registry_url: String,
    client: reqwest::Client,
    cache: HashMap<String, PackageMetadata>,
}

impl NpmRegistry {
    pub fn new() -> Self {
        Self::with_registry("https://registry.npmjs.org")
    }

    pub fn with_registry(url: &str) -> Self {
        Self {
            registry_url: url.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
            cache: HashMap::new(),
        }
    }

    /// Fetch package metadata from registry
    pub async fn get_package(&mut self, name: &str) -> Result<PackageMetadata, RegistryError> {
        // Check cache first
        if let Some(pkg) = self.cache.get(name) {
            return Ok(pkg.clone());
        }

        let url = format!("{}/{}", self.registry_url, encode_package_name(name));

        let response = self
            .client
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| RegistryError::Network(e.to_string()))?;

        if response.status() == 404 {
            return Err(RegistryError::NotFound(name.to_string()));
        }

        if !response.status().is_success() {
            return Err(RegistryError::Http(response.status().as_u16()));
        }

        let metadata: PackageMetadata = response
            .json()
            .await
            .map_err(|e| RegistryError::Parse(e.to_string()))?;

        self.cache.insert(name.to_string(), metadata.clone());
        Ok(metadata)
    }

    /// Resolve a package version to a specific version string
    pub async fn resolve_version(
        &mut self,
        name: &str,
        version_req: &str,
    ) -> Result<String, RegistryError> {
        let metadata = self.get_package(name).await?;

        // Handle dist-tags (latest, next, etc.)
        if let Some(resolved) = metadata.dist_tags.get(version_req) {
            return Ok(resolved.clone());
        }

        // Parse version requirement
        let req = semver::VersionReq::parse(version_req)
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

    /// Download package tarball
    pub async fn download_tarball(
        &self,
        name: &str,
        version: &str,
    ) -> Result<Vec<u8>, RegistryError> {
        let metadata = self
            .cache
            .get(name)
            .ok_or_else(|| RegistryError::NotFound(name.to_string()))?;

        let version_info = metadata
            .versions
            .get(version)
            .ok_or_else(|| RegistryError::NotFound(format!("{}@{}", name, version)))?;

        let tarball_url = &version_info.dist.tarball;

        let response = self
            .client
            .get(tarball_url)
            .send()
            .await
            .map_err(|e| RegistryError::Network(e.to_string()))?;

        if !response.status().is_success() {
            return Err(RegistryError::Http(response.status().as_u16()));
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| RegistryError::Network(e.to_string()))?;

        // Verify integrity if available
        if let Some(integrity) = &version_info.dist.integrity {
            verify_integrity(&bytes, integrity)?;
        }

        Ok(bytes.to_vec())
    }

    /// Get cached package metadata (without network request)
    pub fn get_cached(&self, name: &str) -> Option<&PackageMetadata> {
        self.cache.get(name)
    }

    /// Clear the package cache
    pub fn clear_cache(&mut self) {
        self.cache.clear();
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
    fn test_registry_default() {
        let registry = NpmRegistry::default();
        assert_eq!(registry.registry_url, "https://registry.npmjs.org");
    }

    #[test]
    fn test_registry_custom_url() {
        let registry = NpmRegistry::with_registry("https://npm.pkg.github.com/");
        assert_eq!(registry.registry_url, "https://npm.pkg.github.com");
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_get_package() {
        let mut registry = NpmRegistry::new();
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
        let mut registry = NpmRegistry::new();
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
        let mut registry = NpmRegistry::new();
        let result = registry.resolve_version("lodash", "latest").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_scoped_package() {
        let mut registry = NpmRegistry::new();
        let result = registry.get_package("@types/node").await;
        if let Ok(pkg) = result {
            assert_eq!(pkg.name, "@types/node");
        }
    }
}
