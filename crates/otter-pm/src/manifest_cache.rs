//! Disk cache for package manifests with ETag support

use crate::registry::PackageMetadata;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// Cached manifest with HTTP caching headers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedManifest {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub data: PackageMetadata,
}

/// Disk cache for package manifests
#[derive(Clone)]
pub struct ManifestCache {
    cache_dir: PathBuf,
}

impl ManifestCache {
    /// Create a new manifest cache
    pub fn new() -> Self {
        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("otter/manifests");

        Self { cache_dir }
    }

    /// Create manifest cache with custom directory
    pub fn with_dir(cache_dir: PathBuf) -> Self {
        Self { cache_dir }
    }

    /// Get cached manifest if exists
    pub fn get(&self, name: &str) -> Option<CachedManifest> {
        let path = self.cache_path(name);
        let data = std::fs::read(&path).ok()?;
        serde_json::from_slice(&data).ok()
    }

    /// Save manifest to cache
    pub fn set(&self, name: &str, manifest: &CachedManifest) -> std::io::Result<()> {
        let path = self.cache_path(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec(manifest)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&path, json)?;
        Ok(())
    }

    /// Clear the manifest cache
    pub fn clear(&self) -> std::io::Result<()> {
        if self.cache_dir.exists() {
            std::fs::remove_dir_all(&self.cache_dir)?;
        }
        Ok(())
    }

    /// Get cache path for a package
    fn cache_path(&self, name: &str) -> PathBuf {
        // @types/node -> types-node.json
        let safe_name = name.replace('/', "-").replace('@', "");
        self.cache_dir.join(format!("{}.json", safe_name))
    }

    /// Check if cache exists for a package
    pub fn has(&self, name: &str) -> bool {
        self.cache_path(name).exists()
    }
}

impl Default for ManifestCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe manifest cache wrapper
pub type SharedManifestCache = Arc<ManifestCache>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn create_test_metadata() -> PackageMetadata {
        PackageMetadata {
            name: "test-package".to_string(),
            dist_tags: HashMap::new(),
            versions: HashMap::new(),
        }
    }

    #[test]
    fn test_manifest_cache_path() {
        let cache = ManifestCache::with_dir(PathBuf::from("/tmp/test-cache"));
        assert_eq!(
            cache.cache_path("lodash"),
            PathBuf::from("/tmp/test-cache/lodash.json")
        );
        assert_eq!(
            cache.cache_path("@types/node"),
            PathBuf::from("/tmp/test-cache/types-node.json")
        );
    }

    #[test]
    fn test_manifest_cache_roundtrip() {
        let cache = ManifestCache::with_dir(PathBuf::from("/tmp/otter-test-cache"));

        let manifest = CachedManifest {
            etag: Some("\"abc123\"".to_string()),
            last_modified: None,
            data: create_test_metadata(),
        };

        // Save
        cache.set("test-pkg", &manifest).unwrap();

        // Load
        let loaded = cache.get("test-pkg").unwrap();
        assert_eq!(loaded.etag, manifest.etag);
        assert_eq!(loaded.data.name, manifest.data.name);

        // Cleanup
        std::fs::remove_dir_all("/tmp/otter-test-cache").ok();
    }
}
