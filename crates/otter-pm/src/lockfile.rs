//! Lockfile format (otter.lock)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Lockfile structure (otter.lock)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Lockfile {
    /// Lockfile format version
    pub version: u32,

    /// Locked packages
    pub packages: HashMap<String, LockfileEntry>,
}

impl Lockfile {
    pub fn new() -> Self {
        Self {
            version: 1,
            packages: HashMap::new(),
        }
    }

    /// Load lockfile from path
    pub fn load(path: &Path) -> Result<Self, LockfileError> {
        let content = fs::read_to_string(path).map_err(|e| LockfileError::Io(e.to_string()))?;

        serde_json::from_str(&content).map_err(|e| LockfileError::Parse(e.to_string()))
    }

    /// Save lockfile to path
    pub fn save(&self, path: &Path) -> Result<(), LockfileError> {
        let content =
            serde_json::to_string_pretty(self).map_err(|e| LockfileError::Parse(e.to_string()))?;

        fs::write(path, content).map_err(|e| LockfileError::Io(e.to_string()))
    }

    /// Check if a package is locked at a specific version
    pub fn is_locked(&self, name: &str, version: &str) -> bool {
        self.packages
            .get(name)
            .is_some_and(|entry| entry.version == version)
    }

    /// Get locked version for a package
    pub fn get_version(&self, name: &str) -> Option<&str> {
        self.packages.get(name).map(|e| e.version.as_str())
    }
}

/// Single package entry in lockfile
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockfileEntry {
    /// Exact version installed
    pub version: String,

    /// Resolved tarball URL
    pub resolved: String,

    /// Integrity hash (SHA-512)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integrity: Option<String>,

    /// Dependencies with version requirements
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub dependencies: HashMap<String, String>,
}

#[derive(Debug, thiserror::Error)]
pub enum LockfileError {
    #[error("IO error: {0}")]
    Io(String),

    #[error("Parse error: {0}")]
    Parse(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lockfile_new() {
        let lockfile = Lockfile::new();
        assert_eq!(lockfile.version, 1);
        assert!(lockfile.packages.is_empty());
    }

    #[test]
    fn test_lockfile_serialize() {
        let mut lockfile = Lockfile::new();
        lockfile.packages.insert(
            "lodash".to_string(),
            LockfileEntry {
                version: "4.17.21".to_string(),
                resolved: "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz".to_string(),
                integrity: Some("sha512-v2kDEe57lecTulaDIuNTPy3Ry4gLGJ6Z1O3vE1krgXZNrsQ+LFTGHVxVjcXPs17LhbZVGedAJv8XZ1tvj5FvSg==".to_string()),
                dependencies: HashMap::new(),
            },
        );

        let json = serde_json::to_string_pretty(&lockfile).unwrap();
        assert!(json.contains("lodash"));
        assert!(json.contains("4.17.21"));
    }

    #[test]
    fn test_lockfile_deserialize() {
        let json = r#"{
            "version": 1,
            "packages": {
                "lodash": {
                    "version": "4.17.21",
                    "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz"
                }
            }
        }"#;

        let lockfile: Lockfile = serde_json::from_str(json).unwrap();
        assert_eq!(lockfile.version, 1);
        assert!(lockfile.packages.contains_key("lodash"));
        assert_eq!(lockfile.packages["lodash"].version, "4.17.21");
    }

    #[test]
    fn test_is_locked() {
        let mut lockfile = Lockfile::new();
        lockfile.packages.insert(
            "lodash".to_string(),
            LockfileEntry {
                version: "4.17.21".to_string(),
                resolved: "https://example.com".to_string(),
                integrity: None,
                dependencies: HashMap::new(),
            },
        );

        assert!(lockfile.is_locked("lodash", "4.17.21"));
        assert!(!lockfile.is_locked("lodash", "4.17.20"));
        assert!(!lockfile.is_locked("underscore", "1.0.0"));
    }
}
