//! Module loader for ES modules and CommonJS
//!
//! Handles loading, resolving, and executing JavaScript modules.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use otter_vm_bytecode::Module;
use otter_vm_bytecode::module::{ExportRecord, ImportBinding, ImportRecord};
use otter_vm_compiler::Compiler;
use otter_vm_core::value::Value;

use crate::module_provider::{MediaType, ModuleResolution, ModuleType};
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

    /// Check if export exists
    pub fn has(&self, name: &str) -> bool {
        self.exports
            .read()
            .map(|e| e.contains_key(name))
            .unwrap_or(false)
    }

    /// Convert to a Value (object)
    pub fn to_value(&self) -> Value {
        // For now return undefined, interpreter will handle this
        Value::undefined()
    }
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
        let path = Path::new(url);
        let dirname = path
            .parent()
            .map(|p| {
                let s = p.to_string_lossy().to_string();
                if s.is_empty() { ".".to_string() } else { s }
            })
            .unwrap_or_else(|| ".".to_string());
        let filename = url.to_string();

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
    /// - `module.exports` becomes the default export
    /// - Properties of `module.exports` become named exports
    pub fn cjs_to_esm(namespace: &ModuleNamespace) -> ModuleNamespace {
        // The CJS module.exports is already in the namespace as individual exports
        // For default export, we'd need the whole object - this is handled at runtime
        let result = ModuleNamespace::new();

        // Copy all exports
        for key in namespace.keys() {
            if let Some(value) = namespace.get(&key) {
                result.set(&key, value);
            }
        }

        result
    }

    /// Wrap ESM exports for CJS require
    ///
    /// When CJS requires ESM:
    /// - Returns an object with all named exports
    /// - Default export is available as `.default`
    pub fn esm_to_cjs(namespace: &ModuleNamespace) -> ModuleNamespace {
        // Same as above - the namespace already contains all exports
        let result = ModuleNamespace::new();

        for key in namespace.keys() {
            if let Some(value) = namespace.get(&key) {
                result.set(&key, value);
            }
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
    /// oxc resolver
    resolver: Resolver,
}

impl ModuleLoader {
    /// Create a new module loader
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        let base_dir = base_dir.into();

        // Configure oxc resolver
        let options = ResolveOptions {
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
            resolver: Resolver::new(options),
        }
    }

    /// Compile source code directly as a module and cache it.
    pub fn compile_source(
        &self,
        source: &str,
        url: &str,
        eval_mode: bool,
    ) -> Result<Arc<otter_vm_bytecode::Module>, ModuleError> {
        let is_esm = url.ends_with(".mjs") || url.ends_with(".mts");
        let compiler = Compiler::new();
        let bytecode = compiler
            .compile_ext(source, url, eval_mode, is_esm, false)
            .map_err(|e| ModuleError::CompileError(e.to_string()))?;

        let bytecode_arc = Arc::new(bytecode.clone());
        let loaded = LoadedModule::new(url.to_string(), bytecode);
        let module = Arc::new(RwLock::new(loaded));

        if let Ok(mut modules) = self.modules.write() {
            modules.insert(url.to_string(), module);
        }

        Ok(bytecode_arc)
    }

    /// Update a module's namespace after execution.
    pub fn update_namespace(&self, url: &str, ctx: &otter_vm_core::context::VmContext) {
        if let Some(module) = self.get(url) {
            if let Ok(mut guard) = module.write() {
                let exports = guard.exports().to_vec();
                let global = ctx.global();
                let captured = ctx.captured_exports();

                println!(
                    "Updating namespace for {}. Export count: {}. Captured: {}",
                    url,
                    exports.len(),
                    captured.is_some()
                );

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
                                println!("  Captured default export (from context)");
                                guard.namespace.set("default", val.clone());
                            } else if let Some(val) = global.get(&"default".into()) {
                                println!("  Captured default export (from global)");
                                guard.namespace.set("default", val);
                            } else if let Some(val) = ctx
                                .realm_global(ctx.realm_id())
                                .and_then(|g| g.get(&"default".into()))
                            {
                                println!("  Captured default export (from realm global)");
                                guard.namespace.set("default", val);
                            } else {
                                println!("  FAILED to capture default export");
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

    /// Resolve a module specifier to an absolute path
    pub fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
    ) -> Result<ModuleResolution, ModuleError> {
        // 1. Check registered providers first (node:, otter:, etc.)
        if let Ok(providers) = self.providers.read() {
            for provider in providers.iter() {
                if let Some(resolution) = provider.resolve(specifier, referrer) {
                    return Ok(resolution);
                }
            }
        }

        // 2. Handle absolute paths
        if specifier.starts_with('/') {
            return Ok(ModuleResolution {
                url: specifier.to_string(),
                module_type: if specifier.ends_with(".mjs") || specifier.ends_with(".mts") {
                    ModuleType::ESM
                } else {
                    ModuleType::CommonJS
                },
            });
        }

        // 3. Get the directory of the referrer
        let referrer_path = Path::new(referrer);
        let referrer_dir = referrer_path.parent().unwrap_or(&self.base_dir);

        // 4. Use oxc resolver for filesystem modules
        match self.resolver.resolve(referrer_dir, specifier) {
            Ok(resolution) => {
                let url = resolution.path().to_string_lossy().to_string();
                let module_type = if url.ends_with(".mjs") || url.ends_with(".mts") {
                    ModuleType::ESM
                } else {
                    ModuleType::CommonJS
                };
                Ok(ModuleResolution { url, module_type })
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
        // Check if already loaded
        if let Some(module) = self.modules.read().ok().and_then(|m| m.get(url).cloned()) {
            return Ok(module);
        }

        // 1. Try to load from providers (handles builtin://, custom protocols)
        if let Ok(providers) = self.providers.read() {
            for provider in providers.iter() {
                if let Some(source) = provider.load(url) {
                    // Compile the source from provider
                    let compiler = Compiler::new();
                    let bytecode = compiler
                        .compile(&source.code, url, module_type == ModuleType::ESM)
                        .map_err(|e| ModuleError::CompileError(e.to_string()))?;

                    // Create loaded module
                    let loaded = LoadedModule::new(url.to_string(), bytecode);
                    let module = Arc::new(RwLock::new(loaded));

                    // Store in cache
                    if let Ok(mut modules) = self.modules.write() {
                        modules.insert(url.to_string(), Arc::clone(&module));
                    }

                    return Ok(module);
                }
            }
        }

        // 2. Read from filesystem
        let source =
            std::fs::read_to_string(url).map_err(|e| ModuleError::IoError(e.to_string()))?;

        // Compile
        let compiler = Compiler::new();
        let bytecode = compiler
            .compile(&source, url, module_type == ModuleType::ESM)
            .map_err(|e| ModuleError::CompileError(e.to_string()))?;

        // Create loaded module
        let loaded = LoadedModule::new(url.to_string(), bytecode);
        let module = Arc::new(RwLock::new(loaded));

        // Store in cache
        if let Ok(mut modules) = self.modules.write() {
            modules.insert(url.to_string(), Arc::clone(&module));
        }

        Ok(module)
    }

    /// Build the module dependency graph and return modules in topological order
    pub fn build_graph(&self, entry: &str) -> Result<Vec<String>, ModuleError> {
        let mut order = Vec::new();
        let mut visited = HashMap::new();

        self.visit_module(entry, &mut visited, &mut order)?;

        Ok(order)
    }

    /// DFS visit for topological sort
    fn visit_module(
        &self,
        url: &str,
        visited: &mut HashMap<String, bool>,
        order: &mut Vec<String>,
    ) -> Result<(), ModuleError> {
        // Check if already visited
        if let Some(&in_progress) = visited.get(url) {
            if in_progress {
                // Circular dependency - this is allowed in ESM but needs special handling
                return Ok(());
            }
            // Already fully visited
            return Ok(());
        }

        // Mark as in progress
        visited.insert(url.to_string(), true);

        // Load the module
        // All modules should be loaded/compiled by now via link()
        let module = self
            .get(url)
            .ok_or_else(|| ModuleError::NotFound(url.to_string()))?;
        let imports = {
            let m = module
                .read()
                .map_err(|_| ModuleError::NotFound(url.to_string()))?;
            m.imports().to_vec()
        };

        // Visit dependencies
        for import in imports {
            let resolution = self.resolve(&import.specifier, url)?;
            self.visit_module(&resolution.url, visited, order)?;
        }

        // Mark as complete
        visited.insert(url.to_string(), false);

        // Add to order
        order.push(url.to_string());

        Ok(())
    }

    /// Link a module (resolve all imports)
    pub fn link(&self, url: &str) -> Result<(), ModuleError> {
        let module = self
            .modules
            .read()
            .ok()
            .and_then(|m| m.get(url).cloned())
            .ok_or_else(|| ModuleError::NotFound(url.to_string()))?;

        let mut module_guard = module
            .write()
            .map_err(|_| ModuleError::NotFound(url.to_string()))?;

        if module_guard.state != ModuleState::Unlinked {
            return Ok(());
        }

        module_guard.state = ModuleState::Linking;

        // Process imports
        let imports = module_guard.imports().to_vec();

        for import in imports {
            let resolution = self.resolve(&import.specifier, url)?;
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
        self.modules.read().ok()?.get(url).cloned()
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

    /// Load a module as CommonJS (with wrapper)
    ///
    /// This is used when require() is called from CJS code
    pub fn require(
        &self,
        specifier: &str,
        referrer: &str,
    ) -> Result<Arc<RwLock<LoadedModule>>, ModuleError> {
        // Resolve the specifier
        let resolution = self.resolve(specifier, referrer)?;

        // Load the module
        let module = self.load(&resolution.url, resolution.module_type)?;

        // Build graph and link
        let order = self.build_graph(&resolution.url)?;
        for module_url in &order {
            self.link(module_url)?;
        }

        Ok(module)
    }
}

impl Default for ModuleLoader {
    fn default() -> Self {
        Self::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }
}

/// Create the module extension for dynamic imports and CommonJS require
///
/// This extension provides ops for:
/// - `__module_resolve`: Resolve a module specifier to absolute path
/// - `__module_load`: Load and compile a module (async, for ESM dynamic import)
/// - `__module_require`: Synchronous require for CommonJS
/// - `__module_dirname`: Get __dirname for a module
/// - `__module_filename`: Get __filename for a module
pub fn module_extension(loader: Arc<RwLock<ModuleLoader>>) -> crate::Extension {
    use crate::extension::{op_async, op_sync};
    use serde_json::json;

    let loader_resolve = Arc::clone(&loader);
    let loader_load = Arc::clone(&loader);
    let loader_require = Arc::clone(&loader);
    let loader_dirname = Arc::clone(&loader);

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

                let loader = loader_resolve
                    .read()
                    .map_err(|e| format!("Lock error: {}", e))?;

                match loader.resolve(specifier, referrer) {
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
                    let loader_guard = loader.read().map_err(|e| format!("Lock error: {}", e))?;

                    // Build graph to get all dependencies
                    let order = loader_guard.build_graph(&url).map_err(|e| e.to_string())?;

                    // Link all modules
                    for module_url in &order {
                        loader_guard.link(module_url).map_err(|e| e.to_string())?;
                    }

                    Ok(json!({
                        "url": url,
                        "dependencies": order,
                    }))
                }
            }),
            // Synchronous require for CommonJS
            op_sync("__module_require", move |args| {
                let specifier = args
                    .first()
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "Missing specifier argument".to_string())?;
                let referrer = args
                    .get(1)
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "Missing referrer argument".to_string())?;

                let loader = loader_require
                    .read()
                    .map_err(|e| format!("Lock error: {}", e))?;

                match loader.require(specifier, referrer) {
                    Ok(module) => {
                        let guard = module.read().map_err(|e| e.to_string())?;
                        // Return module info - actual exports are populated by interpreter
                        Ok(json!({
                            "url": guard.url,
                            "type": if guard.is_esm() { "esm" } else { "cjs" },
                        }))
                    }
                    Err(e) => Err(e.to_string()),
                }
            }),
            // Get __dirname for a module
            op_sync("__module_dirname", move |args| {
                let url = args
                    .first()
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "Missing url argument".to_string())?;

                let loader = loader_dirname
                    .read()
                    .map_err(|e| format!("Lock error: {}", e))?;

                let wrapper = loader.get_cjs_wrapper(url);
                Ok(json!(wrapper.dirname()))
            }),
        ])
        .with_js(
            r#"
// Dynamic import helper
globalThis.__dynamicImport = async function(specifier, referrer) {
    const resolved = __module_resolve(specifier, referrer);
    const result = await __module_load(resolved);
    return result;
};

// CommonJS require (synchronous)
// Note: In real usage, require is created per-module with correct referrer
globalThis.__createRequire = function(referrer) {
    function require(specifier) {
        return __module_require(specifier, referrer);
    }

    require.resolve = function(specifier) {
        return __module_resolve(specifier, referrer);
    };

    require.cache = {};

    return require;
};

// Get __dirname for a module
globalThis.__getDirname = function(url) {
    return __module_dirname(url);
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
    fn test_interop_esm_to_cjs() {
        let esm_ns = ModuleNamespace::new();
        esm_ns.set("default", Value::int32(42));
        esm_ns.set("named", Value::int32(1));

        let cjs_ns = interop::esm_to_cjs(&esm_ns);

        assert!(cjs_ns.has("default"));
        assert!(cjs_ns.has("named"));
    }
}
