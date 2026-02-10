//! Configuration for node-compat test runner.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Top-level config (from node_compat_config.toml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeCompatConfig {
    /// Default per-test timeout in seconds (default: 10).
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,

    /// Per-module configuration.
    #[serde(default)]
    pub modules: HashMap<String, ModuleConfig>,
}

/// Per-module test selection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModuleConfig {
    /// Glob patterns for test files (e.g. ["test-events-*.js"]).
    #[serde(default)]
    pub patterns: Vec<String>,

    /// Test files to skip (exact basenames).
    #[serde(default)]
    pub skip: Vec<String>,
}

impl ModuleConfig {
    /// Check if a filename matches any of this module's patterns.
    ///
    /// Supports simple globs with a single `*` wildcard:
    /// - `"test-assert-*.js"` matches `"test-assert-ok.js"`
    /// - `"test-assert.js"` matches exactly `"test-assert.js"`
    pub fn matches(&self, filename: &str) -> bool {
        self.patterns
            .iter()
            .any(|pat| simple_glob_match(pat, filename))
    }
}

/// Simple glob match supporting a single `*` wildcard.
fn simple_glob_match(pattern: &str, input: &str) -> bool {
    if let Some(star_pos) = pattern.find('*') {
        let prefix = &pattern[..star_pos];
        let suffix = &pattern[star_pos + 1..];
        input.starts_with(prefix)
            && input.ends_with(suffix)
            && input.len() >= prefix.len() + suffix.len()
    } else {
        pattern == input
    }
}

fn default_timeout() -> u64 {
    10
}

impl NodeCompatConfig {
    /// Load from a TOML file, or return defaults.
    pub fn load_or_default(path: Option<&Path>) -> Self {
        let default_path = Path::new("node_compat_config.toml");
        let config_path = path.unwrap_or(default_path);

        if config_path.exists() {
            match std::fs::read_to_string(config_path) {
                Ok(contents) => match toml::from_str(&contents) {
                    Ok(cfg) => return cfg,
                    Err(e) => {
                        eprintln!("Warning: failed to parse {}: {}", config_path.display(), e);
                    }
                },
                Err(e) => {
                    eprintln!("Warning: failed to read {}: {}", config_path.display(), e);
                }
            }
        }

        Self::default()
    }

    /// Check if a test file should be skipped for a given module.
    pub fn is_skipped(&self, module: &str, test_filename: &str) -> bool {
        self.modules
            .get(module)
            .is_some_and(|m| m.skip.iter().any(|s| s == test_filename))
    }
}

impl Default for NodeCompatConfig {
    fn default() -> Self {
        Self {
            timeout_secs: default_timeout(),
            modules: HashMap::new(),
        }
    }
}
