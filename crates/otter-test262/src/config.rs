//! TOML configuration for the test262 runner

use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::runner::DEFAULT_SKIP_FEATURES;

/// Test262 runner configuration loaded from TOML file
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Test262Config {
    /// Path to the test262 directory
    pub test262_path: Option<PathBuf>,

    /// Git commit SHA to pin test262 to
    pub test262_commit: Option<String>,

    /// Features to skip (replaces DEFAULT_SKIP_FEATURES if non-empty)
    pub skip_features: Vec<String>,

    /// Test path patterns to ignore (glob-style)
    pub ignored_tests: Vec<String>,

    /// Tests known to panic/crash the VM
    pub known_panics: Vec<String>,

    /// Default timeout in seconds per test
    pub timeout_secs: Option<u64>,

    /// Directory for saving results
    pub results_dir: Option<PathBuf>,
}

impl Default for Test262Config {
    fn default() -> Self {
        Self {
            test262_path: None,
            test262_commit: None,
            skip_features: DEFAULT_SKIP_FEATURES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            ignored_tests: Vec::new(),
            known_panics: Vec::new(),
            timeout_secs: None,
            results_dir: None,
        }
    }
}

impl Test262Config {
    /// Load configuration from a TOML file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read config '{}': {}", path.display(), e))?;
        toml::from_str(&content)
            .map_err(|e| format!("Failed to parse config '{}': {}", path.display(), e))
    }

    /// Try to load from the default location, fall back to defaults if not found.
    pub fn load_or_default(path: Option<&Path>) -> Self {
        if let Some(path) = path {
            match Self::load(path) {
                Ok(config) => config,
                Err(e) => {
                    eprintln!("Warning: {}", e);
                    Self::default()
                }
            }
        } else {
            // Try default location
            let default_path = Path::new("test262_config.toml");
            if default_path.exists() {
                match Self::load(default_path) {
                    Ok(config) => config,
                    Err(e) => {
                        eprintln!("Warning: {}", e);
                        Self::default()
                    }
                }
            } else {
                Self::default()
            }
        }
    }

    /// Check if a test path matches any ignored pattern
    pub fn is_ignored(&self, test_path: &str) -> bool {
        self.ignored_tests
            .iter()
            .any(|pattern| test_path.contains(pattern.as_str()))
    }

    /// Check if a test path is a known panic
    pub fn is_known_panic(&self, test_path: &str) -> bool {
        self.known_panics
            .iter()
            .any(|pattern| test_path.contains(pattern.as_str()))
    }
}
