//! `test262_config.toml` loader.
//!
//! The format is the one the project has used since the legacy
//! runner: `timeout_secs`, `max_heap_bytes_per_test`, `skip_features`,
//! `skip_flags`, `ignored_tests`, `known_panics`. The new-engine
//! runner reads exactly this file — no renames, no parallel formats.
//!
//! Resolution rules (CLI flags always win over config defaults):
//! 1. `--config <path>` if supplied;
//! 2. `test262_config.toml` in the current working directory;
//! 3. compiled-in defaults.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Test262 runner configuration loaded from `test262_config.toml`.
///
/// Fields match the format the project has carried since the legacy
/// runner — see [`test262_config.toml`](../../../../test262_config.toml)
/// in the repository root.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct Test262Config {
    /// Path to the test262 directory (informational; the new-engine
    /// runner always uses `vendor/test262`).
    pub test262_path: Option<PathBuf>,

    /// Pinned upstream commit (informational; the actual pin lives
    /// in the `vendor/test262` submodule).
    pub test262_commit: Option<String>,

    /// `features:` tokens whose tests are reported as
    /// `Skipped(<feature>)`.
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

    /// Resolve config from `path` if supplied, else
    /// `test262_config.toml` in the cwd, else defaults. Prints a
    /// warning to stderr on parse failure but never panics.
    #[must_use]
    pub fn load_or_default(path: Option<&Path>) -> Self {
        if let Some(path) = path {
            return match Self::load(path) {
                Ok(cfg) => cfg,
                Err(message) => {
                    eprintln!("warning: {message}");
                    Self::default()
                }
            };
        }
        let default_path = Path::new("test262_config.toml");
        if default_path.exists() {
            return match Self::load(default_path) {
                Ok(cfg) => cfg,
                Err(message) => {
                    eprintln!("warning: {message}");
                    Self::default()
                }
            };
        }
        Self::default()
    }

    /// Substring match a normalised path against `ignored_tests`.
    #[must_use]
    pub fn is_ignored(&self, test_path: &str) -> bool {
        self.ignored_tests
            .iter()
            .any(|pattern| test_path.contains(pattern.as_str()))
    }

    /// Substring match a normalised path against `known_panics`.
    #[must_use]
    pub fn is_known_panic(&self, test_path: &str) -> bool {
        self.known_panics
            .iter()
            .any(|pattern| test_path.contains(pattern.as_str()))
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
    fn parses_legacy_config_shape() {
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
        assert!(cfg.is_ignored("staging/sm/Math/foo.js"));
        assert!(cfg.is_known_panic("RegExp/S15.10.2.8_A3_T15.js"));
    }

    #[test]
    fn defaults_when_missing() {
        let cfg =
            Test262Config::load_or_default(Some(Path::new("/definitely/does/not/exist.toml")));
        assert!(cfg.skip_features.is_empty());
        assert_eq!(cfg.timeout_secs, None);
    }
}
