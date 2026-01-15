//! Configuration types for the Otter runtime.
//!
//! This module provides configuration structs for TypeScript transpilation
//! and other runtime settings.

use std::path::{Path, PathBuf};
use swc_ecma_ast::EsVersion;

use crate::error::JscResult;
use crate::tsconfig::{TsConfigJson, find_tsconfig};

/// TypeScript transpilation configuration.
///
/// Controls how TypeScript code is processed before execution.
#[derive(Debug, Clone)]
pub struct TypeScriptConfig {
    /// Enable type checking before execution (requires external type checker).
    /// Default: false (transpile-only mode, like `tsc --noEmit false`).
    pub check: bool,

    /// Path to tsconfig.json for custom compiler options.
    pub tsconfig: Option<PathBuf>,

    /// Enable strict mode type checking.
    /// Default: true
    pub strict: bool,

    /// Skip type checking for library/declaration files.
    /// Default: true
    pub skip_lib_check: bool,

    /// Target ECMAScript version for output.
    /// Default: ES2022
    pub target: EsVersion,

    /// Enable TSX/JSX syntax support.
    /// Default: true
    pub tsx: bool,

    /// Enable decorator support.
    /// Default: true
    pub decorators: bool,

    /// Generate source maps for debugging.
    /// Default: false
    pub source_maps: bool,
}

impl Default for TypeScriptConfig {
    fn default() -> Self {
        Self {
            check: false,
            tsconfig: None,
            strict: true,
            skip_lib_check: true,
            target: EsVersion::Es2022,
            tsx: true,
            decorators: true,
            source_maps: false,
        }
    }
}

impl TypeScriptConfig {
    /// Create a new TypeScript config with defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create config optimized for fast transpilation (no type checking).
    pub fn transpile_only() -> Self {
        Self {
            check: false,
            source_maps: false,
            ..Default::default()
        }
    }

    /// Create config with type checking enabled.
    pub fn with_type_check() -> Self {
        Self {
            check: true,
            ..Default::default()
        }
    }

    /// Create config with source maps enabled.
    pub fn with_source_maps() -> Self {
        Self {
            source_maps: true,
            ..Default::default()
        }
    }

    /// Set the target ECMAScript version.
    pub fn target(mut self, target: EsVersion) -> Self {
        self.target = target;
        self
    }

    /// Set the path to tsconfig.json.
    pub fn tsconfig(mut self, path: impl Into<PathBuf>) -> Self {
        self.tsconfig = Some(path.into());
        self
    }

    /// Enable or disable TSX support.
    pub fn tsx(mut self, enabled: bool) -> Self {
        self.tsx = enabled;
        self
    }

    /// Enable or disable decorator support.
    pub fn decorators(mut self, enabled: bool) -> Self {
        self.decorators = enabled;
        self
    }

    /// Enable or disable source map generation.
    pub fn source_maps(mut self, enabled: bool) -> Self {
        self.source_maps = enabled;
        self
    }

    /// Convert to SWC transpile options.
    pub fn to_transpile_options(
        &self,
        filename: Option<&str>,
    ) -> crate::transpiler::TranspileOptions {
        crate::transpiler::TranspileOptions {
            target: self.target,
            source_map: self.source_maps,
            filename: filename.unwrap_or("script.ts").to_string(),
        }
    }

    /// Load configuration from a tsconfig.json file.
    ///
    /// This parses the tsconfig.json file (including any extended configs)
    /// and returns a TypeScriptConfig with the relevant settings applied.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use otter_runtime::TypeScriptConfig;
    ///
    /// let config = TypeScriptConfig::from_tsconfig("./tsconfig.json").unwrap();
    /// ```
    pub fn from_tsconfig(path: impl AsRef<Path>) -> JscResult<Self> {
        let tsconfig = TsConfigJson::load_with_extends(path.as_ref())?;
        let mut config = tsconfig.to_typescript_config();
        config.tsconfig = Some(path.as_ref().to_path_buf());
        Ok(config)
    }

    /// Load configuration by auto-discovering tsconfig.json.
    ///
    /// Starting from `start_dir`, walks up the directory tree looking for
    /// tsconfig.json. If found, loads and parses it. If not found, returns
    /// the default configuration.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use otter_runtime::TypeScriptConfig;
    ///
    /// // Auto-discover tsconfig.json from current directory
    /// let config = TypeScriptConfig::discover(".").unwrap();
    /// ```
    pub fn discover(start_dir: impl AsRef<Path>) -> JscResult<Self> {
        match find_tsconfig(start_dir) {
            Some(path) => Self::from_tsconfig(&path),
            None => Ok(Self::default()),
        }
    }

    /// Load configuration, preferring explicit path over auto-discovery.
    ///
    /// If `tsconfig_path` is Some, loads from that path. Otherwise,
    /// auto-discovers from `start_dir`. If neither finds a config,
    /// returns defaults.
    pub fn load(
        tsconfig_path: Option<impl AsRef<Path>>,
        start_dir: impl AsRef<Path>,
    ) -> JscResult<Self> {
        if let Some(path) = tsconfig_path {
            Self::from_tsconfig(path)
        } else {
            Self::discover(start_dir)
        }
    }

    /// Merge another config into this one (other takes precedence for set values).
    pub fn merge(mut self, other: Self) -> Self {
        // Only override if the other config has non-default tsconfig path
        if other.tsconfig.is_some() {
            self.tsconfig = other.tsconfig;
        }
        // Override other fields (other takes precedence)
        self.check = other.check;
        self.strict = other.strict;
        self.skip_lib_check = other.skip_lib_check;
        self.target = other.target;
        self.tsx = other.tsx;
        self.decorators = other.decorators;
        self.source_maps = other.source_maps;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = TypeScriptConfig::default();
        assert!(!config.check);
        assert!(config.tsconfig.is_none());
        assert!(config.strict);
        assert!(config.skip_lib_check);
        assert!(config.tsx);
        assert!(config.decorators);
        assert!(!config.source_maps);
    }

    #[test]
    fn test_transpile_only() {
        let config = TypeScriptConfig::transpile_only();
        assert!(!config.check);
        assert!(!config.source_maps);
    }

    #[test]
    fn test_with_type_check() {
        let config = TypeScriptConfig::with_type_check();
        assert!(config.check);
    }

    #[test]
    fn test_builder_pattern() {
        let config = TypeScriptConfig::new()
            .target(EsVersion::Es2020)
            .tsx(false)
            .source_maps(true);

        assert_eq!(config.target, EsVersion::Es2020);
        assert!(!config.tsx);
        assert!(config.source_maps);
    }
}
