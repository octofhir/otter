//! Configuration file parsing for otter.toml.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Main configuration structure.
#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// TypeScript-related settings
    #[serde(default)]
    pub typescript: TypeScriptConfig,

    /// Module resolution settings
    #[serde(default)]
    #[allow(dead_code)]
    pub modules: ModulesConfig,

    /// Default permissions
    #[serde(default)]
    pub permissions: PermissionsConfig,
}

/// TypeScript configuration.
#[derive(Debug, Deserialize)]
pub struct TypeScriptConfig {
    /// Enable type checking before running
    #[serde(default = "default_true")]
    pub check: bool,

    /// Enable strict mode
    #[serde(default = "default_true")]
    pub strict: bool,

    /// Path to tsconfig.json
    pub tsconfig: Option<PathBuf>,
}

impl Default for TypeScriptConfig {
    fn default() -> Self {
        Self {
            check: true,
            strict: true,
            tsconfig: None,
        }
    }
}

/// Module resolution configuration.
#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
pub struct ModulesConfig {
    /// Allowed remote module URLs (glob patterns)
    #[serde(default)]
    pub remote_allowlist: Vec<String>,

    /// Module cache directory
    pub cache_dir: Option<PathBuf>,

    /// Import map aliases
    #[serde(default)]
    pub import_map: HashMap<String, String>,
}

/// Default permission settings.
#[derive(Debug, Default, Deserialize)]
pub struct PermissionsConfig {
    /// Allowed file system read paths
    #[serde(default)]
    pub allow_read: Vec<String>,

    /// Allowed file system write paths
    #[serde(default)]
    pub allow_write: Vec<String>,

    /// Allowed network hosts
    #[serde(default)]
    pub allow_net: Vec<String>,

    /// Allowed environment variables
    #[serde(default)]
    pub allow_env: Vec<String>,
}

fn default_true() -> bool {
    true
}

/// Load configuration from a file or search for default config files.
pub fn load_config(path: Option<&Path>) -> anyhow::Result<Config> {
    let config_path = path.map(PathBuf::from).or_else(find_config_file);

    match config_path {
        Some(path) if path.exists() => {
            let content = std::fs::read_to_string(&path)?;
            let config: Config = toml::from_str(&content)
                .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", path.display(), e))?;
            Ok(config)
        }
        _ => Ok(Config::default()),
    }
}

/// Search for configuration file in the current directory and parent directories.
fn find_config_file() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;

    // Config file names to search for
    const CONFIG_NAMES: &[&str] = &["otter.toml", "otter.config.toml", ".otterrc.toml"];

    // Search in current directory and parents
    let mut dir = Some(cwd.as_path());
    while let Some(current) = dir {
        for name in CONFIG_NAMES {
            let path = current.join(name);
            if path.exists() {
                return Some(path);
            }
        }
        dir = current.parent();
    }

    None
}

/// Find tsconfig.json by walking up from a file's directory.
/// Follows Bun's approach: checks for tsconfig.json first, then jsconfig.json.
pub fn find_tsconfig_for_file(file_path: &Path) -> Option<PathBuf> {
    let start_dir = file_path.parent()?;
    find_tsconfig_in_ancestors(start_dir)
}

/// Search for tsconfig.json/jsconfig.json in ancestor directories.
fn find_tsconfig_in_ancestors(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        // Check tsconfig.json first, then jsconfig.json (like Bun)
        for name in &["tsconfig.json", "jsconfig.json"] {
            let config_path = current.join(name);
            if config_path.exists() {
                return Some(config_path);
            }
        }

        // Move to parent directory
        if !current.pop() {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert!(config.typescript.check);
        assert!(config.typescript.strict);
        assert!(config.permissions.allow_read.is_empty());
    }

    #[test]
    fn test_parse_config() {
        let toml = r#"
[typescript]
check = false
strict = true

[permissions]
allow_read = ["."]
allow_net = ["api.example.com"]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(!config.typescript.check);
        assert!(config.typescript.strict);
        assert_eq!(config.permissions.allow_read, vec!["."]);
        assert_eq!(config.permissions.allow_net, vec!["api.example.com"]);
    }

    #[test]
    fn test_parse_modules_config() {
        let toml = r#"
[modules]
remote_allowlist = ["https://esm.sh/*", "https://cdn.skypack.dev/*"]
cache_dir = ".otter/cache"

[modules.import_map]
"@/utils" = "./src/utils/index.ts"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.modules.remote_allowlist.len(), 2);
        assert_eq!(
            config.modules.cache_dir,
            Some(PathBuf::from(".otter/cache"))
        );
        assert_eq!(
            config.modules.import_map.get("@/utils"),
            Some(&"./src/utils/index.ts".to_string())
        );
    }
}
