//! Type checker using tsgo RPC.
//!
//! This module provides the high-level API for type checking TypeScript code
//! using tsgo.

use super::binary::find_tsgo;
use super::diagnostics::Diagnostic;
use super::rpc::TsgoChannel;
use crate::error::JscResult;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Type checking configuration.
#[derive(Debug, Clone)]
pub struct TypeCheckConfig {
    /// Enable type checking (default: true).
    /// When false, all check functions return empty diagnostics.
    pub enabled: bool,

    /// Path to tsconfig.json for project configuration.
    pub tsconfig: Option<PathBuf>,

    /// Enable strict mode type checking.
    /// Default: true
    pub strict: bool,

    /// Skip type checking of declaration files (.d.ts).
    /// Default: true
    pub skip_lib_check: bool,

    /// Target ECMAScript version (e.g., "ES2022").
    pub target: Option<String>,

    /// Module system (e.g., "NodeNext", "ESNext").
    pub module: Option<String>,

    /// Library files to include (e.g., ["ES2020", "DOM"]).
    /// Default: ["ES2020"]
    pub lib: Vec<String>,
}

impl Default for TypeCheckConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tsconfig: None,
            strict: true,
            skip_lib_check: true,
            target: None,
            module: None,
            lib: vec!["ES2020".to_string()],
        }
    }
}

impl TypeCheckConfig {
    /// Create a new config with defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create config with type checking disabled.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Default::default()
        }
    }

    /// Set the tsconfig.json path.
    pub fn with_tsconfig(mut self, path: impl Into<PathBuf>) -> Self {
        self.tsconfig = Some(path.into());
        self
    }

    /// Set strict mode.
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Set skip lib check.
    pub fn with_skip_lib_check(mut self, skip: bool) -> Self {
        self.skip_lib_check = skip;
        self
    }

    /// Set target ECMAScript version.
    pub fn with_target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    /// Set module system.
    pub fn with_module(mut self, module: impl Into<String>) -> Self {
        self.module = Some(module.into());
        self
    }

    /// Set library files to include.
    pub fn with_lib(mut self, lib: Vec<String>) -> Self {
        self.lib = lib;
        self
    }

    /// Convert to compiler options JSON.
    fn to_compiler_options(&self) -> Value {
        let mut opts = json!({
            "strict": self.strict,
            "skipLibCheck": self.skip_lib_check,
            "noEmit": true,
        });

        if let Some(ref target) = self.target {
            opts["target"] = json!(target);
        }

        if let Some(ref module) = self.module {
            opts["module"] = json!(module);
        }

        if !self.lib.is_empty() {
            opts["lib"] = json!(self.lib);
        }

        opts
    }
}

/// Type checker using tsgo via RPC.
///
/// Manages a tsgo subprocess and provides methods for type checking
/// TypeScript projects and files.
///
/// # Example
///
/// ```no_run
/// use otter_runtime::tsgo::{TypeChecker, TypeCheckConfig};
/// use std::path::Path;
///
/// async fn check() {
///     let mut checker = TypeChecker::new().await.unwrap();
///     let config = TypeCheckConfig::default();
///
///     let diagnostics = checker.check_project(
///         Path::new("./tsconfig.json"),
///         &config,
///     ).unwrap();
///
///     for diag in &diagnostics {
///         eprintln!("{}", diag);
///     }
///
///     checker.shutdown().unwrap();
/// }
/// ```
pub struct TypeChecker {
    channel: TsgoChannel,
    configured: bool,
    /// Current project ID from loadProject
    project_id: Option<String>,
    /// Temp directory for synthetic configs
    temp_dir: Option<tempfile::TempDir>,
}

impl TypeChecker {
    /// Create a new type checker.
    ///
    /// This will find the tsgo binary (downloading if necessary) and
    /// start it in API mode.
    ///
    /// # Errors
    ///
    /// Returns error if tsgo cannot be found or started.
    pub async fn new() -> JscResult<Self> {
        let tsgo_path = find_tsgo().await?;
        let channel = TsgoChannel::new(&tsgo_path)?;

        tracing::debug!("TypeChecker started with tsgo at {:?}", tsgo_path);

        Ok(Self {
            channel,
            configured: false,
            project_id: None,
            temp_dir: None,
        })
    }

    /// Create a type checker with a specific tsgo binary path.
    pub fn with_binary(tsgo_path: &Path) -> JscResult<Self> {
        let channel = TsgoChannel::new(tsgo_path)?;

        Ok(Self {
            channel,
            configured: false,
            project_id: None,
            temp_dir: None,
        })
    }

    /// Configure the type checker with compiler options.
    ///
    /// This should be called before checking files.
    pub fn configure(&mut self, config: &TypeCheckConfig) -> JscResult<()> {
        tracing::debug!("Configuring tsgo with options: {:?}", config);

        // Send configure request with supported callbacks
        // tsgo expects certain callbacks to be registered for file system operations
        let _: Value = self.channel.request(
            "configure",
            json!({
                "callbacks": [
                    "readFile",
                    "getPackageJsonScopeIfApplicable",
                    "getPackageScopeForPath",
                    "resolveModuleName",
                    "resolveTypeReferenceDirective",
                    "getImpliedNodeFormatForFile",
                    "isNodeSourceFile"
                ],
                "logFile": "",
                "forkContextInfo": {
                    // Web APIs that exist in both @types/node and Otter runtime
                    // These are ignored when they conflict with Otter's native implementations
                    "typesNodeIgnorableNames": [
                        "AbortController",
                        "AbortSignal",
                        "Blob",
                        "BroadcastChannel",
                        "ByteLengthQueuingStrategy",
                        "CloseEvent",
                        "CompressionStream",
                        "CountQueuingStrategy",
                        "Crypto",
                        "CryptoKey",
                        "CustomEvent",
                        "DecompressionStream",
                        "DOMException",
                        "Event",
                        "EventSource",
                        "EventTarget",
                        "fetch",
                        "File",
                        "FormData",
                        "Headers",
                        "MessageChannel",
                        "MessageEvent",
                        "MessagePort",
                        "Navigator",
                        "navigator",
                        "Performance",
                        "PerformanceEntry",
                        "PerformanceMark",
                        "PerformanceMeasure",
                        "ProgressEvent",
                        "ReadableByteStreamController",
                        "ReadableStream",
                        "ReadableStreamBYOBReader",
                        "ReadableStreamBYOBRequest",
                        "ReadableStreamDefaultController",
                        "ReadableStreamDefaultReader",
                        "Request",
                        "Response",
                        "SubtleCrypto",
                        "TextDecoder",
                        "TextDecoderStream",
                        "TextEncoder",
                        "TextEncoderStream",
                        "TransformStream",
                        "TransformStreamDefaultController",
                        "URL",
                        "URLSearchParams",
                        "WebSocket",
                        "WritableStream",
                        "WritableStreamDefaultController",
                        "WritableStreamDefaultWriter"
                    ],
                    // Node.js-only globals that don't exist in Otter
                    "nodeOnlyGlobalNames": [
                        "__dirname",
                        "__filename",
                        "Buffer",
                        "clearImmediate",
                        "global",
                        "require",
                        "setImmediate"
                    ]
                }
            }),
        )?;

        self.configured = true;
        Ok(())
    }

    /// Load a TypeScript project from a tsconfig.json file.
    ///
    /// This parses the tsconfig and prepares for type checking.
    /// Returns the project ID.
    pub fn load_project(&mut self, tsconfig: &Path) -> JscResult<String> {
        let tsconfig_path = tsconfig
            .canonicalize()
            .unwrap_or_else(|_| tsconfig.to_path_buf());

        // Set lib search root to tsconfig directory for finding TypeScript lib files
        if let Some(tsconfig_dir) = tsconfig_path.parent() {
            self.channel.set_lib_search_root(tsconfig_dir.to_path_buf());
        }

        tracing::debug!("Loading project from {:?}", tsconfig_path);

        let response: ProjectResponse = self.channel.request(
            "loadProject",
            json!({
                "configFileName": tsconfig_path.to_string_lossy()
            }),
        )?;

        tracing::debug!("Project loaded with ID: {}", response.id);
        self.project_id = Some(response.id.clone());
        Ok(response.id)
    }

    /// Get diagnostics for the loaded project.
    ///
    /// Must call `load_project` first.
    pub fn get_diagnostics(&mut self) -> JscResult<Vec<Diagnostic>> {
        let project_id = self.project_id.as_ref().ok_or_else(|| {
            crate::error::JscError::internal(
                "No project loaded. Call load_project first.".to_string(),
            )
        })?;

        // tsgo returns diagnostics as a plain array, not wrapped in an object
        let diagnostics: Vec<Diagnostic> = self.channel.request(
            "getDiagnostics",
            json!({
                "project": project_id,
                "fileNames": Vec::<String>::new()
            }),
        )?;
        Ok(diagnostics)
    }

    /// Check a project and return all diagnostics.
    ///
    /// This is a convenience method that:
    /// 1. Configures the checker
    /// 2. Loads the project
    /// 3. Returns all diagnostics
    ///
    /// # Arguments
    ///
    /// * `tsconfig` - Path to tsconfig.json
    /// * `config` - Type check configuration
    pub fn check_project(
        &mut self,
        tsconfig: &Path,
        config: &TypeCheckConfig,
    ) -> JscResult<Vec<Diagnostic>> {
        if !config.enabled {
            return Ok(Vec::new());
        }

        if !self.configured {
            self.configure(config)?;
        }

        self.load_project(tsconfig)?;
        self.get_diagnostics()
    }

    /// Check specific files and return diagnostics.
    ///
    /// # Arguments
    ///
    /// * `files` - Files to check
    /// * `config` - Type check configuration
    pub fn check_files(
        &mut self,
        files: &[PathBuf],
        config: &TypeCheckConfig,
    ) -> JscResult<Vec<Diagnostic>> {
        if !config.enabled || files.is_empty() {
            return Ok(Vec::new());
        }

        if !self.configured {
            self.configure(config)?;
        }

        // Convert paths to absolute
        let file_paths: Vec<String> = files
            .iter()
            .map(|p| {
                p.canonicalize()
                    .unwrap_or_else(|_| p.to_path_buf())
                    .to_string_lossy()
                    .to_string()
            })
            .collect();

        tracing::debug!("Checking files: {:?}", file_paths);

        // Create a synthetic tsconfig with the files
        // tsgo expects to load a project via loadProject
        let synthetic_config = json!({
            "compilerOptions": config.to_compiler_options(),
            "files": file_paths
        });

        // Create a temp directory and write the config
        use crate::error::JscError;
        let temp_dir = tempfile::tempdir()
            .map_err(|e| JscError::internal(format!("Failed to create temp directory: {}", e)))?;
        let config_path = temp_dir.path().join("tsconfig.json");
        std::fs::write(&config_path, synthetic_config.to_string())
            .map_err(|e| JscError::internal(format!("Failed to write tsconfig: {}", e)))?;

        tracing::debug!("Created synthetic tsconfig at {:?}", config_path);

        // Keep temp dir alive
        self.temp_dir = Some(temp_dir);

        // Load the project and get diagnostics
        self.load_project(&config_path)?;
        self.get_diagnostics()
    }

    /// Check if the tsgo process is still running.
    pub fn is_running(&mut self) -> bool {
        self.channel.is_running()
    }

    /// Shutdown the type checker and release resources.
    pub fn shutdown(self) -> JscResult<()> {
        tracing::debug!("Shutting down TypeChecker");
        self.channel.shutdown()
    }
}

/// Response from loadProject RPC call.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct ProjectResponse {
    id: String,
    #[serde(default)]
    config_file_name: String,
    #[serde(default)]
    root_files: Vec<String>,
}

/// Response from getDiagnostics RPC call.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct DiagnosticsResponse {
    #[serde(default)]
    diagnostics: Vec<Diagnostic>,
}

// ============================================================================
// Convenience functions
// ============================================================================

/// Check types for a set of files.
///
/// This is a convenience function that creates a TypeChecker, checks the files,
/// and shuts down.
///
/// # Example
///
/// ```no_run
/// use otter_runtime::tsgo::{check_types, TypeCheckConfig};
/// use std::path::PathBuf;
///
/// async fn check() {
///     let files = vec![PathBuf::from("src/main.ts")];
///     let config = TypeCheckConfig::default();
///
///     let diagnostics = check_types(&files, &config).await.unwrap();
///
///     for diag in diagnostics {
///         eprintln!("{}", diag);
///     }
/// }
/// ```
pub async fn check_types(
    files: &[PathBuf],
    config: &TypeCheckConfig,
) -> JscResult<Vec<Diagnostic>> {
    if !config.enabled || files.is_empty() {
        return Ok(Vec::new());
    }

    let mut checker = TypeChecker::new().await?;

    // If a tsconfig is specified, use project-based checking
    let diagnostics = if let Some(ref tsconfig) = config.tsconfig {
        checker.check_project(tsconfig, config)?
    } else {
        checker.check_files(files, config)?
    };

    checker.shutdown()?;
    Ok(diagnostics)
}

/// Check a single file for type errors.
///
/// # Example
///
/// ```no_run
/// use otter_runtime::tsgo::{check_file, TypeCheckConfig};
/// use std::path::Path;
///
/// async fn check() {
///     let config = TypeCheckConfig::default();
///     let diagnostics = check_file(Path::new("src/main.ts"), &config).await.unwrap();
/// }
/// ```
pub async fn check_file(file: &Path, config: &TypeCheckConfig) -> JscResult<Vec<Diagnostic>> {
    check_types(&[file.to_path_buf()], config).await
}

/// Check a project by tsconfig.json path.
///
/// # Example
///
/// ```no_run
/// use otter_runtime::tsgo::{check_project, TypeCheckConfig};
/// use std::path::Path;
///
/// async fn check() {
///     let config = TypeCheckConfig::default();
///     let diagnostics = check_project(Path::new("./tsconfig.json"), &config).await.unwrap();
/// }
/// ```
pub async fn check_project(
    tsconfig: &Path,
    config: &TypeCheckConfig,
) -> JscResult<Vec<Diagnostic>> {
    if !config.enabled {
        return Ok(Vec::new());
    }

    let mut checker = TypeChecker::new().await?;
    let diagnostics = checker.check_project(tsconfig, config)?;
    checker.shutdown()?;
    Ok(diagnostics)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = TypeCheckConfig::default();
        assert!(config.enabled);
        assert!(config.strict);
        assert!(config.skip_lib_check);
        assert!(config.tsconfig.is_none());
    }

    #[test]
    fn test_config_disabled() {
        let config = TypeCheckConfig::disabled();
        assert!(!config.enabled);
    }

    #[test]
    fn test_config_builder() {
        let config = TypeCheckConfig::new()
            .with_tsconfig("./tsconfig.json")
            .with_strict(false)
            .with_target("ES2020")
            .with_module("NodeNext");

        assert!(config.tsconfig.is_some());
        assert!(!config.strict);
        assert_eq!(config.target, Some("ES2020".to_string()));
        assert_eq!(config.module, Some("NodeNext".to_string()));
    }

    #[test]
    fn test_config_to_compiler_options() {
        let config = TypeCheckConfig::new()
            .with_strict(true)
            .with_skip_lib_check(true)
            .with_target("ES2022");

        let opts = config.to_compiler_options();

        assert_eq!(opts["strict"], true);
        assert_eq!(opts["skipLibCheck"], true);
        assert_eq!(opts["noEmit"], true);
        assert_eq!(opts["target"], "ES2022");
    }

    // Integration tests that require tsgo would go here
    // They should be marked #[ignore] for unit test runs

    #[tokio::test]
    #[ignore = "requires tsgo binary"]
    async fn test_check_empty_files() {
        let config = TypeCheckConfig::default();
        let diagnostics = check_types(&[], &config).await.unwrap();
        assert!(diagnostics.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires tsgo binary"]
    async fn test_check_disabled() {
        let config = TypeCheckConfig::disabled();
        let files = vec![PathBuf::from("nonexistent.ts")];
        let diagnostics = check_types(&files, &config).await.unwrap();
        assert!(diagnostics.is_empty());
    }
}
