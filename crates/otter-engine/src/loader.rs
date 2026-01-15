//! ESM module loader
//!
//! Supports loading modules from:
//! - Local files via oxc-resolver (node_modules, tsconfig paths, etc.)
//! - `node:` URLs for Node.js built-in modules
//! - `https://` URLs for remote modules (with allowlist-based security)

use otter_runtime::{JscError, JscResult};
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

    /// Condition names for package.json exports
    pub condition_names: Vec<String>,
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
            condition_names: vec!["import".into(), "module".into(), "default".into()],
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceType {
    JavaScript,
    TypeScript,
    Json,
}

/// Module loader with caching and oxc-resolver integration
pub struct ModuleLoader {
    config: LoaderConfig,
    resolver: Resolver,
    cache: Arc<RwLock<HashMap<String, ResolvedModule>>>,
}

impl ModuleLoader {
    pub fn new(config: LoaderConfig) -> Self {
        let resolve_options = ResolveOptions {
            extensions: config.extensions.clone(),
            condition_names: config.condition_names.clone(),
            ..ResolveOptions::default()
        };

        Self {
            config,
            resolver: Resolver::new(resolve_options),
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Resolve and load a module
    pub async fn load(&self, specifier: &str, referrer: Option<&str>) -> JscResult<ResolvedModule> {
        // Check cache first
        let cache_key = format!("{}|{}", specifier, referrer.unwrap_or(""));
        {
            let cache = self.cache.read().await;
            if let Some(module) = cache.get(&cache_key) {
                return Ok(module.clone());
            }
        }

        // Resolve the specifier
        let resolved_url = self.resolve(specifier, referrer)?;

        // Load the module
        let module = self.load_url(&resolved_url).await?;

        // Cache the result
        {
            let mut cache = self.cache.write().await;
            cache.insert(cache_key, module.clone());
        }

        Ok(module)
    }

    /// Resolve a module specifier to a URL or file path
    pub fn resolve(&self, specifier: &str, referrer: Option<&str>) -> JscResult<String> {
        // Check import map first
        if let Some(mapped) = self.config.import_map.get(specifier) {
            return self.resolve(mapped, referrer);
        }

        // Node.js built-in modules
        if specifier.starts_with("node:") {
            return Ok(specifier.to_string());
        }

        // Absolute URLs (https://, http://)
        if specifier.starts_with("https://") || specifier.starts_with("http://") {
            return self.validate_remote_url(specifier);
        }

        // File URLs - convert to path and resolve
        if specifier.starts_with("file://") {
            return Ok(specifier.to_string());
        }

        // Use oxc-resolver for everything else (relative paths, bare specifiers)
        let base_dir = referrer
            .and_then(|r| {
                let path = r.strip_prefix("file://").unwrap_or(r);
                Path::new(path).parent().map(|p| p.to_path_buf())
            })
            .unwrap_or_else(|| self.config.base_dir.clone());

        match self.resolver.resolve(&base_dir, specifier) {
            Ok(resolution) => {
                let path = resolution.full_path();
                Ok(format!("file://{}", path.display()))
            }
            Err(e) => Err(JscError::ModuleError(format!(
                "Cannot resolve '{}' from '{}': {}",
                specifier,
                base_dir.display(),
                e
            ))),
        }
    }

    /// Validate remote URL against allowlist
    fn validate_remote_url(&self, url: &str) -> JscResult<String> {
        for pattern in &self.config.remote_allowlist {
            if glob_match(pattern, url) {
                return Ok(url.to_string());
            }
        }

        Err(JscError::ModuleError(format!(
            "Remote module '{}' not in allowlist. Add to remote_allowlist in config.",
            url
        )))
    }

    /// Load module from URL
    async fn load_url(&self, url: &str) -> JscResult<ResolvedModule> {
        if let Some(builtin) = url.strip_prefix("node:") {
            return self.load_node_builtin(builtin);
        }

        if let Some(path) = url.strip_prefix("file://") {
            return self.load_file(path).await;
        }

        if url.starts_with("https://") {
            return self.load_remote(url).await;
        }

        Err(JscError::ModuleError(format!(
            "Unsupported URL scheme: {}",
            url
        )))
    }

    /// Load a local file
    async fn load_file(&self, path: &str) -> JscResult<ResolvedModule> {
        let path = PathBuf::from(path);

        let source = tokio::fs::read_to_string(&path).await.map_err(|e| {
            JscError::ModuleError(format!("Failed to read '{}': {}", path.display(), e))
        })?;

        let source_type = Self::source_type_from_path(&path);

        Ok(ResolvedModule {
            specifier: path.display().to_string(),
            url: format!("file://{}", path.display()),
            source,
            source_type,
        })
    }

    /// Load a remote module
    async fn load_remote(&self, url: &str) -> JscResult<ResolvedModule> {
        // Check disk cache first
        let cache_path = self.get_cache_path(url);
        if cache_path.exists() {
            let source = tokio::fs::read_to_string(&cache_path)
                .await
                .map_err(|e| JscError::ModuleError(format!("Failed to read cache: {}", e)))?;

            return Ok(ResolvedModule {
                specifier: url.to_string(),
                url: url.to_string(),
                source,
                source_type: Self::source_type_from_url(url),
            });
        }

        // Fetch from network
        let response = reqwest::get(url)
            .await
            .map_err(|e| JscError::ModuleError(format!("Failed to fetch '{}': {}", url, e)))?;

        if !response.status().is_success() {
            return Err(JscError::ModuleError(format!(
                "Failed to fetch '{}': HTTP {}",
                url,
                response.status()
            )));
        }

        let source = response
            .text()
            .await
            .map_err(|e| JscError::ModuleError(format!("Failed to read response: {}", e)))?;

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
        })
    }

    /// Load a Node.js built-in module
    fn load_node_builtin(&self, name: &str) -> JscResult<ResolvedModule> {
        Ok(ResolvedModule {
            specifier: format!("node:{}", name),
            url: format!("node:{}", name),
            source: String::new(),
            source_type: SourceType::JavaScript,
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

    /// Clear the in-memory cache
    pub async fn clear_cache(&self) {
        let mut cache = self.cache.write().await;
        cache.clear();
    }

    /// Get the loader configuration
    pub fn config(&self) -> &LoaderConfig {
        &self.config
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_node_builtin() {
        let loader = ModuleLoader::new(LoaderConfig::default());
        let result = loader.resolve("node:fs", None).unwrap();
        assert_eq!(result, "node:fs");
    }

    #[test]
    fn test_resolve_node_builtin_path() {
        let loader = ModuleLoader::new(LoaderConfig::default());
        let result = loader.resolve("node:path", None).unwrap();
        assert_eq!(result, "node:path");
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
}
