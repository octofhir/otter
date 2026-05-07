//! Lockfile format (otter.lock)
//!
//! Writes a deterministic, diffable JSON — every collection is a
//! `BTreeMap` so key ordering is stable byte-for-byte across
//! runs. Two invocations of the same resolver against the same
//! registry MUST produce identical lockfiles; this is the base
//! for reproducible installs (T2) and the signed-lockfile work
//! (S3).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// Lockfile structure (otter.lock)
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Lockfile {
    /// Lockfile format version
    pub version: u32,

    /// Locked packages, keyed by package name. `BTreeMap` ensures
    /// deterministic key order in the serialised JSON.
    pub packages: BTreeMap<String, LockfileEntry>,
}

impl Lockfile {
    pub fn new() -> Self {
        Self {
            version: 1,
            packages: BTreeMap::new(),
        }
    }

    /// Load lockfile from path
    pub fn load(path: &Path) -> Result<Self, LockfileError> {
        let content = fs::read_to_string(path).map_err(|e| LockfileError::Io(e.to_string()))?;

        serde_json::from_str(&content).map_err(|e| LockfileError::Parse(e.to_string()))
    }

    /// Save lockfile to path with a trailing newline (POSIX-clean).
    pub fn save(&self, path: &Path) -> Result<(), LockfileError> {
        let mut content = self.serialize_canonical()?;
        content.push('\n');
        fs::write(path, content).map_err(|e| LockfileError::Io(e.to_string()))
    }

    /// Canonical serialization — pretty-printed JSON with stable
    /// key order. Two `Lockfile`s that compare `Eq` produce
    /// byte-identical output. Use this for hashing / diffing /
    /// signing.
    pub fn serialize_canonical(&self) -> Result<String, LockfileError> {
        serde_json::to_string_pretty(self).map_err(|e| LockfileError::Parse(e.to_string()))
    }

    /// SHA-256 checksum of the canonical serialization. Drives
    /// the integrity check in S3 (signed lockfiles) and the
    /// short summary printed by `otterjs install`.
    pub fn checksum(&self) -> Result<String, LockfileError> {
        use sha2::{Digest, Sha256};
        let canonical = self.serialize_canonical()?;
        let digest = Sha256::digest(canonical.as_bytes());
        Ok(format!("{digest:x}"))
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockfileEntry {
    /// Exact version installed
    pub version: String,

    /// Resolved tarball URL
    pub resolved: String,

    /// Integrity hash (SHA-512)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integrity: Option<String>,

    /// Dependencies with version requirements. `BTreeMap` for
    /// deterministic serialisation.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, String>,
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

    fn sample_entry(version: &str) -> LockfileEntry {
        LockfileEntry {
            version: version.to_string(),
            resolved: format!("https://registry.npmjs.org/x/-/x-{version}.tgz"),
            integrity: Some(format!("sha512-stub-{version}")),
            dependencies: BTreeMap::new(),
        }
    }

    #[test]
    fn test_lockfile_new() {
        let lockfile = Lockfile::new();
        assert_eq!(lockfile.version, 1);
        assert!(lockfile.packages.is_empty());
    }

    #[test]
    fn test_lockfile_serialize() {
        let mut lockfile = Lockfile::new();
        lockfile
            .packages
            .insert("lodash".to_string(), sample_entry("4.17.21"));
        let json = lockfile.serialize_canonical().unwrap();
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
        lockfile
            .packages
            .insert("lodash".to_string(), sample_entry("4.17.21"));

        assert!(lockfile.is_locked("lodash", "4.17.21"));
        assert!(!lockfile.is_locked("lodash", "4.17.20"));
        assert!(!lockfile.is_locked("underscore", "1.0.0"));
    }

    /// T2: serialisation MUST be deterministic. Insert the same
    /// entries in different orders and verify byte-identical
    /// output — `BTreeMap` key sort + serde_json's sorted-by-key
    /// guarantee deliver this.
    #[test]
    fn t2_lockfile_serialization_is_deterministic() {
        let mut a = Lockfile::new();
        a.packages
            .insert("z-last".to_string(), sample_entry("1.0.0"));
        a.packages
            .insert("a-first".to_string(), sample_entry("2.0.0"));
        a.packages
            .insert("m-middle".to_string(), sample_entry("3.0.0"));

        // Build a second lockfile with insertion order reversed —
        // a `HashMap` would produce a different byte pattern; the
        // `BTreeMap` we now use guarantees it doesn't.
        let mut b = Lockfile::new();
        b.packages
            .insert("m-middle".to_string(), sample_entry("3.0.0"));
        b.packages
            .insert("a-first".to_string(), sample_entry("2.0.0"));
        b.packages
            .insert("z-last".to_string(), sample_entry("1.0.0"));

        let sa = a.serialize_canonical().unwrap();
        let sb = b.serialize_canonical().unwrap();
        assert_eq!(sa, sb, "canonical lockfile JSON must be byte-identical");
    }

    /// T2: transitive dependencies also need a deterministic
    /// order — `LockfileEntry.dependencies` is also a `BTreeMap`.
    #[test]
    fn t2_lockfile_dependencies_are_deterministic() {
        let mut deps_a: BTreeMap<String, String> = BTreeMap::new();
        deps_a.insert("lodash".to_string(), "^4.0.0".to_string());
        deps_a.insert("axios".to_string(), "^1.0.0".to_string());
        deps_a.insert("chalk".to_string(), "^5.0.0".to_string());
        let mut a = Lockfile::new();
        a.packages.insert(
            "express".to_string(),
            LockfileEntry {
                version: "4.0.0".to_string(),
                resolved: "https://registry.npmjs.org/express/-/express-4.0.0.tgz".to_string(),
                integrity: None,
                dependencies: deps_a,
            },
        );
        let mut deps_b: BTreeMap<String, String> = BTreeMap::new();
        deps_b.insert("chalk".to_string(), "^5.0.0".to_string());
        deps_b.insert("lodash".to_string(), "^4.0.0".to_string());
        deps_b.insert("axios".to_string(), "^1.0.0".to_string());
        let mut b = Lockfile::new();
        b.packages.insert(
            "express".to_string(),
            LockfileEntry {
                version: "4.0.0".to_string(),
                resolved: "https://registry.npmjs.org/express/-/express-4.0.0.tgz".to_string(),
                integrity: None,
                dependencies: deps_b,
            },
        );
        assert_eq!(
            a.serialize_canonical().unwrap(),
            b.serialize_canonical().unwrap(),
        );
    }

    /// T2: checksum is stable for equal lockfiles and differs for
    /// any content change. Drives the signed-lockfile work in S3.
    #[test]
    fn t2_lockfile_checksum_is_stable_and_content_sensitive() {
        let mut a = Lockfile::new();
        a.packages
            .insert("lodash".to_string(), sample_entry("4.17.21"));
        let mut b = Lockfile::new();
        b.packages
            .insert("lodash".to_string(), sample_entry("4.17.21"));
        assert_eq!(a.checksum().unwrap(), b.checksum().unwrap());

        // Different version → different checksum.
        let mut c = Lockfile::new();
        c.packages
            .insert("lodash".to_string(), sample_entry("4.17.22"));
        assert_ne!(a.checksum().unwrap(), c.checksum().unwrap());
    }

    /// T1: save + load round-trips preserve every field. Adding
    /// this as an on-disk test (not an in-memory one) exercises
    /// the trailing-newline + UTF-8 path `save()` writes.
    #[test]
    fn t1_lockfile_save_and_load_round_trip() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("otter.lock");
        let mut a = Lockfile::new();
        a.packages
            .insert("react".to_string(), sample_entry("19.0.0"));
        a.save(&path).expect("save");
        let b = Lockfile::load(&path).expect("load");
        assert_eq!(a, b);
    }
}
