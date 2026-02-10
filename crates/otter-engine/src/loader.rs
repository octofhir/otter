//! ESM module loader
//!
//! Supports loading modules from:
//! - Local files via oxc-resolver (node_modules, tsconfig paths, etc.)
//! - `https://` URLs for remote modules (with allowlist-based security)

use crate::error::{EngineError, EngineResult};
use otter_nodejs::{NodeApiProfile, get_builtin_entry_for_profile, is_builtin_for_profile};
use oxc_resolver::{ResolveOptions, Resolver};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Module loader configuration
#[derive(Debug, Clone)]
pub struct LoaderConfig {
    /// Base directory for module resolution
    pub base_dir: PathBuf,

    /// Allowed remote hosts (glob patterns)
    pub remote_allowlist: Vec<String>,

    /// Cache directory for remote modules
    pub cache_dir: PathBuf,

    /// Import map for aliasing (applied before oxc-resolver)
    pub import_map: HashMap<String, String>,

    /// File extensions to resolve
    pub extensions: Vec<String>,

    /// Condition names for package.json exports (legacy, used for ESM)
    pub condition_names: Vec<String>,

    /// Condition names for ESM imports (import/export statements)
    /// e.g., ["import", "module", "node", "default"]
    pub esm_conditions: Vec<String>,

    /// Condition names for CJS requires (require() calls)
    /// e.g., ["require", "node", "default"]
    pub cjs_conditions: Vec<String>,

    /// Node.js builtin availability profile for module resolution/loading.
    pub node_api_profile: NodeApiProfile,
}

impl Default for LoaderConfig {
    fn default() -> Self {
        Self {
            base_dir: std::env::current_dir().unwrap_or_default(),
            remote_allowlist: vec![
                "https://esm.sh/*".into(),
                "https://cdn.skypack.dev/*".into(),
                "https://unpkg.com/*".into(),
            ],
            cache_dir: dirs::cache_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("otter/modules"),
            import_map: HashMap::new(),
            extensions: vec![
                ".ts".into(),
                ".tsx".into(),
                ".js".into(),
                ".jsx".into(),
                ".mjs".into(),
                ".mts".into(),
                ".json".into(),
            ],
            // Legacy field - keep for backward compatibility
            condition_names: vec!["import".into(), "module".into(), "default".into()],
            // ESM: prefer "import" and "module" entry points
            esm_conditions: vec![
                "import".into(),
                "module".into(),
                "node".into(),
                "default".into(),
            ],
            // CJS: prefer "require" entry points
            cjs_conditions: vec!["require".into(), "node".into(), "default".into()],
            // Secure-by-default loader profile (Node builtins disabled unless enabled explicitly).
            node_api_profile: NodeApiProfile::None,
        }
    }
}

/// Resolved module information
#[derive(Debug, Clone)]
pub struct ResolvedModule {
    pub specifier: String,
    pub url: String,
    pub source: String,
    pub source_type: SourceType,
    pub module_type: ModuleType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceType {
    JavaScript,
    TypeScript,
    Json,
}

/// Module format type (ESM vs CommonJS)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModuleType {
    /// ES Modules (import/export)
    #[default]
    ESM,
    /// CommonJS (require/module.exports)
    CommonJS,
}

impl ModuleType {
    /// Detect module type from file extension
    pub fn from_extension(ext: Option<&str>) -> Option<Self> {
        match ext {
            Some("cjs" | "cts") => Some(ModuleType::CommonJS),
            Some("mjs" | "mts") => Some(ModuleType::ESM),
            _ => None, // Need to check package.json
        }
    }

    /// Check if this is CommonJS
    pub fn is_commonjs(&self) -> bool {
        matches!(self, ModuleType::CommonJS)
    }

    /// Check if this is ESM
    pub fn is_esm(&self) -> bool {
        matches!(self, ModuleType::ESM)
    }
}

/// Import context for resolution
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImportContext {
    /// ESM import (import/export statements)
    #[default]
    ESM,
    /// CommonJS require (require() calls)
    CJS,
}

/// Module loader with caching and oxc-resolver integration
pub struct ModuleLoader {
    config: LoaderConfig,
    /// Resolver for ESM imports (uses esm_conditions)
    esm_resolver: Resolver,
    /// Resolver for CJS requires (uses cjs_conditions)
    cjs_resolver: Resolver,
    cache: Arc<RwLock<HashMap<String, ResolvedModule>>>,
}

impl ModuleLoader {
    pub fn new(config: LoaderConfig) -> Self {
        // ESM resolver with "import", "module" conditions
        let esm_options = ResolveOptions {
            extensions: config.extensions.clone(),
            condition_names: config.esm_conditions.clone(),
            ..ResolveOptions::default()
        };

        // CJS resolver with "require" conditions
        let cjs_options = ResolveOptions {
            extensions: config.extensions.clone(),
            condition_names: config.cjs_conditions.clone(),
            ..ResolveOptions::default()
        };

        Self {
            esm_resolver: Resolver::new(esm_options),
            cjs_resolver: Resolver::new(cjs_options),
            config,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Resolve and load a module (uses ESM conditions by default)
    pub async fn load(
        &self,
        specifier: &str,
        referrer: Option<&str>,
    ) -> EngineResult<ResolvedModule> {
        self.load_with_context(specifier, referrer, ImportContext::ESM)
            .await
    }

    /// Resolve and load a module with explicit import context
    pub async fn load_with_context(
        &self,
        specifier: &str,
        referrer: Option<&str>,
        context: ImportContext,
    ) -> EngineResult<ResolvedModule> {
        // Resolve the specifier with context
        let resolved_url = self.resolve_with_context(specifier, referrer, context)?;

        // Shared cache by canonical URL for mixed ESM/CJS graphs.
        // A single module instance must be reused regardless of import context.
        {
            let cache = self.cache.read().await;
            if let Some(module) = cache.get(&resolved_url) {
                return Ok(module.clone());
            }
        }

        // Load the module
        let module = self.load_url(&resolved_url).await?;

        // Cache the result
        {
            let mut cache = self.cache.write().await;
            cache.insert(resolved_url, module.clone());
        }

        Ok(module)
    }

    /// Resolve a module specifier to a URL or file path (uses ESM conditions by default)
    pub fn resolve(&self, specifier: &str, referrer: Option<&str>) -> EngineResult<String> {
        self.resolve_with_context(specifier, referrer, ImportContext::ESM)
    }

    /// Resolve a module specifier for a require() call (uses CJS conditions)
    pub fn resolve_require(&self, specifier: &str, referrer: Option<&str>) -> EngineResult<String> {
        self.resolve_with_context(specifier, referrer, ImportContext::CJS)
    }

    /// Resolve a module specifier with explicit import context
    ///
    /// The context determines which package.json exports conditions are used:
    /// - ESM: ["import", "module", "node", "default"]
    /// - CJS: ["require", "node", "default"]
    pub fn resolve_with_context(
        &self,
        specifier: &str,
        referrer: Option<&str>,
        context: ImportContext,
    ) -> EngineResult<String> {
        // Check import map first
        if let Some(mapped) = self.config.import_map.get(specifier) {
            return self.resolve_with_context(mapped, referrer, context);
        }

        // Otter built-in modules
        if specifier.starts_with("otter:") {
            return Ok(specifier.to_string());
        }
        if is_otter_builtin(specifier) {
            return Ok(format!("otter:{}", specifier));
        }

        // Node built-ins (prefixed and bare) are canonicalized to builtin://node:*
        if is_builtin_for_profile(specifier, self.config.node_api_profile) {
            let name = specifier.strip_prefix("node:").unwrap_or(specifier);
            return Ok(format!("builtin://node:{}", name));
        }

        // npm namespace support (initial Bun-style path): `npm:pkg` -> resolve as `pkg`.
        if let Some(raw) = specifier.strip_prefix("npm:") {
            let normalized = normalize_npm_specifier(raw).ok_or_else(|| {
                EngineError::ModuleError(format!("Invalid npm specifier '{}'", specifier))
            })?;
            return self.resolve_with_context(&normalized, referrer, context);
        }

        // Absolute URLs (https://, http://)
        if specifier.starts_with("https://") || specifier.starts_with("http://") {
            return self.validate_remote_url(specifier);
        }

        // File URLs - convert to path and resolve
        if specifier.starts_with("file://") {
            let path = specifier.strip_prefix("file://").unwrap_or(specifier);
            let canonical = canonicalize_path(Path::new(path));
            return Ok(format!("file://{}", canonical.display()));
        }

        // Use oxc-resolver for everything else (relative paths, bare specifiers)
        let base_dir = referrer
            .and_then(|r| {
                let path = r.strip_prefix("file://").unwrap_or(r);
                Path::new(path).parent().map(|p| p.to_path_buf())
            })
            .unwrap_or_else(|| self.config.base_dir.clone());

        // Select resolver based on import context
        let resolver = match context {
            ImportContext::ESM => &self.esm_resolver,
            ImportContext::CJS => &self.cjs_resolver,
        };

        match resolver.resolve(&base_dir, specifier) {
            Ok(resolution) => {
                let path = canonicalize_path(&resolution.full_path());
                Ok(format!("file://{}", path.display()))
            }
            Err(e) => Err(EngineError::ModuleError(format!(
                "Cannot resolve '{}' from '{}': {}",
                specifier,
                base_dir.display(),
                e
            ))),
        }
    }

    /// Validate remote URL against allowlist
    fn validate_remote_url(&self, url: &str) -> EngineResult<String> {
        for pattern in &self.config.remote_allowlist {
            if glob_match(pattern, url) {
                return Ok(url.to_string());
            }
        }

        Err(EngineError::ModuleError(format!(
            "Remote module '{}' not in allowlist. Add to remote_allowlist in config.",
            url
        )))
    }

    /// Load module from URL
    async fn load_url(&self, url: &str) -> EngineResult<ResolvedModule> {
        if let Some(name) = url.strip_prefix("builtin://node:") {
            return self.load_node_builtin(name);
        }

        // Otter built-in modules (e.g., "otter")
        if let Some(builtin) = url.strip_prefix("otter:") {
            return self.load_otter_builtin(builtin);
        }

        if url.starts_with("npm:") {
            return Err(EngineError::ModuleError(format!(
                "npm namespace is not configured for '{}'. Configure npm resolution/provider first.",
                url
            )));
        }

        if let Some(path) = url.strip_prefix("file://") {
            return self.load_file(path).await;
        }

        if url.starts_with("https://") {
            return self.load_remote(url).await;
        }

        Err(EngineError::ModuleError(format!(
            "Unsupported URL scheme: {}",
            url
        )))
    }

    /// Load a local file
    async fn load_file(&self, path: &str) -> EngineResult<ResolvedModule> {
        let path = PathBuf::from(path);

        let source = tokio::fs::read_to_string(&path).await.map_err(|e| {
            EngineError::ModuleError(format!("Failed to read '{}': {}", path.display(), e))
        })?;

        // Strip shebang if present (e.g., #!/usr/bin/env node)
        let source = strip_shebang(&source);

        let source_type = Self::source_type_from_path(&path);
        let module_type = self.detect_module_type(&path);

        Ok(ResolvedModule {
            specifier: path.display().to_string(),
            url: format!("file://{}", path.display()),
            source,
            source_type,
            module_type,
        })
    }

    /// Load a remote module
    async fn load_remote(&self, url: &str) -> EngineResult<ResolvedModule> {
        // Check disk cache first
        let cache_path = self.get_cache_path(url);
        if cache_path.exists() {
            let source = tokio::fs::read_to_string(&cache_path)
                .await
                .map_err(|e| EngineError::ModuleError(format!("Failed to read cache: {}", e)))?;

            return Ok(ResolvedModule {
                specifier: url.to_string(),
                url: url.to_string(),
                source,
                source_type: Self::source_type_from_url(url),
                module_type: Self::module_type_from_url(url),
            });
        }

        // Fetch from network
        let response = reqwest::get(url)
            .await
            .map_err(|e| EngineError::ModuleError(format!("Failed to fetch '{}': {}", url, e)))?;

        if !response.status().is_success() {
            return Err(EngineError::ModuleError(format!(
                "Failed to fetch '{}': HTTP {}",
                url,
                response.status()
            )));
        }

        let source = response
            .text()
            .await
            .map_err(|e| EngineError::ModuleError(format!("Failed to read response: {}", e)))?;

        // Cache to disk
        if let Some(parent) = cache_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let _ = tokio::fs::write(&cache_path, &source).await;

        Ok(ResolvedModule {
            specifier: url.to_string(),
            url: url.to_string(),
            source,
            source_type: Self::source_type_from_url(url),
            module_type: Self::module_type_from_url(url),
        })
    }

    /// Load an Otter built-in module (e.g., "otter")
    fn load_otter_builtin(&self, name: &str) -> EngineResult<ResolvedModule> {
        Ok(ResolvedModule {
            specifier: format!("otter:{}", name),
            url: format!("otter:{}", name),
            source: String::new(),
            source_type: SourceType::JavaScript,
            module_type: ModuleType::ESM, // Otter builtins are exposed as ESM
        })
    }

    /// Load a Node.js built-in module.
    ///
    /// All Node.js modules are now native extensions — returns empty source
    /// so the module loader knows the specifier is valid.
    fn load_node_builtin(&self, name: &str) -> EngineResult<ResolvedModule> {
        let _entry =
            get_builtin_entry_for_profile(name, self.config.node_api_profile).ok_or_else(|| {
                EngineError::ModuleError(format!(
                    "Node.js built-in module '{}' is unavailable for profile {:?}",
                    name, self.config.node_api_profile
                ))
            })?;

        Ok(ResolvedModule {
            specifier: format!("node:{}", name),
            url: format!("builtin://node:{}", name),
            source: String::new(),
            source_type: SourceType::JavaScript,
            module_type: ModuleType::ESM,
        })
    }

    /// Get cache path for a URL
    fn get_cache_path(&self, url: &str) -> PathBuf {
        let hash = format!("{:x}", md5::compute(url));
        self.config.cache_dir.join(&hash[..2]).join(&hash)
    }

    /// Determine source type from file path
    fn source_type_from_path(path: &Path) -> SourceType {
        match path.extension().and_then(|e| e.to_str()) {
            Some("ts") | Some("tsx") | Some("mts") | Some("cts") => SourceType::TypeScript,
            Some("json") => SourceType::Json,
            _ => SourceType::JavaScript,
        }
    }

    /// Determine source type from URL
    fn source_type_from_url(url: &str) -> SourceType {
        if url.ends_with(".ts") || url.ends_with(".tsx") || url.ends_with(".mts") {
            SourceType::TypeScript
        } else if url.ends_with(".json") {
            SourceType::Json
        } else {
            SourceType::JavaScript
        }
    }

    /// Determine module type from URL
    fn module_type_from_url(url: &str) -> ModuleType {
        if url.ends_with(".cjs") || url.ends_with(".cts") {
            ModuleType::CommonJS
        } else if url.ends_with(".mjs") || url.ends_with(".mts") {
            ModuleType::ESM
        } else {
            // Remote modules are assumed to be ESM (e.g., esm.sh, skypack)
            ModuleType::ESM
        }
    }

    /// Clear the in-memory cache
    pub async fn clear_cache(&self) {
        let mut cache = self.cache.write().await;
        cache.clear();
    }

    /// Get the loader configuration
    pub fn config(&self) -> &LoaderConfig {
        &self.config
    }

    pub fn detect_module_type(&self, path: &Path) -> ModuleType {
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if let Some(module_type) = ModuleType::from_extension(Some(ext)) {
                return module_type;
            }

            if matches!(ext, "ts" | "tsx" | "mts") {
                return ModuleType::ESM;
            }
        }

        if let Some(pkg_type) = self.find_package_type(path) {
            if pkg_type == "module" {
                return ModuleType::ESM;
            } else if pkg_type == "commonjs" {
                return ModuleType::CommonJS;
            }
        }

        ModuleType::ESM
    }

    /// Find the nearest package.json and return its "type" field value.
    /// Returns "commonjs" if package.json exists but has no "type" field (Node.js default).
    fn find_package_type(&self, path: &Path) -> Option<String> {
        let mut current = path.parent()?;

        loop {
            let pkg_path = current.join("package.json");
            if pkg_path.exists() {
                if let Ok(content) = std::fs::read_to_string(&pkg_path) {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let Some(type_field) = json.get("type").and_then(|v| v.as_str()) {
                            return Some(type_field.to_string());
                        }
                    }
                }
                // Found package.json but no "type" field - Node.js defaults to CommonJS
                return Some("commonjs".to_string());
            }

            match current.parent() {
                Some(parent) if parent != current => current = parent,
                _ => break,
            }
        }

        None
    }
}

/// Check if a specifier is an Otter built-in module
fn is_otter_builtin(specifier: &str) -> bool {
    matches!(specifier, "otter")
}

/// Normalize `npm:` specifier payload to a bare package specifier usable by resolver.
fn normalize_npm_specifier(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }

    if raw.starts_with('@') {
        let slash = raw.find('/')?;
        let after_scope = &raw[slash + 1..];
        let pkg_end_rel = after_scope.find('/').unwrap_or(after_scope.len());
        let pkg_and_ver = &after_scope[..pkg_end_rel];

        let (pkg_name, rest_after_pkg) = if let Some(ver_at) = pkg_and_ver.find('@') {
            (&pkg_and_ver[..ver_at], &after_scope[pkg_end_rel..])
        } else {
            (pkg_and_ver, &after_scope[pkg_end_rel..])
        };

        if pkg_name.is_empty() {
            return None;
        }

        return Some(format!(
            "@{}/{}{}",
            &raw[1..slash],
            pkg_name,
            rest_after_pkg
        ));
    }

    let slash = raw.find('/').unwrap_or(raw.len());
    let pkg_and_ver = &raw[..slash];
    let rest = &raw[slash..];
    let pkg = pkg_and_ver.split('@').next().unwrap_or_default();
    if pkg.is_empty() {
        return None;
    }
    Some(format!("{}{}", pkg, rest))
}

/// Simple glob matching for URL patterns
fn glob_match(pattern: &str, url: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix("/*") {
        url.starts_with(prefix)
    } else if let Some(prefix) = pattern.strip_suffix('*') {
        url.starts_with(prefix)
    } else {
        pattern == url
    }
}

/// Strip shebang line from source code if present.
///
/// Replaces the shebang line with spaces to preserve line numbers for error messages.
fn strip_shebang(source: &str) -> String {
    if source.starts_with("#!") {
        if let Some(newline_pos) = source.find('\n') {
            format!("{}{}", " ".repeat(newline_pos), &source[newline_pos..])
        } else {
            String::new()
        }
    } else {
        source.to_string()
    }
}

fn canonicalize_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn loader_for_profile(profile: NodeApiProfile) -> ModuleLoader {
        ModuleLoader::new(LoaderConfig {
            node_api_profile: profile,
            ..Default::default()
        })
    }

    #[test]
    fn test_resolve_node_imports_prefixed() {
        let loader = loader_for_profile(NodeApiProfile::Full);
        let result = loader.resolve("node:fs", None).unwrap();
        assert_eq!(result, "builtin://node:fs");
    }

    #[test]
    fn test_resolve_node_imports_bare() {
        let loader = loader_for_profile(NodeApiProfile::Full);
        let result = loader.resolve("path", None).unwrap();
        assert_eq!(result, "builtin://node:path");
    }

    #[test]
    fn test_resolve_node_imports_none_profile() {
        let loader = loader_for_profile(NodeApiProfile::None);
        assert!(loader.resolve("node:fs", None).is_err());
        assert!(loader.resolve("path", None).is_err());
    }

    #[test]
    fn test_resolve_node_imports_safe_profile() {
        let loader = loader_for_profile(NodeApiProfile::SafeCore);
        let path = loader.resolve("path", None).unwrap();
        assert_eq!(path, "builtin://node:path");
        assert!(loader.resolve("process", None).is_err());
    }

    #[test]
    fn test_resolve_remote_allowed() {
        let loader = ModuleLoader::new(LoaderConfig::default());
        let result = loader.resolve("https://esm.sh/lodash", None).unwrap();
        assert_eq!(result, "https://esm.sh/lodash");
    }

    #[test]
    fn test_resolve_remote_skypack() {
        let loader = ModuleLoader::new(LoaderConfig::default());
        let result = loader
            .resolve("https://cdn.skypack.dev/react", None)
            .unwrap();
        assert_eq!(result, "https://cdn.skypack.dev/react");
    }

    #[test]
    fn test_resolve_remote_denied() {
        let loader = ModuleLoader::new(LoaderConfig::default());
        let result = loader.resolve("https://evil.com/malware.js", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_import_map() {
        let mut import_map = HashMap::new();
        import_map.insert("lodash".to_string(), "https://esm.sh/lodash@4".to_string());

        let config = LoaderConfig {
            import_map,
            ..Default::default()
        };
        let loader = ModuleLoader::new(config);

        let result = loader.resolve("lodash", None).unwrap();
        assert_eq!(result, "https://esm.sh/lodash@4");
    }

    #[test]
    fn test_resolve_file_url_passthrough() {
        let loader = ModuleLoader::new(LoaderConfig::default());
        let result = loader.resolve("file:///some/path/module.js", None).unwrap();
        assert_eq!(result, "file:///some/path/module.js");
    }

    #[test]
    fn test_resolve_npm_namespace_to_node_modules() {
        let dir = tempdir().unwrap();
        let pkg_dir = dir.path().join("node_modules").join("lodash");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{
  "name": "lodash",
  "main": "index.js"
}"#,
        )
        .unwrap();
        std::fs::write(pkg_dir.join("index.js"), "module.exports = {};").unwrap();

        let main = dir.path().join("main.mjs");
        std::fs::write(&main, "import _ from 'npm:lodash';").unwrap();

        let config = LoaderConfig {
            base_dir: dir.path().to_path_buf(),
            ..Default::default()
        };
        let loader = ModuleLoader::new(config);
        let referrer = format!("file://{}", main.display());
        let result = loader.resolve("npm:lodash@4", Some(&referrer)).unwrap();
        assert!(result.ends_with("node_modules/lodash/index.js"));
    }

    #[test]
    fn test_normalize_npm_specifier() {
        assert_eq!(normalize_npm_specifier("lodash").as_deref(), Some("lodash"));
        assert_eq!(
            normalize_npm_specifier("lodash@4/fp").as_deref(),
            Some("lodash/fp")
        );
        assert_eq!(
            normalize_npm_specifier("@scope/pkg@1.2.3/sub").as_deref(),
            Some("@scope/pkg/sub")
        );
        assert!(normalize_npm_specifier("").is_none());
    }

    #[test]
    fn test_glob_match() {
        assert!(glob_match("https://esm.sh/*", "https://esm.sh/lodash"));
        assert!(glob_match("https://esm.sh/*", "https://esm.sh/react@18"));
        assert!(!glob_match("https://esm.sh/*", "https://other.com/lodash"));
        assert!(glob_match(
            "https://example.com/lib*",
            "https://example.com/library"
        ));
        assert!(glob_match(
            "https://exact.com/path",
            "https://exact.com/path"
        ));
        assert!(!glob_match(
            "https://exact.com/path",
            "https://exact.com/other"
        ));
    }

    #[test]
    fn test_source_type_from_path() {
        assert_eq!(
            ModuleLoader::source_type_from_path(Path::new("file.ts")),
            SourceType::TypeScript
        );
        assert_eq!(
            ModuleLoader::source_type_from_path(Path::new("file.tsx")),
            SourceType::TypeScript
        );
        assert_eq!(
            ModuleLoader::source_type_from_path(Path::new("file.mts")),
            SourceType::TypeScript
        );
        assert_eq!(
            ModuleLoader::source_type_from_path(Path::new("file.json")),
            SourceType::Json
        );
        assert_eq!(
            ModuleLoader::source_type_from_path(Path::new("file.js")),
            SourceType::JavaScript
        );
        assert_eq!(
            ModuleLoader::source_type_from_path(Path::new("file.mjs")),
            SourceType::JavaScript
        );
    }

    #[test]
    fn test_source_type_from_url() {
        assert_eq!(
            ModuleLoader::source_type_from_url("https://example.com/file.ts"),
            SourceType::TypeScript
        );
        assert_eq!(
            ModuleLoader::source_type_from_url("https://example.com/file.json"),
            SourceType::Json
        );
        assert_eq!(
            ModuleLoader::source_type_from_url("https://example.com/file.js"),
            SourceType::JavaScript
        );
    }

    #[test]
    fn test_module_type_from_extension() {
        assert_eq!(
            ModuleType::from_extension(Some("cjs")),
            Some(ModuleType::CommonJS)
        );
        assert_eq!(
            ModuleType::from_extension(Some("cts")),
            Some(ModuleType::CommonJS)
        );
        assert_eq!(
            ModuleType::from_extension(Some("mjs")),
            Some(ModuleType::ESM)
        );
        assert_eq!(
            ModuleType::from_extension(Some("mts")),
            Some(ModuleType::ESM)
        );
        assert_eq!(ModuleType::from_extension(Some("js")), None);
        assert_eq!(ModuleType::from_extension(Some("ts")), None);
    }

    #[test]
    fn test_module_type_from_url() {
        assert_eq!(
            ModuleLoader::module_type_from_url("https://example.com/file.cjs"),
            ModuleType::CommonJS
        );
        assert_eq!(
            ModuleLoader::module_type_from_url("https://example.com/file.mjs"),
            ModuleType::ESM
        );
        assert_eq!(
            ModuleLoader::module_type_from_url("https://esm.sh/lodash"),
            ModuleType::ESM
        );
    }

    #[test]
    fn test_detect_module_type_explicit_extension() {
        let loader = ModuleLoader::new(LoaderConfig::default());

        assert_eq!(
            loader.detect_module_type(Path::new("/foo/bar.cjs")),
            ModuleType::CommonJS
        );
        assert_eq!(
            loader.detect_module_type(Path::new("/foo/bar.mjs")),
            ModuleType::ESM
        );
        assert_eq!(
            loader.detect_module_type(Path::new("/foo/bar.cts")),
            ModuleType::CommonJS
        );
        assert_eq!(
            loader.detect_module_type(Path::new("/foo/bar.mts")),
            ModuleType::ESM
        );
    }

    #[test]
    fn test_detect_module_type_default_esm() {
        let loader = ModuleLoader::new(LoaderConfig::default());

        assert_eq!(
            loader.detect_module_type(Path::new("/nonexistent/path/file.js")),
            ModuleType::ESM
        );
    }

    #[tokio::test]
    async fn test_shared_cache_between_esm_and_cjs_contexts() {
        let dir = tempdir().unwrap();
        let shared_path = dir.path().join("shared.cjs");
        let main_path = dir.path().join("main.mjs");

        std::fs::write(&shared_path, "module.exports = { value: 1 };").unwrap();
        std::fs::write(&main_path, "import x from './shared.cjs';").unwrap();

        let loader = ModuleLoader::new(LoaderConfig {
            base_dir: dir.path().to_path_buf(),
            ..Default::default()
        });

        let referrer = format!("file://{}", main_path.display());
        let first = loader
            .load_with_context("./shared.cjs", Some(&referrer), ImportContext::ESM)
            .await
            .unwrap();

        // Mutate file after first load. If ESM/CJS use separate cache entries,
        // second load would observe changed source.
        std::fs::write(&shared_path, "module.exports = { value: 2 };").unwrap();

        let second = loader
            .load_with_context("./shared.cjs", Some(&referrer), ImportContext::CJS)
            .await
            .unwrap();

        assert_eq!(first.url, second.url);
        assert_eq!(first.source, second.source);
    }
}
