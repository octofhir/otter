//! Module loader for ES modules and CommonJS
//!
//! Handles loading, resolving, and executing JavaScript modules.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use otter_vm_bytecode::Module;
use otter_vm_bytecode::module::{ExportRecord, ImportBinding, ImportRecord};
use otter_vm_compiler::Compiler;
use otter_vm_core::gc::GcRef;
use otter_vm_core::memory::MemoryManager;
use otter_vm_core::object::JsObject;
use otter_vm_core::object::PropertyKey;
use otter_vm_core::value::Value;

use crate::module_provider::{ModuleResolution, ModuleType};
use oxc_resolver::{ResolveOptions, Resolver};

/// Module loading error
#[derive(Debug, Clone)]
pub enum ModuleError {
    /// Module not found
    NotFound(String),
    /// Compile error
    CompileError(String),
    /// Resolution error
    ResolveError(String),
    /// Circular import detected during linking
    CircularImport(String),
    /// Export not found
    ExportNotFound {
        /// Module specifier
        module: String,
        /// Export name
        export: String,
    },
    /// IO error
    IoError(String),
}

impl std::fmt::Display for ModuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModuleError::NotFound(url) => write!(f, "Module not found: {}", url),
            ModuleError::CompileError(msg) => write!(f, "Compile error: {}", msg),
            ModuleError::ResolveError(msg) => write!(f, "Resolution error: {}", msg),
            ModuleError::CircularImport(url) => write!(f, "Circular import: {}", url),
            ModuleError::ExportNotFound { module, export } => {
                write!(f, "Export '{}' not found in module '{}'", export, module)
            }
            ModuleError::IoError(msg) => write!(f, "IO error: {}", msg),
        }
    }
}

impl std::error::Error for ModuleError {}

/// Module state during loading and evaluation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleState {
    /// Module bytecode loaded but not linked
    Unlinked,
    /// Currently linking (resolving imports)
    Linking,
    /// Linked and ready for evaluation
    Linked,
    /// Currently evaluating
    Evaluating,
    /// Fully evaluated
    Evaluated,
    /// Error during evaluation
    Error,
}

/// Import context for condition-aware resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImportContext {
    /// ESM import()/import declaration context.
    #[default]
    ESM,
    /// CommonJS require() context.
    CJS,
}

/// Module namespace object that holds exports
#[derive(Debug, Default)]
pub struct ModuleNamespace {
    /// Exported values by name
    exports: RwLock<HashMap<String, Value>>,
}

impl ModuleNamespace {
    /// Create a new empty namespace
    pub fn new() -> Self {
        Self {
            exports: RwLock::new(HashMap::new()),
        }
    }

    /// Get an export by name
    pub fn get(&self, name: &str) -> Option<Value> {
        self.exports.read().ok()?.get(name).cloned()
    }

    /// Set an export
    pub fn set(&self, name: &str, value: Value) {
        if let Ok(mut exports) = self.exports.write() {
            exports.insert(name.to_string(), value);
        }
    }

    /// Get all export names
    pub fn keys(&self) -> Vec<String> {
        self.exports
            .read()
            .map(|e| e.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Snapshot all namespace entries.
    pub fn entries(&self) -> Vec<(String, Value)> {
        self.exports
            .read()
            .map(|e| e.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    /// Check if export exists
    pub fn has(&self, name: &str) -> bool {
        self.exports
            .read()
            .map(|e| e.contains_key(name))
            .unwrap_or(false)
    }

    /// Clear all exports in this namespace.
    pub fn clear(&self) {
        if let Ok(mut exports) = self.exports.write() {
            exports.clear();
        }
    }

    /// Convert to a Value (object)
    pub fn to_value(&self) -> Value {
        // For now return undefined, interpreter will handle this
        Value::undefined()
    }
}

pub fn namespace_to_object(namespace: &ModuleNamespace, mm: Arc<MemoryManager>) -> Value {
    let obj = GcRef::new(JsObject::new(Value::null(), mm));
    for (key, value) in namespace.entries() {
        let _ = obj.set(PropertyKey::string(&key), value);
    }
    Value::object(obj)
}

/// A loaded module
pub struct LoadedModule {
    /// Resolved URL/path of the module
    pub url: String,
    /// Compiled bytecode
    pub bytecode: Arc<Module>,
    /// Module type
    pub module_type: ModuleType,
    /// Module state
    pub state: ModuleState,
    /// Module namespace (exports)
    pub namespace: Arc<ModuleNamespace>,
    /// Import bindings (local name -> (module url, export name))
    pub import_bindings: HashMap<String, (String, String)>,
}

impl LoadedModule {
    /// Create a new loaded module from bytecode
    pub fn new(url: String, bytecode: Module) -> Self {
        let module_type = if bytecode.is_esm {
            ModuleType::ESM
        } else {
            ModuleType::CommonJS
        };

        Self {
            url,
            bytecode: Arc::new(bytecode),
            module_type,
            state: ModuleState::Unlinked,
            namespace: Arc::new(ModuleNamespace::new()),
            import_bindings: HashMap::new(),
        }
    }

    /// Create a pre-evaluated native module from a `GcRef<JsObject>` namespace.
    ///
    /// The module is immediately in `Evaluated` state — no bytecode execution needed.
    /// Used by v2 extensions that provide fully native module implementations.
    pub fn native(url: String, namespace_obj: GcRef<JsObject>) -> Self {
        let namespace = ModuleNamespace::new();
        // Copy all enumerable own properties from the namespace object into ModuleNamespace
        for key in namespace_obj.own_keys() {
            if let PropertyKey::String(s) = &key {
                if let Some(val) = namespace_obj.get(&key) {
                    namespace.set(s.as_str(), val);
                }
            }
        }

        // Also set "default" to the entire namespace object
        namespace.set("default", Value::object(namespace_obj));

        // Create a minimal empty bytecode module (never executed)
        let bytecode = Module::builder(&url).is_esm(true).build();

        Self {
            url,
            bytecode: Arc::new(bytecode),
            module_type: ModuleType::ESM,
            state: ModuleState::Evaluated,
            namespace: Arc::new(namespace),
            import_bindings: HashMap::new(),
        }
    }

    /// Get import records
    pub fn imports(&self) -> &[ImportRecord] {
        &self.bytecode.imports
    }

    /// Get an export by name
    pub fn get_export(&self, name: &str) -> Option<Value> {
        self.namespace.get(name)
    }

    /// Get export records
    pub fn exports(&self) -> &[ExportRecord] {
        &self.bytecode.exports
    }

    /// Check if this is a CommonJS module
    pub fn is_cjs(&self) -> bool {
        self.module_type == ModuleType::CommonJS
    }

    /// Check if this is an ES module
    pub fn is_esm(&self) -> bool {
        self.module_type == ModuleType::ESM
    }
}

/// CommonJS module wrapper
///
/// Provides the CommonJS globals for a module:
/// - `require(specifier)` - Load and return another module
/// - `module` - Object with `exports` property
/// - `exports` - Alias for `module.exports`
/// - `__dirname` - Directory of the current module
/// - `__filename` - Full path of the current module
#[derive(Debug, Clone)]
pub struct CjsWrapper {
    /// Module URL/path
    pub url: String,
    /// Directory name
    pub dirname: String,
    /// File name
    pub filename: String,
}

impl CjsWrapper {
    /// Create a new CommonJS wrapper for a module
    pub fn new(url: &str) -> Self {
        let normalized = url.strip_prefix("file://").unwrap_or(url);
        let path = Path::new(normalized);
        let dirname = path
            .parent()
            .map(|p| {
                let s = p.to_string_lossy().to_string();
                if s.is_empty() { ".".to_string() } else { s }
            })
            .unwrap_or_else(|| ".".to_string());
        let filename = normalized.to_string();

        Self {
            url: url.to_string(),
            dirname,
            filename,
        }
    }

    /// Get __dirname value
    pub fn dirname(&self) -> &str {
        &self.dirname
    }

    /// Get __filename value
    pub fn filename(&self) -> &str {
        &self.filename
    }
}

/// ESM-CJS interop helpers
pub mod interop {
    use super::*;

    /// Wrap CJS exports for ESM import
    ///
    /// When ESM imports CJS:
    /// - `default` is the current `module.exports` value
    /// - named exports are enumerable own properties of `module.exports`
    ///
    /// Fallback behavior for this foundation phase:
    /// - if `module.exports` is not present, we reuse `default` if present,
    ///   otherwise `undefined`.
    /// - existing namespace keys are copied as named exports when object-key
    ///   enumeration is not available.
    pub fn cjs_to_esm(namespace: &ModuleNamespace) -> ModuleNamespace {
        let result = ModuleNamespace::new();
        let module_exports = namespace
            .get("module.exports")
            .or_else(|| namespace.get("default"))
            .unwrap_or_else(Value::undefined);

        // ESM default binding always maps to module.exports for CJS modules.
        result.set("default", module_exports.clone());

        // Named exports come from enumerable own keys of module.exports.
        if let Some(exports_obj) = module_exports.as_object() {
            for key in exports_obj.own_keys() {
                let Some(desc) = exports_obj.get_own_property_descriptor(&key) else {
                    continue;
                };
                if !desc.enumerable() {
                    continue;
                }

                let name = match key {
                    PropertyKey::String(s) => s.as_str().to_string(),
                    PropertyKey::Index(i) => i.to_string(),
                    PropertyKey::Symbol(_) => continue,
                };

                if name == "default" {
                    continue;
                }

                if let Some(value) = exports_obj.get(&PropertyKey::string(&name)) {
                    result.set(&name, value);
                }
            }
        }

        // Fallback and compatibility: preserve existing named keys in namespace.
        for (key, value) in namespace.entries() {
            if key == "default" || key == "module.exports" {
                continue;
            }
            if !result.has(&key) {
                result.set(&key, value);
            }
        }

        result
    }

    /// Wrap ESM exports for CJS require
    ///
    /// When CJS requires ESM:
    /// - return value is a namespace-like object with named exports
    /// - `.default` is always present (explicit default export or `undefined`)
    ///
    /// Limitation for this foundation phase:
    /// - require() remains synchronous; async ESM loading/evaluation is not
    ///   supported in this path and must be handled by the async ESM loader.
    pub fn esm_to_cjs(namespace: &ModuleNamespace) -> ModuleNamespace {
        let result = ModuleNamespace::new();

        for (key, value) in namespace.entries() {
            result.set(&key, value);
        }

        if !result.has("default") {
            result.set("default", Value::undefined());
        }

        result
    }
}

/// Module loader
pub struct ModuleLoader {
    /// Loaded modules by resolved URL
    modules: RwLock<HashMap<String, Arc<RwLock<LoadedModule>>>>,
    /// Module providers for custom protocols (node:, otter:, etc.)
    providers: RwLock<Vec<Arc<dyn crate::module_provider::ModuleProvider>>>,
    /// Base directory for resolution
    base_dir: PathBuf,
    /// Resolver for ESM imports (uses import conditions)
    esm_resolver: Resolver,
    /// Resolver for CJS require() (uses require conditions)
    cjs_resolver: Resolver,
    /// Native module namespaces from v2 extensions (specifier -> namespace object).
    /// Checked first during load — instant, no compilation.
    native_modules: RwLock<HashMap<String, GcRef<JsObject>>>,
}

impl ModuleLoader {
    /// Create a new module loader
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        let base_dir = base_dir.into();

        // Resolver for ESM imports
        let esm_options = ResolveOptions {
            extensions: vec![
                ".js".to_string(),
                ".mjs".to_string(),
                ".cjs".to_string(),
                ".ts".to_string(),
                ".mts".to_string(),
                ".cts".to_string(),
                ".json".to_string(),
            ],
            main_fields: vec!["main".to_string(), "module".to_string()],
            condition_names: vec![
                "import".to_string(),
                "module".to_string(),
                "node".to_string(),
                "default".to_string(),
            ],
            ..Default::default()
        };
        // Resolver for CJS require()
        let cjs_options = ResolveOptions {
            extensions: esm_options.extensions.clone(),
            main_fields: esm_options.main_fields.clone(),
            condition_names: vec![
                "require".to_string(),
                "node".to_string(),
                "default".to_string(),
            ],
            ..Default::default()
        };

        Self {
            modules: RwLock::new(HashMap::new()),
            providers: RwLock::new(Vec::new()),
            base_dir,
            esm_resolver: Resolver::new(esm_options),
            cjs_resolver: Resolver::new(cjs_options),
            native_modules: RwLock::new(HashMap::new()),
        }
    }

    /// Compile source code directly as a module and cache it.
    pub fn compile_source(
        &self,
        source: &str,
        url: &str,
        eval_mode: bool,
    ) -> Result<Arc<otter_vm_bytecode::Module>, ModuleError> {
        let normalized_url = self.normalize_url_key(url);
        let is_esm = normalized_url.ends_with(".mjs") || normalized_url.ends_with(".mts");
        let compiler = Compiler::new();
        let bytecode = compiler
            .compile_ext(source, &normalized_url, eval_mode, is_esm, false)
            .map_err(|e| ModuleError::CompileError(e.to_string()))?;

        let bytecode_arc = Arc::new(bytecode.clone());
        let loaded = LoadedModule::new(normalized_url.clone(), bytecode);
        let module = Arc::new(RwLock::new(loaded));

        if let Ok(mut modules) = self.modules.write() {
            modules.insert(normalized_url, module);
        }

        Ok(bytecode_arc)
    }

    /// Update a module's namespace after execution.
    pub fn update_namespace(&self, url: &str, ctx: &otter_vm_core::context::VmContext) {
        if let Some(module) = self.get(url) {
            if let Ok(guard) = module.write() {
                let exports = guard.exports().to_vec();
                let global = ctx.global();
                let captured = ctx.captured_exports();

                for export_record in exports {
                    match export_record {
                        otter_vm_bytecode::module::ExportRecord::Named { local: _, exported } => {
                            // First check captured exports (for ESM)
                            if let Some(val) = captured.and_then(|c| c.get(&exported)) {
                                println!(
                                    "  Captured named export (from context): {} = {:?}",
                                    exported, val
                                );
                                guard.namespace.set(&exported, val.clone());
                            } else if let Some(val) = global.get(&exported.as_str().into()) {
                                println!(
                                    "  Captured named export (from global): {} = {:?}",
                                    exported, val
                                );
                                guard.namespace.set(&exported, val);
                            } else {
                                // Fallback: try to see if it's in the realm's global
                                if let Some(val) = ctx
                                    .realm_global(ctx.realm_id())
                                    .and_then(|g| g.get(&exported.as_str().into()))
                                {
                                    println!(
                                        "  Captured named export (from realm global): {} = {:?}",
                                        exported, val
                                    );
                                    guard.namespace.set(&exported, val);
                                } else {
                                    println!("  FAILED to capture named export: {}", exported);
                                }
                            }
                        }
                        otter_vm_bytecode::module::ExportRecord::Default { local: _ } => {
                            // First check captured exports (for ESM)
                            if let Some(val) = captured.and_then(|c| c.get("default")) {
                                guard.namespace.set("default", val.clone());
                            } else if let Some(val) = global.get(&"default".into()) {
                                guard.namespace.set("default", val);
                            } else if let Some(val) = ctx
                                .realm_global(ctx.realm_id())
                                .and_then(|g| g.get(&"default".into()))
                            {
                                guard.namespace.set("default", val);
                            } 
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Register a module provider for custom protocols.
    ///
    /// Providers are checked in order of registration during resolve/load.
    /// Use this to add support for `node:`, `otter:`, or custom URL schemes.
    pub fn register_provider(&self, provider: Arc<dyn crate::module_provider::ModuleProvider>) {
        if let Ok(mut providers) = self.providers.write() {
            providers.push(provider);
        }
    }

    /// Register a native module namespace from a v2 extension.
    ///
    /// Native modules are checked first during `load()` — they return a
    /// pre-built `GcRef<JsObject>` namespace instantly, with no JS compilation.
    pub fn register_native_module(&self, specifier: &str, namespace: GcRef<JsObject>) {
        if let Ok(mut native) = self.native_modules.write() {
            native.insert(specifier.to_string(), namespace);
        }
    }

    /// Get a native module namespace, if one is registered for this specifier.
    pub fn get_native_module(&self, specifier: &str) -> Option<GcRef<JsObject>> {
        self.native_modules
            .read()
            .ok()
            .and_then(|m| m.get(specifier).cloned())
    }

    /// Check if a specifier has a native module registered.
    pub fn has_native_module(&self, specifier: &str) -> bool {
        self.native_modules
            .read()
            .ok()
            .map(|m| m.contains_key(specifier))
            .unwrap_or(false)
    }

    /// Resolve a module specifier to an absolute path
    pub fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
    ) -> Result<ModuleResolution, ModuleError> {
        self.resolve_with_context(specifier, referrer, ImportContext::ESM)
    }

    /// Resolve a specifier in CommonJS require() context.
    pub fn resolve_require(
        &self,
        specifier: &str,
        referrer: &str,
    ) -> Result<ModuleResolution, ModuleError> {
        self.resolve_with_context(specifier, referrer, ImportContext::CJS)
    }

    /// Resolve a module specifier to an absolute path/URL with context-aware conditions.
    pub fn resolve_with_context(
        &self,
        specifier: &str,
        referrer: &str,
        context: ImportContext,
    ) -> Result<ModuleResolution, ModuleError> {
        // 0. Handle explicit URL-like namespaces first.
        if specifier.starts_with("http://") || specifier.starts_with("https://") {
            return Ok(ModuleResolution {
                url: specifier.to_string(),
                module_type: ModuleType::ESM,
            });
        }
        if specifier.starts_with("file://") {
            let normalized = self.normalize_url_key(specifier);
            let module_type = if normalized.ends_with(".mjs") || normalized.ends_with(".mts") {
                ModuleType::ESM
            } else {
                ModuleType::CommonJS
            };
            return Ok(ModuleResolution {
                url: format!("file://{}", normalized),
                module_type,
            });
        }

        // 1. Check registered providers first (node:, otter:, etc.)
        if let Ok(providers) = self.providers.read() {
            for provider in providers.iter() {
                if let Some(resolution) = provider.resolve(specifier, referrer) {
                    return Ok(resolution);
                }
            }
        }

        // 2. npm namespace support (initial Bun-style path): `npm:pkg` -> resolve as `pkg`
        // Version tags are normalized (e.g. `npm:lodash@4` -> `lodash`).
        if let Some(raw) = specifier.strip_prefix("npm:") {
            let normalized = normalize_npm_specifier(raw).ok_or_else(|| {
                ModuleError::ResolveError(format!("Invalid npm specifier '{}'", specifier))
            })?;
            return self.resolve_with_context(&normalized, referrer, context);
        }

        // 3. otter namespace without provider remains canonical and load-time handled.
        if specifier.starts_with("otter:") {
            return Ok(ModuleResolution {
                url: specifier.to_string(),
                module_type: ModuleType::ESM,
            });
        }

        // 4. Handle absolute paths
        if specifier.starts_with('/') {
            let normalized = self.normalize_url_key(specifier);
            return Ok(ModuleResolution {
                url: normalized.clone(),
                module_type: if normalized.ends_with(".mjs") || normalized.ends_with(".mts") {
                    ModuleType::ESM
                } else {
                    ModuleType::CommonJS
                },
            });
        }

        // 5. Get the directory of the referrer
        let referrer_path = Path::new(referrer);
        let referrer_dir = referrer_path.parent().unwrap_or(&self.base_dir);

        // 6. Use context-aware oxc resolver for filesystem modules
        let resolver = match context {
            ImportContext::ESM => &self.esm_resolver,
            ImportContext::CJS => &self.cjs_resolver,
        };

        match resolver.resolve(referrer_dir, specifier) {
            Ok(resolution) => {
                let normalized = self.normalize_url_key(&resolution.path().to_string_lossy());
                let module_type = if normalized.ends_with(".mjs") || normalized.ends_with(".mts") {
                    ModuleType::ESM
                } else {
                    ModuleType::CommonJS
                };
                Ok(ModuleResolution {
                    url: normalized,
                    module_type,
                })
            }
            Err(e) => Err(ModuleError::ResolveError(format!(
                "Cannot resolve '{}' from '{}': {}",
                specifier, referrer, e
            ))),
        }
    }

    /// Load a module from a file
    pub fn load(
        &self,
        url: &str,
        module_type: ModuleType,
    ) -> Result<Arc<RwLock<LoadedModule>>, ModuleError> {
        let normalized_url = self.normalize_url_key(url);

        // Check if already loaded
        if let Some(module) = self
            .modules
            .read()
            .ok()
            .and_then(|m| m.get(&normalized_url).cloned())
        {
            return Ok(module);
        }

        // 0. Check v2 native modules first — instant, no JS compilation
        if let Some(namespace_obj) = self.get_native_module(&normalized_url) {
            let loaded = LoadedModule::native(normalized_url.clone(), namespace_obj);
            let module = Arc::new(RwLock::new(loaded));
            if let Ok(mut modules) = self.modules.write() {
                modules.insert(normalized_url.clone(), Arc::clone(&module));
            }
            return Ok(module);
        }

        // 1. Try to load from providers (handles builtin://, custom protocols)
        if let Ok(providers) = self.providers.read() {
            for provider in providers.iter() {
                if let Some(source) = provider.load(&normalized_url) {
                    // Compile the source from provider
                    let compile_source = if module_type == ModuleType::CommonJS {
                        self.wrap_commonjs_source(&normalized_url, &source.code)?
                    } else {
                        source.code
                    };
                    let compiler = Compiler::new();
                    let bytecode = compiler
                        .compile(
                            &compile_source,
                            &normalized_url,
                            module_type == ModuleType::ESM,
                        )
                        .map_err(|e| ModuleError::CompileError(e.to_string()))?;

                    // Create loaded module
                    let loaded = LoadedModule::new(normalized_url.clone(), bytecode);
                    let module = Arc::new(RwLock::new(loaded));

                    // Store in cache
                    if let Ok(mut modules) = self.modules.write() {
                        modules.insert(normalized_url.clone(), Arc::clone(&module));
                    }

                    return Ok(module);
                }
            }
        }

        // 2. URL namespaces that need dedicated providers/fetchers
        if normalized_url.starts_with("http://") || normalized_url.starts_with("https://") {
            return Err(ModuleError::IoError(format!(
                "Remote module loading is not configured for '{}'. Register an https module provider.",
                normalized_url
            )));
        }
        if normalized_url.starts_with("npm:") {
            return Err(ModuleError::IoError(format!(
                "npm namespace is not configured for '{}'. Register an npm module provider.",
                normalized_url
            )));
        }
        if normalized_url.starts_with("otter:") {
            return Err(ModuleError::NotFound(format!(
                "No provider registered for Otter namespace module '{}'",
                normalized_url
            )));
        }

        // 3. Read from filesystem
        let source = std::fs::read_to_string(&normalized_url)
            .map_err(|e| ModuleError::IoError(e.to_string()))?;

        // Compile
        let compile_source = if module_type == ModuleType::CommonJS {
            self.wrap_commonjs_source(&normalized_url, &source)?
        } else {
            source
        };
        let compiler = Compiler::new();
        let bytecode = compiler
            .compile(
                &compile_source,
                &normalized_url,
                module_type == ModuleType::ESM,
            )
            .map_err(|e| ModuleError::CompileError(e.to_string()))?;

        // Create loaded module
        let loaded = LoadedModule::new(normalized_url.clone(), bytecode);
        let module = Arc::new(RwLock::new(loaded));

        // Store in cache
        if let Ok(mut modules) = self.modules.write() {
            modules.insert(normalized_url, Arc::clone(&module));
        }

        Ok(module)
    }

    /// Build the module dependency graph and return modules in topological order
    pub fn build_graph(&self, entry: &str) -> Result<Vec<String>, ModuleError> {
        let entry = self.normalize_url_key(entry);
        let mut order = Vec::new();
        let mut visited = HashMap::new();

        self.visit_module(&entry, &mut visited, &mut order)?;

        Ok(order)
    }

    /// DFS visit for topological sort
    fn visit_module(
        &self,
        url: &str,
        visited: &mut HashMap<String, bool>,
        order: &mut Vec<String>,
    ) -> Result<(), ModuleError> {
        let url = self.normalize_url_key(url);

        // Check if already visited
        if let Some(&in_progress) = visited.get(&url) {
            if in_progress {
                // Circular dependency - this is allowed in ESM but needs special handling
                return Ok(());
            }
            // Already fully visited
            return Ok(());
        }

        // Mark as in progress
        visited.insert(url.clone(), true);

        // Load the module
        // All modules should be loaded/compiled by now via link()
        let module = self
            .get(&url)
            .ok_or_else(|| ModuleError::NotFound(url.clone()))?;
        let (imports, module_context) = {
            let m = module
                .read()
                .map_err(|_| ModuleError::NotFound(url.clone()))?;
            let context = if m.module_type == ModuleType::CommonJS {
                ImportContext::CJS
            } else {
                ImportContext::ESM
            };
            (m.imports().to_vec(), context)
        };

        // Visit dependencies
        for import in imports {
            let resolution = self.resolve_with_context(&import.specifier, &url, module_context)?;
            self.visit_module(&resolution.url, visited, order)?;
        }

        // Mark as complete
        visited.insert(url.clone(), false);

        // Add to order
        order.push(url);

        Ok(())
    }

    /// Link a module (resolve all imports)
    pub fn link(&self, url: &str) -> Result<(), ModuleError> {
        let url = self.normalize_url_key(url);
        let module = self
            .modules
            .read()
            .ok()
            .and_then(|m| m.get(&url).cloned())
            .ok_or_else(|| ModuleError::NotFound(url.clone()))?;

        let mut module_guard = module
            .write()
            .map_err(|_| ModuleError::NotFound(url.clone()))?;

        if module_guard.state != ModuleState::Unlinked {
            return Ok(());
        }

        module_guard.state = ModuleState::Linking;

        // Process imports
        let imports = module_guard.imports().to_vec();
        let module_context = if module_guard.module_type == ModuleType::CommonJS {
            ImportContext::CJS
        } else {
            ImportContext::ESM
        };

        for import in imports {
            let resolution = self.resolve_with_context(&import.specifier, &url, module_context)?;
            let resolved = resolution.url;

            // Ensure dependency is loaded
            self.load(&resolved, resolution.module_type)?;

            // Build import bindings
            for binding in &import.bindings {
                match binding {
                    ImportBinding::Named { imported, local } => {
                        module_guard
                            .import_bindings
                            .insert(local.clone(), (resolved.clone(), imported.clone()));
                    }
                    ImportBinding::Default { local } => {
                        module_guard
                            .import_bindings
                            .insert(local.clone(), (resolved.clone(), "default".to_string()));
                    }
                    ImportBinding::Namespace { local } => {
                        // Namespace imports get the whole module namespace
                        module_guard
                            .import_bindings
                            .insert(local.clone(), (resolved.clone(), "*".to_string()));
                    }
                }
            }
        }

        module_guard.state = ModuleState::Linked;
        Ok(())
    }

    /// Get a loaded module
    pub fn get(&self, url: &str) -> Option<Arc<RwLock<LoadedModule>>> {
        let normalized = self.normalize_url_key(url);
        self.modules.read().ok()?.get(&normalized).cloned()
    }

    /// Get an import value for a module
    pub fn get_import_value(
        &self,
        module_url: &str,
        local_name: &str,
    ) -> Result<Value, ModuleError> {
        let module = self
            .get(module_url)
            .ok_or_else(|| ModuleError::NotFound(module_url.to_string()))?;

        let module_guard = module
            .read()
            .map_err(|_| ModuleError::NotFound(module_url.to_string()))?;

        // Look up the binding
        let (source_url, export_name) = module_guard
            .import_bindings
            .get(local_name)
            .ok_or_else(|| ModuleError::ExportNotFound {
                module: module_url.to_string(),
                export: local_name.to_string(),
            })?
            .clone();

        drop(module_guard);

        // Get the source module's namespace
        let source_module = self
            .get(&source_url)
            .ok_or_else(|| ModuleError::NotFound(source_url.clone()))?;

        let source_guard = source_module
            .read()
            .map_err(|_| ModuleError::NotFound(source_url.clone()))?;

        if export_name == "*" {
            // Return the whole namespace
            Ok(source_guard.namespace.to_value())
        } else {
            // Return specific export
            source_guard
                .namespace
                .get(&export_name)
                .ok_or_else(|| ModuleError::ExportNotFound {
                    module: source_url.clone(),
                    export: export_name.clone(),
                })
        }
    }

    /// Detect module type from file extension and content
    pub fn detect_module_type(url: &str, _source: &str) -> ModuleType {
        // Check extension first
        if url.ends_with(".mjs") || url.ends_with(".mts") {
            return ModuleType::ESM;
        }
        if url.ends_with(".cjs") || url.ends_with(".cts") {
            return ModuleType::CommonJS;
        }

        // Default to ESM for .js/.ts files
        // In a full implementation we'd check package.json "type" field
        ModuleType::ESM
    }

    /// Get base directory
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Get a CommonJS wrapper for a module
    pub fn get_cjs_wrapper(&self, url: &str) -> CjsWrapper {
        CjsWrapper::new(url)
    }

    /// Wrap CommonJS source into a function scope with Node-compatible globals.
    ///
    /// Contract:
    /// - `require`, `module`, `exports`, `__filename`, `__dirname` are provided.
    /// - module cache identity stays anchored to resolved URL (same `module.exports` object).
    /// - `__module_commit` publishes final/partial exports to loader namespace.
    fn wrap_commonjs_source(&self, url: &str, source: &str) -> Result<String, ModuleError> {
        let wrapper = self.get_cjs_wrapper(url);
        let url_lit = serde_json::to_string(url).map_err(|e| {
            ModuleError::CompileError(format!("CJS wrapper url encode failed: {}", e))
        })?;
        let filename_lit = serde_json::to_string(wrapper.filename()).map_err(|e| {
            ModuleError::CompileError(format!("CJS wrapper filename encode failed: {}", e))
        })?;
        let dirname_lit = serde_json::to_string(wrapper.dirname()).map_err(|e| {
            ModuleError::CompileError(format!("CJS wrapper dirname encode failed: {}", e))
        })?;

        Ok(format!(
            r#"
const __otter_cjs_referrer = {url_lit};
const __otter_cjs_require =
    (typeof globalThis.__createRequire === "function")
        ? globalThis.__createRequire(__otter_cjs_referrer)
        : function() {{
            throw new Error("CommonJS require() is unavailable (module extension not registered)");
        }};
const __otter_cjs_module = {{ exports: {{}} }};
const __otter_cjs_exports = __otter_cjs_module.exports;
try {{
    (function(exports, require, module, __filename, __dirname) {{
{source}
    }})(__otter_cjs_exports, __otter_cjs_require, __otter_cjs_module, {filename_lit}, {dirname_lit});
}} finally {{
    if (typeof globalThis.__module_commit === "function") {{
        globalThis.__module_commit(__otter_cjs_referrer, __otter_cjs_module, __otter_cjs_exports);
    }}
}}
"#
        ))
    }

    /// Commit CommonJS `module.exports` value into shared module namespace.
    ///
    /// This powers:
    /// - ESM -> CJS interop (`default` and named exports from enumerable keys)
    /// - CJS cache identity (`require()` returns the same module.exports object)
    pub fn commit_cjs_exports(&self, url: &str, module_exports: Value) -> Result<(), ModuleError> {
        let normalized = self.normalize_url_key(url);
        let module = self
            .get(&normalized)
            .ok_or_else(|| ModuleError::NotFound(normalized.clone()))?;
        let mut guard = module
            .write()
            .map_err(|_| ModuleError::NotFound(normalized.clone()))?;

        let tmp = ModuleNamespace::new();
        tmp.set("module.exports", module_exports.clone());
        let esm_view = interop::cjs_to_esm(&tmp);

        guard.namespace.clear();
        guard.namespace.set("module.exports", module_exports);
        for (name, value) in esm_view.entries() {
            guard.namespace.set(&name, value);
        }

        if guard.state != ModuleState::Error {
            guard.state = ModuleState::Evaluated;
        }

        Ok(())
    }

    /// Resolve and load a module for CommonJS `require()` and return runtime value.
    ///
    /// Contract:
    /// - `require(cjs)` returns `module.exports` directly.
    /// - `require(esm)` returns namespace object with `.default` + named exports.
    ///
    /// Limitation:
    /// - this path is synchronous; if target ESM has not been evaluated yet, exports can be
    ///   partially initialized (`default` may be `undefined`) until ESM evaluation finishes.
    pub fn require_value(
        &self,
        specifier: &str,
        referrer: &str,
        mm: Arc<MemoryManager>,
    ) -> Result<Value, ModuleError> {
        let module = self.require(specifier, referrer)?;
        let guard = module
            .read()
            .map_err(|_| ModuleError::NotFound(specifier.to_string()))?;

        if guard.is_esm() {
            let cjs_view = interop::esm_to_cjs(&guard.namespace);
            return Ok(namespace_to_object(&cjs_view, mm));
        }

        if let Some(value) = guard
            .namespace
            .get("module.exports")
            .or_else(|| guard.namespace.get("default"))
        {
            return Ok(value);
        }

        Ok(namespace_to_object(&guard.namespace, mm))
    }

    /// Load a module as CommonJS (with wrapper)
    ///
    /// This is used when require() is called from CJS code
    pub fn require(
        &self,
        specifier: &str,
        referrer: &str,
    ) -> Result<Arc<RwLock<LoadedModule>>, ModuleError> {
        // Resolve the specifier
        let resolution = self.resolve_require(specifier, referrer)?;

        // Load the module
        let module = self.load(&resolution.url, resolution.module_type)?;

        // Build graph and link
        let order = self.build_graph(&resolution.url)?;
        for module_url in &order {
            self.link(module_url)?;
        }

        Ok(module)
    }

    /// Normalize URL/cache key so ESM/CJS graphs share one module instance.
    /// Get the normalized URL key for a given URL.
    pub fn normalize_url(&self, url: &str) -> String {
        self.normalize_url_key(url)
    }

    fn normalize_url_key(&self, url: &str) -> String {
        if url.starts_with("builtin://")
            || url.starts_with("otter:")
            || url.starts_with("http://")
            || url.starts_with("https://")
            || url.starts_with("npm:")
        {
            return url.to_string();
        }

        if url.starts_with('<') && url.ends_with('>') {
            return url.to_string();
        }

        let raw = url.strip_prefix("file://").unwrap_or(url);
        let path = Path::new(raw);
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.base_dir.join(path)
        };
        let canonical = std::fs::canonicalize(&absolute).unwrap_or(absolute);
        canonical.to_string_lossy().to_string()
    }
}

impl Default for ModuleLoader {
    fn default() -> Self {
        Self::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }
}

/// Normalize `npm:` specifier payload to a bare package specifier usable by resolver.
///
/// Examples:
/// - `lodash` -> `lodash`
/// - `lodash@4` -> `lodash`
/// - `lodash@4/sub/path` -> `lodash/sub/path`
/// - `@scope/pkg@1.2.3/sub` -> `@scope/pkg/sub`
fn normalize_npm_specifier(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }

    if raw.starts_with('@') {
        // Scoped package: @scope/pkg[/subpath] with optional @version on pkg segment.
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

    // Unscoped package: pkg[/subpath] with optional @version.
    let slash = raw.find('/').unwrap_or(raw.len());
    let pkg_and_ver = &raw[..slash];
    let rest = &raw[slash..];
    let pkg = pkg_and_ver.split('@').next().unwrap_or_default();
    if pkg.is_empty() {
        return None;
    }
    Some(format!("{}{}", pkg, rest))
}

/// Create the module extension for dynamic imports and CommonJS require
///
/// This extension provides ops for:
/// - `__module_resolve`: Resolve a module specifier to absolute path
/// - `__module_load`: Load and compile a module (async, for ESM dynamic import)
/// - `__module_require`: Synchronous require for CommonJS
/// - `__module_commit`: Commit CommonJS `module.exports` into shared namespace cache
/// - `__module_dirname`: Get __dirname for a module
/// - `__module_filename`: Get __filename for a module
pub fn module_extension(loader: Arc<ModuleLoader>) -> crate::Extension {
    use crate::extension::{op_async, op_native_with_mm, op_sync};
    use otter_vm_core::error::VmError;
    use serde_json::json;

    let loader_resolve = Arc::clone(&loader);
    let loader_load = Arc::clone(&loader);
    let loader_require = Arc::clone(&loader);
    let loader_commit = Arc::clone(&loader);
    let loader_dirname = Arc::clone(&loader);
    let loader_filename = Arc::clone(&loader);

    crate::Extension::new("module")
        .with_ops(vec![
            // Resolve a module specifier
            op_sync("__module_resolve", move |args| {
                let specifier = args
                    .first()
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "Missing specifier argument".to_string())?;
                let referrer = args
                    .get(1)
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "Missing referrer argument".to_string())?;
                let context = match args.get(2).and_then(|v| v.as_str()) {
                    Some("cjs" | "require") => ImportContext::CJS,
                    _ => ImportContext::ESM,
                };

                match loader_resolve.resolve_with_context(specifier, referrer, context) {
                    Ok(resolved) => Ok(json!(resolved)),
                    Err(e) => Err(e.to_string()),
                }
            }),
            // Load a module asynchronously (for dynamic import())
            op_async("__module_load", move |args| {
                let loader = Arc::clone(&loader_load);
                let url = args
                    .first()
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                async move {
                    // Build graph to get all dependencies
                    let order = loader.build_graph(&url).map_err(|e| e.to_string())?;

                    // Link all modules
                    for module_url in &order {
                        loader.link(module_url).map_err(|e| e.to_string())?;
                    }

                    Ok(json!({
                        "url": url,
                        "dependencies": order,
                    }))
                }
            }),
            // Synchronous require for CommonJS
            op_native_with_mm("__module_require", move |args, mm| {
                let specifier = args
                    .first()
                    .and_then(|v| v.as_string())
                    .map(|s| s.as_str().to_string())
                    .ok_or_else(|| VmError::type_error("Missing specifier argument"))?;
                let referrer = args
                    .get(1)
                    .and_then(|v| v.as_string())
                    .map(|s| s.as_str().to_string())
                    .ok_or_else(|| VmError::type_error("Missing referrer argument"))?;

                loader_require
                    .require_value(&specifier, &referrer, mm)
                    .map_err(|e| VmError::type_error(e.to_string()))
            }),
            // Commit CommonJS `module.exports` to shared loader cache namespace.
            op_native_with_mm("__module_commit", move |args, _mm| {
                let url = args
                    .first()
                    .and_then(|v| v.as_string())
                    .map(|s| s.as_str().to_string())
                    .ok_or_else(|| VmError::type_error("Missing url argument"))?;

                let module_exports =
                    if let Some(module_obj) = args.get(1).and_then(|v| v.as_object()) {
                        module_obj
                            .get(&PropertyKey::string("exports"))
                            .or_else(|| args.get(2).cloned())
                            .unwrap_or_else(Value::undefined)
                    } else {
                        args.get(2).cloned().unwrap_or_else(Value::undefined)
                    };

                loader_commit
                    .commit_cjs_exports(&url, module_exports)
                    .map_err(|e| VmError::type_error(e.to_string()))?;

                Ok(Value::undefined())
            }),
            // Get __dirname for a module
            op_sync("__module_dirname", move |args| {
                let url = args
                    .first()
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "Missing url argument".to_string())?;

                let wrapper = loader_dirname.get_cjs_wrapper(url);
                Ok(json!(wrapper.dirname()))
            }),
            // Get __filename for a module
            op_sync("__module_filename", move |args| {
                let url = args
                    .first()
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "Missing url argument".to_string())?;

                let wrapper = loader_filename.get_cjs_wrapper(url);
                Ok(json!(wrapper.filename()))
            }),
        ])
        .with_js(
            r#"
// Dynamic import helper
globalThis.__dynamicImport = async function(specifier, referrer) {
    const resolved = __module_resolve(specifier, referrer, "esm");
    const result = await __module_load(resolved.url);
    return result;
};

// CommonJS require (synchronous)
// Note: In real usage, require is created per-module with correct referrer
globalThis.__createRequire = function(referrer) {
    function require(specifier) {
        return __module_require(specifier, referrer);
    }

    require.resolve = function(specifier) {
        const resolved = __module_resolve(specifier, referrer, "cjs");
        return resolved.url;
    };

    require.cache = {};
    require.filename = __module_filename(referrer);
    require.dirname = __module_dirname(referrer);

    return require;
};

// Get __dirname for a module
globalThis.__getDirname = function(url) {
    return __module_dirname(url);
};

// Get __filename for a module
globalThis.__getFilename = function(url) {
    return __module_filename(url);
};
"#,
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn test_module_state() {
        assert_eq!(ModuleState::Unlinked, ModuleState::Unlinked);
        assert_ne!(ModuleState::Unlinked, ModuleState::Linked);
    }

    #[test]
    fn test_module_namespace() {
        let ns = ModuleNamespace::new();
        ns.set("foo", Value::int32(42));

        assert!(ns.has("foo"));
        assert!(!ns.has("bar"));
        assert_eq!(ns.get("foo"), Some(Value::int32(42)));
    }

    #[test]
    fn test_detect_module_type() {
        assert_eq!(
            ModuleLoader::detect_module_type("foo.mjs", ""),
            ModuleType::ESM
        );
        assert_eq!(
            ModuleLoader::detect_module_type("foo.cjs", ""),
            ModuleType::CommonJS
        );
        assert_eq!(
            ModuleLoader::detect_module_type("foo.js", ""),
            ModuleType::ESM
        );
    }

    #[test]
    fn test_module_loader_basic() {
        let dir = tempdir().unwrap();
        let loader = ModuleLoader::new(dir.path());

        // Create a simple module file
        let module_path = dir.path().join("test.js");
        let mut file = std::fs::File::create(&module_path).unwrap();
        writeln!(file, "export const x = 42;").unwrap();

        let url = module_path.to_string_lossy().to_string();
        let module = loader.load(&url, ModuleType::ESM).unwrap();

        let module_guard = module.read().unwrap();
        assert_eq!(module_guard.state, ModuleState::Unlinked);
        assert_eq!(module_guard.module_type, ModuleType::ESM);
    }

    #[test]
    fn test_module_resolution() {
        let dir = tempdir().unwrap();
        let loader = ModuleLoader::new(dir.path());

        // Create main.js
        let main_path = dir.path().join("main.js");
        let mut file = std::fs::File::create(&main_path).unwrap();
        writeln!(file, "import {{ foo }} from './util.js';").unwrap();

        // Create util.js
        let util_path = dir.path().join("util.js");
        let mut file = std::fs::File::create(&util_path).unwrap();
        writeln!(file, "export const foo = 1;").unwrap();

        let main_url = main_path.to_string_lossy().to_string();
        let resolution = loader.resolve("./util.js", &main_url).unwrap();

        assert!(resolution.url.ends_with("util.js"));
    }

    #[test]
    fn test_module_resolution_context_aware_conditions() {
        let dir = tempdir().unwrap();
        let loader = ModuleLoader::new(dir.path());

        // Create a package with conditional exports for import/require.
        let pkg_dir = dir.path().join("node_modules").join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{
  "name": "pkg",
  "exports": {
    ".": {
      "import": "./esm.mjs",
      "require": "./cjs.cjs"
    }
  }
}"#,
        )
        .unwrap();
        std::fs::write(pkg_dir.join("esm.mjs"), "export default 1;").unwrap();
        std::fs::write(pkg_dir.join("cjs.cjs"), "module.exports = 1;").unwrap();

        let main_path = dir.path().join("main.mjs");
        std::fs::write(&main_path, "import x from 'pkg';").unwrap();
        let referrer = main_path.to_string_lossy().to_string();

        let esm = loader
            .resolve_with_context("pkg", &referrer, ImportContext::ESM)
            .unwrap();
        assert!(esm.url.ends_with("esm.mjs"));
        assert_eq!(esm.module_type, ModuleType::ESM);

        let cjs = loader
            .resolve_with_context("pkg", &referrer, ImportContext::CJS)
            .unwrap();
        assert!(cjs.url.ends_with("cjs.cjs"));
        assert_eq!(cjs.module_type, ModuleType::CommonJS);
    }

    #[test]
    fn test_module_resolution_namespace_passthrough() {
        let dir = tempdir().unwrap();
        let loader = ModuleLoader::new(dir.path());

        let https = loader
            .resolve("https://esm.sh/lodash", "/tmp/main.mjs")
            .unwrap();
        assert_eq!(https.url, "https://esm.sh/lodash");

        let file = loader
            .resolve("file:///tmp/example.mjs", "/tmp/main.mjs")
            .unwrap();
        assert_eq!(file.url, "file:///tmp/example.mjs");
    }

    #[test]
    fn test_module_resolution_npm_namespace_to_node_modules() {
        let dir = tempdir().unwrap();
        let loader = ModuleLoader::new(dir.path());

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

        let main_path = dir.path().join("main.mjs");
        std::fs::write(&main_path, "import _ from 'npm:lodash';").unwrap();
        let referrer = main_path.to_string_lossy().to_string();

        let npm = loader.resolve("npm:lodash@4", &referrer).unwrap();
        assert!(npm.url.ends_with("node_modules/lodash/index.js"));
    }

    #[test]
    fn test_normalize_npm_specifier() {
        assert_eq!(normalize_npm_specifier("lodash").as_deref(), Some("lodash"));
        assert_eq!(
            normalize_npm_specifier("lodash@4").as_deref(),
            Some("lodash")
        );
        assert_eq!(
            normalize_npm_specifier("lodash@4/fp").as_deref(),
            Some("lodash/fp")
        );
        assert_eq!(
            normalize_npm_specifier("@scope/pkg").as_deref(),
            Some("@scope/pkg")
        );
        assert_eq!(
            normalize_npm_specifier("@scope/pkg@1.2.3/sub").as_deref(),
            Some("@scope/pkg/sub")
        );
        assert!(normalize_npm_specifier("").is_none());
    }

    #[test]
    fn test_build_graph() {
        let dir = tempdir().unwrap();
        // Canonicalize to handle macOS /var -> /private/var symlinks
        let canon_dir = dir.path().canonicalize().unwrap();
        let loader = ModuleLoader::new(&canon_dir);

        // Create a.js -> imports b.js
        let a_path = canon_dir.join("a.js");
        let mut file = std::fs::File::create(&a_path).unwrap();
        writeln!(file, "import {{ x }} from './b.js';\nexport const y = x;").unwrap();

        // Create b.js
        let b_path = canon_dir.join("b.js");
        let mut file = std::fs::File::create(&b_path).unwrap();
        writeln!(file, "export const x = 42;").unwrap();

        let a_url = a_path.to_string_lossy().to_string();
        let b_url = b_path.to_string_lossy().to_string();

        // Load modules into the loader first (build_graph expects them to be loaded)
        loader.load(&b_url, ModuleType::ESM).unwrap();
        loader.load(&a_url, ModuleType::ESM).unwrap();

        let order = loader.build_graph(&a_url).unwrap();

        // b.js should come before a.js in topological order
        assert_eq!(order.len(), 2);
        assert!(order[0].ends_with("b.js"));
        assert!(order[1].ends_with("a.js"));
    }

    #[test]
    fn test_cjs_wrapper() {
        let wrapper = CjsWrapper::new("/path/to/module.js");

        assert_eq!(wrapper.filename(), "/path/to/module.js");
        assert_eq!(wrapper.dirname(), "/path/to");
    }

    #[test]
    fn test_cjs_wrapper_current_dir() {
        let wrapper = CjsWrapper::new("module.js");

        assert_eq!(wrapper.filename(), "module.js");
        assert_eq!(wrapper.dirname(), ".");
    }

    #[test]
    fn test_cjs_wrapper_file_url() {
        let wrapper = CjsWrapper::new("file:///tmp/example/module.js");
        assert!(wrapper.filename().ends_with("/tmp/example/module.js"));
        assert!(wrapper.dirname().ends_with("/tmp/example"));
    }

    #[test]
    fn test_module_require() {
        let dir = tempdir().unwrap();
        let loader = ModuleLoader::new(dir.path());

        // Create main.js (CJS)
        let main_path = dir.path().join("main.cjs");
        let mut file = std::fs::File::create(&main_path).unwrap();
        writeln!(file, "const x = 42;").unwrap();

        // Create util.cjs
        let util_path = dir.path().join("util.cjs");
        let mut file = std::fs::File::create(&util_path).unwrap();
        writeln!(file, "const y = 1;").unwrap();

        let main_url = main_path.to_string_lossy().to_string();
        let result = loader.require("./util.cjs", &main_url);

        assert!(result.is_ok());
        let module = result.unwrap();
        let guard = module.read().unwrap();
        assert!(guard.url.ends_with("util.cjs"));
    }

    #[test]
    fn test_wrap_commonjs_source_contains_runtime_contract() {
        let loader = ModuleLoader::new("/tmp");
        let wrapped = loader
            .wrap_commonjs_source("/tmp/example.cjs", "module.exports = { x: 1 };")
            .unwrap();
        assert!(wrapped.contains("__createRequire"));
        assert!(wrapped.contains("__module_commit"));
        assert!(wrapped.contains("(exports, require, module, __filename, __dirname)"));
    }

    #[test]
    fn test_commit_cjs_exports_populates_esm_view() {
        let dir = tempdir().unwrap();
        let loader = ModuleLoader::new(dir.path());

        let module_path = dir.path().join("mod.cjs");
        std::fs::write(&module_path, "module.exports = {};").unwrap();
        let module_url = module_path.to_string_lossy().to_string();

        loader.load(&module_url, ModuleType::CommonJS).unwrap();

        let mm = Arc::new(otter_vm_core::memory::MemoryManager::test());
        let exports_obj = GcRef::new(JsObject::new(Value::null(), mm));
        exports_obj
            .set(PropertyKey::string("named"), Value::int32(7))
            .unwrap();

        loader
            .commit_cjs_exports(&module_url, Value::object(exports_obj))
            .unwrap();

        let module = loader.get(&module_url).unwrap();
        let guard = module.read().unwrap();
        assert_eq!(guard.namespace.get("named"), Some(Value::int32(7)));
        assert!(guard.namespace.get("default").is_some());
        assert!(guard.namespace.get("module.exports").is_some());
    }

    #[test]
    fn test_require_value_returns_cjs_module_exports_identity() {
        let dir = tempdir().unwrap();
        let loader = ModuleLoader::new(dir.path());

        let main_path = dir.path().join("main.cjs");
        std::fs::write(&main_path, "require('./shared.cjs');").unwrap();
        let shared_path = dir.path().join("shared.cjs");
        std::fs::write(&shared_path, "module.exports = {};").unwrap();
        let main_url = main_path.to_string_lossy().to_string();
        let shared_url = shared_path.to_string_lossy().to_string();

        loader.load(&shared_url, ModuleType::CommonJS).unwrap();

        let mm = Arc::new(otter_vm_core::memory::MemoryManager::test());
        let exports_obj = GcRef::new(JsObject::new(Value::null(), Arc::clone(&mm)));
        exports_obj
            .set(PropertyKey::string("value"), Value::int32(1))
            .unwrap();
        loader
            .commit_cjs_exports(&shared_url, Value::object(exports_obj))
            .unwrap();

        let value = loader
            .require_value("./shared.cjs", &main_url, Arc::clone(&mm))
            .unwrap();
        let required_obj = value.as_object().unwrap();
        required_obj
            .set(PropertyKey::string("value"), Value::int32(2))
            .unwrap();

        let module = loader.get(&shared_url).unwrap();
        let guard = module.read().unwrap();
        let cached_obj = guard
            .namespace
            .get("module.exports")
            .and_then(|v| v.as_object())
            .unwrap();
        assert_eq!(
            cached_obj.get(&PropertyKey::string("value")),
            Some(Value::int32(2))
        );
    }

    #[test]
    fn test_require_value_returns_esm_namespace_object() {
        let dir = tempdir().unwrap();
        let loader = ModuleLoader::new(dir.path());

        let main_path = dir.path().join("main.cjs");
        std::fs::write(&main_path, "require('./lib.mjs');").unwrap();
        let lib_path = dir.path().join("lib.mjs");
        std::fs::write(&lib_path, "export const named = 5; export default 9;").unwrap();
        let main_url = main_path.to_string_lossy().to_string();
        let lib_url = lib_path.to_string_lossy().to_string();

        let module = loader.load(&lib_url, ModuleType::ESM).unwrap();
        {
            let guard = module.read().unwrap();
            guard.namespace.set("default", Value::int32(9));
            guard.namespace.set("named", Value::int32(5));
        }

        let mm = Arc::new(otter_vm_core::memory::MemoryManager::test());
        let value = loader.require_value("./lib.mjs", &main_url, mm).unwrap();
        let ns_obj = value.as_object().unwrap();

        assert_eq!(
            ns_obj.get(&PropertyKey::string("default")),
            Some(Value::int32(9))
        );
        assert_eq!(
            ns_obj.get(&PropertyKey::string("named")),
            Some(Value::int32(5))
        );
    }

    #[test]
    fn test_interop_cjs_to_esm() {
        let cjs_ns = ModuleNamespace::new();
        cjs_ns.set("foo", Value::int32(1));
        cjs_ns.set("bar", Value::int32(2));

        let esm_ns = interop::cjs_to_esm(&cjs_ns);

        assert!(esm_ns.has("foo"));
        assert!(esm_ns.has("bar"));
        assert_eq!(esm_ns.get("foo"), Some(Value::int32(1)));
    }

    #[test]
    fn test_interop_cjs_to_esm_default_from_module_exports() {
        let cjs_ns = ModuleNamespace::new();
        cjs_ns.set("module.exports", Value::int32(42));
        cjs_ns.set("named", Value::int32(7));

        let esm_ns = interop::cjs_to_esm(&cjs_ns);
        assert_eq!(esm_ns.get("default"), Some(Value::int32(42)));
        assert_eq!(esm_ns.get("named"), Some(Value::int32(7)));
    }

    #[test]
    fn test_interop_esm_to_cjs() {
        let esm_ns = ModuleNamespace::new();
        esm_ns.set("default", Value::int32(42));
        esm_ns.set("named", Value::int32(1));

        let cjs_ns = interop::esm_to_cjs(&esm_ns);

        assert!(cjs_ns.has("default"));
        assert!(cjs_ns.has("named"));
    }

    #[test]
    fn test_interop_esm_to_cjs_default_is_always_present() {
        let esm_ns = ModuleNamespace::new();
        esm_ns.set("named", Value::int32(1));

        let cjs_ns = interop::esm_to_cjs(&esm_ns);
        assert!(cjs_ns.has("default"));
        assert_eq!(cjs_ns.get("named"), Some(Value::int32(1)));
    }
}
