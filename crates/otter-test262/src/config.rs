//! `test262_config.toml` loader.
//!
//! # Contents
//! Effective limits and ordered feature, flag, ignored and known-panic policies.
//!
//! # Invariants
//! The first matching configured pattern determines the typed skip reason.
//! The CLI validates explicit config files and freezes policy for all workers.
//!
//! # See also
//! - [`crate::provenance::RunConfig`] records effective semantic policy.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Test262 runner configuration loaded from `test262_config.toml`.
///
/// Effective policy is read from [`test262_config.toml`](../../../../test262_config.toml)
/// in the repository root.
#[derive(Debug, Default, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct Test262Config {
    /// Informational corpus path; execution uses `vendor/test262`.
    pub test262_path: Option<PathBuf>,

    /// Pinned upstream commit (informational; the actual pin lives
    /// in the `vendor/test262` submodule).
    pub test262_commit: Option<String>,

    /// `features:` tokens whose tests report a typed feature skip.
    pub skip_features: Vec<String>,

    /// `flags:` tokens whose tests are reported as skipped. This is
    /// a generic escape hatch for unsupported host/test modes; the
    /// runner itself honors Test262 strictness flags instead of
    /// filtering them.
    pub skip_flags: Vec<String>,

    /// Test-path substrings whose matching tests skip with reason
    /// `"ignored by config"`.
    pub ignored_tests: Vec<String>,

    /// Test-path substrings for tests known to panic the VM. Reported
    /// as `Skipped` with reason `"known panic"` so the runner keeps
    /// moving while the underlying crash is fixed.
    pub known_panics: Vec<String>,

    /// Default per-test timeout in seconds.
    pub timeout_secs: Option<u64>,

    /// Directory for saving results (informational).
    pub results_dir: Option<PathBuf>,

    /// Per-test heap cap (bytes). `0` disables the cap. CLI
    /// `--max-heap-bytes` takes precedence.
    pub max_heap_bytes_per_test: Option<u64>,
}

impl Test262Config {
    /// Load configuration from `path`.
    ///
    /// # Errors
    /// Returns an error string when the file cannot be read or the
    /// TOML cannot be parsed.
    pub fn load(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read config '{}': {}", path.display(), e))?;
        toml::from_str(&content)
            .map_err(|e| format!("failed to parse config '{}': {}", path.display(), e))
    }

    /// Substring match a normalised path against `ignored_tests`.
    #[must_use]
    pub fn first_ignored(&self, test_path: &str) -> Option<&str> {
        self.ignored_tests
            .iter()
            .find(|pattern| test_path.contains(pattern.as_str()))
            .map(String::as_str)
    }

    /// Substring match a normalised path against `known_panics`.
    #[must_use]
    pub fn first_known_panic(&self, test_path: &str) -> Option<&str> {
        self.known_panics
            .iter()
            .find(|pattern| test_path.contains(pattern.as_str()))
            .map(String::as_str)
    }

    /// Return the first configured `flags:` token present in
    /// `test_flags`.
    #[must_use]
    pub fn first_skipped_flag<'a>(&'a self, test_flags: &'a [String]) -> Option<&'a str> {
        self.skip_flags.iter().find_map(|skipped| {
            test_flags
                .iter()
                .any(|flag| flag == skipped)
                .then_some(skipped.as_str())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_current_config_shape() {
        let toml = r#"
timeout_secs = 10
max_heap_bytes_per_test = 536870912
skip_features = ["Atomics", "SharedArrayBuffer"]
skip_flags = ["noStrict"]
ignored_tests = ["staging/sm/Math"]
known_panics = ["S15.10.2.8_A3_T15"]
"#;
        let cfg: Test262Config = toml::from_str(toml).expect("config should parse");
        assert_eq!(cfg.timeout_secs, Some(10));
        assert_eq!(cfg.max_heap_bytes_per_test, Some(536_870_912));
        assert_eq!(cfg.skip_features.len(), 2);
        assert_eq!(
            cfg.first_skipped_flag(&["noStrict".to_string()]),
            Some("noStrict")
        );
        assert_eq!(
            cfg.first_ignored("staging/sm/Math/foo.js"),
            Some("staging/sm/Math")
        );
        assert_eq!(
            cfg.first_known_panic("RegExp/S15.10.2.8_A3_T15.js"),
            Some("S15.10.2.8_A3_T15")
        );
    }

    #[test]
    fn explicit_missing_and_malformed_configs_fail_without_policy_fallback() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Test262Config::load(&dir.path().join("missing.toml")).is_err());
        let path = dir.path().join("broken.toml");
        std::fs::write(&path, "skip_features = [").unwrap();
        assert!(Test262Config::load(&path).is_err());
        assert!(Test262Config::default().skip_features.is_empty());
    }
}
