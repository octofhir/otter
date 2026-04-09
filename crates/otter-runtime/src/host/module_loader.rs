use oxc_resolver::{ResolveOptions, Resolver};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

const REMOTE_MODULE_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Module loader configuration for hosted entry resolution and loading.
#[derive(Debug, Clone)]
pub struct ModuleLoaderConfig {
    /// Base directory for module resolution.
    pub base_dir: PathBuf,
    /// Allowed remote hosts.
    pub remote_allowlist: Vec<String>,
    /// Cache directory for remote modules.
    pub cache_dir: PathBuf,
    /// Import map for specifier remapping.
    pub import_map: HashMap<String, String>,
    /// File extensions to resolve.
    pub extensions: Vec<String>,
    /// Condition names for ESM imports.
    pub esm_conditions: Vec<String>,
    /// Condition names for CommonJS requires.
    pub cjs_conditions: Vec<String>,
}

impl Default for ModuleLoaderConfig {
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
            esm_conditions: vec![
                "import".into(),
                "module".into(),
                "node".into(),
                "default".into(),
            ],
            cjs_conditions: vec!["require".into(), "node".into(), "default".into()],
        }
    }
}

/// Resolved module information.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModuleType {
    #[default]
    Esm,
    CommonJs,
}

impl ModuleType {
    fn from_extension(ext: Option<&str>) -> Option<Self> {
        match ext {
            Some("cjs" | "cts") => Some(Self::CommonJs),
            Some("mjs" | "mts") => Some(Self::Esm),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImportContext {
    #[default]
    Esm,
    Cjs,
}

/// Errors from hosted module resolution/loading.
#[derive(Debug, thiserror::Error)]
pub enum ModuleLoaderError {
    #[error("module resolution failed: {0}")]
    Resolve(String),
    #[error("module load failed: {0}")]
    Load(String),
}

const MAX_SOURCE_CACHE_SIZE: usize = 256;

struct SourceCache {
    map: HashMap<String, ResolvedModule>,
    order: VecDeque<String>,
}

impl SourceCache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&self, url: &str) -> Option<&ResolvedModule> {
        self.map.get(url)
    }

    fn insert(&mut self, url: String, module: ResolvedModule) {
        if let Some(existing) = self.map.get_mut(&url) {
            *existing = module;
            return;
        }

        if self.map.len() >= MAX_SOURCE_CACHE_SIZE
            && let Some(oldest) = self.order.pop_front()
        {
            self.map.remove(&oldest);
        }

        self.order.push_back(url.clone());
        self.map.insert(url, module);
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

/// Hosted module loader for the new runtime stack.
pub struct ModuleLoader {
    config: ModuleLoaderConfig,
    esm_resolver: Resolver,
    cjs_resolver: Resolver,
    cache: Arc<RwLock<SourceCache>>,
}

impl ModuleLoader {
    pub fn new(config: ModuleLoaderConfig) -> Self {
        let esm_options = ResolveOptions {
            extensions: config.extensions.clone(),
            condition_names: config.esm_conditions.clone(),
            ..ResolveOptions::default()
        };

        let cjs_options = ResolveOptions {
            extensions: config.extensions.clone(),
            condition_names: config.cjs_conditions.clone(),
            ..ResolveOptions::default()
        };

        Self {
            esm_resolver: Resolver::new(esm_options),
            cjs_resolver: Resolver::new(cjs_options),
            config,
            cache: Arc::new(RwLock::new(SourceCache::new())),
        }
    }

    pub fn resolve(
        &self,
        specifier: &str,
        referrer: Option<&str>,
    ) -> Result<String, ModuleLoaderError> {
        self.resolve_with_context(specifier, referrer, ImportContext::Esm)
    }

    pub fn resolve_with_context(
        &self,
        specifier: &str,
        referrer: Option<&str>,
        context: ImportContext,
    ) -> Result<String, ModuleLoaderError> {
        if let Some(mapped) = self.config.import_map.get(specifier) {
            return self.resolve_with_context(mapped, referrer, context);
        }

        if specifier.starts_with("otter:") {
            return Ok(specifier.to_string());
        }
        if is_otter_builtin(specifier) {
            return Ok(format!("otter:{specifier}"));
        }

        if let Some(raw) = specifier.strip_prefix("npm:") {
            let normalized = normalize_npm_specifier(raw).ok_or_else(|| {
                ModuleLoaderError::Resolve(format!("invalid npm specifier '{specifier}'"))
            })?;
            return self.resolve_with_context(&normalized, referrer, context);
        }

        if specifier.starts_with("https://") || specifier.starts_with("http://") {
            return self.validate_remote_url(specifier);
        }

        if specifier.starts_with("file://") {
            let path = specifier.strip_prefix("file://").unwrap_or(specifier);
            let canonical = canonicalize_path(Path::new(path));
            return Ok(format!("file://{}", canonical.display()));
        }

        let base_dir = referrer
            .and_then(|r| {
                let path = r.strip_prefix("file://").unwrap_or(r);
                Path::new(path).parent().map(|p| p.to_path_buf())
            })
            .unwrap_or_else(|| self.config.base_dir.clone());

        let resolver = match context {
            ImportContext::Esm => &self.esm_resolver,
            ImportContext::Cjs => &self.cjs_resolver,
        };

        match resolver.resolve(&base_dir, specifier) {
            Ok(resolution) => {
                let path = canonicalize_path(&resolution.full_path());
                Ok(format!("file://{}", path.display()))
            }
            Err(error) => Err(ModuleLoaderError::Resolve(format!(
                "cannot resolve '{specifier}' from '{}': {error}",
                base_dir.display()
            ))),
        }
    }

    pub fn load(
        &self,
        specifier: &str,
        referrer: Option<&str>,
    ) -> Result<ResolvedModule, ModuleLoaderError> {
        self.load_with_context(specifier, referrer, ImportContext::Esm)
    }

    pub fn load_with_context(
        &self,
        specifier: &str,
        referrer: Option<&str>,
        context: ImportContext,
    ) -> Result<ResolvedModule, ModuleLoaderError> {
        let resolved_url = self.resolve_with_context(specifier, referrer, context)?;

        if let Some(module) = self
            .cache
            .read()
            .expect("cache poisoned")
            .get(&resolved_url)
        {
            return Ok(module.clone());
        }

        let module = self.load_url(&resolved_url)?;
        self.cache
            .write()
            .expect("cache poisoned")
            .insert(resolved_url, module.clone());
        Ok(module)
    }

    fn validate_remote_url(&self, url: &str) -> Result<String, ModuleLoaderError> {
        for pattern in &self.config.remote_allowlist {
            if glob_match(pattern, url) {
                return Ok(url.to_string());
            }
        }

        Err(ModuleLoaderError::Resolve(format!(
            "remote module '{url}' not in allowlist"
        )))
    }

    fn load_url(&self, url: &str) -> Result<ResolvedModule, ModuleLoaderError> {
        if let Some(name) = url.strip_prefix("otter:") {
            return Ok(ResolvedModule {
                specifier: format!("otter:{name}"),
                url: format!("otter:{name}"),
                source: String::new(),
                source_type: SourceType::JavaScript,
                module_type: ModuleType::Esm,
            });
        }

        if let Some(path) = url.strip_prefix("file://") {
            return self.load_file(path);
        }

        if url.starts_with("https://") || url.starts_with("http://") {
            return self.load_remote(url);
        }

        Err(ModuleLoaderError::Load(format!(
            "unsupported URL scheme for '{url}'"
        )))
    }

    fn load_file(&self, path: &str) -> Result<ResolvedModule, ModuleLoaderError> {
        let path = PathBuf::from(path);
        let source = std::fs::read_to_string(&path).map_err(|error| {
            ModuleLoaderError::Load(format!("failed to read '{}': {error}", path.display()))
        })?;

        Ok(ResolvedModule {
            specifier: path.display().to_string(),
            url: format!("file://{}", path.display()),
            source: strip_shebang(&source),
            source_type: Self::source_type_from_path(&path),
            module_type: self.detect_module_type(&path),
        })
    }

    fn load_remote(&self, url: &str) -> Result<ResolvedModule, ModuleLoaderError> {
        let cache_path = self.get_cache_path(url);
        if cache_path.exists() {
            let source = std::fs::read_to_string(&cache_path).map_err(|error| {
                ModuleLoaderError::Load(format!(
                    "failed to read cache '{}': {error}",
                    cache_path.display()
                ))
            })?;

            return Ok(ResolvedModule {
                specifier: url.to_string(),
                url: url.to_string(),
                source,
                source_type: Self::source_type_from_url(url),
                module_type: Self::module_type_from_url(url),
            });
        }

        let client = reqwest::blocking::Client::builder()
            .connect_timeout(REMOTE_MODULE_HTTP_TIMEOUT)
            .timeout(REMOTE_MODULE_HTTP_TIMEOUT)
            .build()
            .map_err(|error| {
                ModuleLoaderError::Load(format!("failed to initialize HTTP module loader: {error}"))
            })?;

        let response = client
            .get(url)
            .send()
            .and_then(|response| response.error_for_status())
            .map_err(|error| {
                ModuleLoaderError::Load(format!("failed to fetch '{url}': {error}"))
            })?;

        let source = response.text().map_err(|error| {
            ModuleLoaderError::Load(format!("failed to read response for '{url}': {error}"))
        })?;

        if let Some(parent) = cache_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&cache_path, &source);

        Ok(ResolvedModule {
            specifier: url.to_string(),
            url: url.to_string(),
            source,
            source_type: Self::source_type_from_url(url),
            module_type: Self::module_type_from_url(url),
        })
    }

    fn get_cache_path(&self, url: &str) -> PathBuf {
        let hash = format!("{:x}", md5::compute(url));
        self.config.cache_dir.join(&hash[..2]).join(&hash)
    }

    fn source_type_from_path(path: &Path) -> SourceType {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("ts") | Some("tsx") | Some("mts") | Some("cts") => SourceType::TypeScript,
            Some("json") => SourceType::Json,
            _ => SourceType::JavaScript,
        }
    }

    fn source_type_from_url(url: &str) -> SourceType {
        if url.ends_with(".ts") || url.ends_with(".tsx") || url.ends_with(".mts") {
            SourceType::TypeScript
        } else if url.ends_with(".json") {
            SourceType::Json
        } else {
            SourceType::JavaScript
        }
    }

    fn module_type_from_url(url: &str) -> ModuleType {
        if url.ends_with(".cjs") || url.ends_with(".cts") {
            ModuleType::CommonJs
        } else {
            ModuleType::Esm
        }
    }

    pub fn clear_cache(&self) {
        self.cache.write().expect("cache poisoned").clear();
    }

    pub fn config(&self) -> &ModuleLoaderConfig {
        &self.config
    }

    pub fn detect_module_type(&self, path: &Path) -> ModuleType {
        if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
            if let Some(module_type) = ModuleType::from_extension(Some(ext)) {
                return module_type;
            }

            if matches!(ext, "ts" | "tsx" | "mts") {
                return ModuleType::Esm;
            }
        }

        if let Some(pkg_type) = self.find_package_type(path) {
            return if pkg_type == "commonjs" {
                ModuleType::CommonJs
            } else {
                ModuleType::Esm
            };
        }

        ModuleType::Esm
    }

    fn find_package_type(&self, path: &Path) -> Option<String> {
        let mut current = path.parent()?;

        loop {
            let pkg_path = current.join("package.json");
            if pkg_path.exists() {
                if let Ok(content) = std::fs::read_to_string(&pkg_path)
                    && let Ok(json) = serde_json::from_str::<serde_json::Value>(&content)
                    && let Some(type_field) = json.get("type").and_then(|value| value.as_str())
                {
                    return Some(type_field.to_string());
                }
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

fn is_otter_builtin(specifier: &str) -> bool {
    matches!(specifier, "otter")
}

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
    Some(format!("{pkg}{rest}"))
}

fn glob_match(pattern: &str, url: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix("/*") {
        url.starts_with(prefix)
    } else if let Some(prefix) = pattern.strip_suffix('*') {
        url.starts_with(prefix)
    } else {
        pattern == url
    }
}

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

    #[test]
    fn test_resolve_remote_allowed() {
        let loader = ModuleLoader::new(ModuleLoaderConfig::default());
        let result = loader
            .resolve("https://esm.sh/lodash", None)
            .expect("remote specifier should be allowed");
        assert_eq!(result, "https://esm.sh/lodash");
    }

    #[test]
    fn test_resolve_import_map() {
        let mut import_map = HashMap::new();
        import_map.insert("lodash".to_string(), "https://esm.sh/lodash@4".to_string());

        let loader = ModuleLoader::new(ModuleLoaderConfig {
            import_map,
            ..Default::default()
        });

        let result = loader
            .resolve("lodash", None)
            .expect("import map should resolve");
        assert_eq!(result, "https://esm.sh/lodash@4");
    }

    #[test]
    fn test_glob_match() {
        assert!(glob_match("https://esm.sh/*", "https://esm.sh/react@18"));
        assert!(!glob_match("https://esm.sh/*", "https://other.com/react"));
    }
}
