//! §16.2.1 — Module loading, linking, and evaluation.
//!
//! Provides the `ModuleHost` trait for embedder-supplied module resolution/loading,
//! and the `ModuleRegistry` that tracks compiled/evaluated modules.
//!
//! Spec: <https://tc39.es/ecma262/#sec-source-text-module-records>

use std::collections::BTreeMap;

use crate::interpreter::{InterpreterError, RuntimeState};
use crate::module::{ExportRecord, ImportBinding, Module};
use crate::value::RegisterValue;

/// §16.2.1.2 — Module states during linking/evaluation.
/// Spec: <https://tc39.es/ecma262/#sec-moduledeclarationlinking>
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleState {
    /// Compiled, not yet linked.
    Unlinked,
    /// Currently resolving imports.
    Linking,
    /// Imports resolved, ready to execute.
    Linked,
    /// Bytecode executing.
    Evaluating,
    /// Execution complete, namespace populated.
    Evaluated,
    /// Evaluation failed.
    Error,
}

/// A loaded module entry in the registry.
#[derive(Debug)]
pub struct LoadedModule {
    /// The compiled module.
    pub module: Module,
    /// Current lifecycle state.
    pub state: ModuleState,
    /// Collected export values after evaluation.
    /// Maps export name → value.
    pub namespace: BTreeMap<String, RegisterValue>,
}

/// Module host trait — the embedder supplies this to resolve and load modules.
///
/// §16.2.1.5.3 — HostResolveImportedModule
/// Spec: <https://tc39.es/ecma262/#sec-hostresolveimportedmodule>
pub trait ModuleHost {
    /// Resolves a module specifier relative to a referrer URL.
    /// Returns the canonical URL for the resolved module.
    fn resolve(&self, specifier: &str, referrer: &str) -> Result<String, String>;

    /// Loads and returns the source code for a resolved module URL.
    fn load(&self, url: &str) -> Result<String, String>;
}

/// In-memory module host for testing — maps URLs to source strings.
#[derive(Debug, Default)]
pub struct InMemoryModuleHost {
    modules: BTreeMap<String, String>,
}

impl InMemoryModuleHost {
    /// Creates a new in-memory host.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a module source at the given URL.
    pub fn add_module(&mut self, url: impl Into<String>, source: impl Into<String>) {
        self.modules.insert(url.into(), source.into());
    }
}

impl ModuleHost for InMemoryModuleHost {
    fn resolve(&self, specifier: &str, _referrer: &str) -> Result<String, String> {
        // Simple resolution: strip ./ prefix, treat as canonical URL.
        let canonical = specifier.strip_prefix("./").unwrap_or(specifier);
        if self.modules.contains_key(canonical) {
            Ok(canonical.to_string())
        } else {
            Err(format!("module not found: {specifier}"))
        }
    }

    fn load(&self, url: &str) -> Result<String, String> {
        self.modules
            .get(url)
            .cloned()
            .ok_or_else(|| format!("module not found: {url}"))
    }
}

/// Module registry — tracks all loaded/evaluated modules in one runtime.
#[derive(Debug, Default)]
pub struct ModuleRegistry {
    /// URL → loaded module.
    modules: BTreeMap<String, LoadedModule>,
}

impl ModuleRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a reference to a loaded module by URL.
    pub fn get(&self, url: &str) -> Option<&LoadedModule> {
        self.modules.get(url)
    }

    /// Returns a mutable reference to a loaded module by URL.
    pub fn get_mut(&mut self, url: &str) -> Option<&mut LoadedModule> {
        self.modules.get_mut(url)
    }

    /// Inserts a compiled module into the registry.
    pub fn insert(&mut self, url: String, module: Module) {
        self.modules.insert(
            url,
            LoadedModule {
                module,
                state: ModuleState::Unlinked,
                namespace: BTreeMap::new(),
            },
        );
    }

    /// Returns whether a module is already in the registry.
    pub fn contains(&self, url: &str) -> bool {
        self.modules.contains_key(url)
    }

    /// Returns the export value for a given module URL and export name.
    pub fn get_export(&self, url: &str, name: &str) -> Option<RegisterValue> {
        self.modules
            .get(url)
            .and_then(|m| m.namespace.get(name).copied())
    }
}

/// §16.2.1.6 — Execute a module graph starting from an entry URL.
///
/// 1. Resolve and load all transitive dependencies (DFS).
/// 2. Link imports (build import→export mappings).
/// 3. Evaluate in topological order (leaves first).
///
/// Spec: <https://tc39.es/ecma262/#sec-moduleevaluation>
pub fn execute_module_graph(
    entry_url: &str,
    host: &dyn ModuleHost,
    runtime: &mut RuntimeState,
    registry: &mut ModuleRegistry,
) -> Result<(), InterpreterError> {
    // Phase 1: Build the module graph — compile and register all modules.
    let topo_order = build_module_graph(entry_url, host, registry)?;

    // Phase 2: Link — resolve import bindings.
    for url in &topo_order {
        link_module(url, registry)?;
    }

    // Phase 3: Evaluate in topological order.
    let interpreter = crate::Interpreter::new();
    for url in &topo_order {
        evaluate_module(url, &interpreter, runtime, registry)?;
    }

    Ok(())
}

/// DFS-based module graph builder. Returns URLs in topological order
/// (dependencies before dependents).
fn build_module_graph(
    url: &str,
    host: &dyn ModuleHost,
    registry: &mut ModuleRegistry,
) -> Result<Vec<String>, InterpreterError> {
    let mut order = Vec::new();
    let mut visiting: BTreeMap<String, bool> = BTreeMap::new(); // true = in-progress
    visit_module(url, host, registry, &mut visiting, &mut order)?;
    Ok(order)
}

fn visit_module(
    url: &str,
    host: &dyn ModuleHost,
    registry: &mut ModuleRegistry,
    visiting: &mut BTreeMap<String, bool>,
    order: &mut Vec<String>,
) -> Result<(), InterpreterError> {
    if let Some(&in_progress) = visiting.get(url) {
        if in_progress {
            // Circular dependency — allowed in ESM, just skip.
            return Ok(());
        }
        // Already fully visited.
        return Ok(());
    }

    visiting.insert(url.to_string(), true);

    // Compile the module if not already in the registry.
    if !registry.contains(url) {
        let source = host.load(url).map_err(|e| {
            InterpreterError::NativeCall(format!("module load error: {e}").into())
        })?;
        let compiled = crate::source::compile_module(&source, url).map_err(|e| {
            InterpreterError::NativeCall(format!("module compile error: {e}").into())
        })?;
        registry.insert(url.to_string(), compiled);
    }

    // Collect import specifiers to visit.
    let specifiers: Vec<String> = registry
        .get(url)
        .map(|m| {
            m.module
                .imports()
                .iter()
                .map(|imp| imp.specifier.to_string())
                .collect()
        })
        .unwrap_or_default();

    // Also collect re-export specifiers.
    let reexport_specifiers: Vec<String> = registry
        .get(url)
        .map(|m| {
            m.module
                .exports()
                .iter()
                .filter_map(|exp| match exp {
                    ExportRecord::ReExportNamed { specifier, .. }
                    | ExportRecord::ReExportAll { specifier }
                    | ExportRecord::ReExportNamespace { specifier, .. } => {
                        Some(specifier.to_string())
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();

    // Visit all dependency specifiers.
    for specifier in specifiers.iter().chain(reexport_specifiers.iter()) {
        let resolved = host.resolve(specifier, url).map_err(|e| {
            InterpreterError::NativeCall(format!("module resolve error: {e}").into())
        })?;
        visit_module(&resolved, host, registry, visiting, order)?;
    }

    visiting.insert(url.to_string(), false);
    order.push(url.to_string());
    Ok(())
}

/// §16.2.1.6.1 — Link a module's imports to their source exports.
fn link_module(url: &str, registry: &mut ModuleRegistry) -> Result<(), InterpreterError> {
    let loaded = registry.get(url).ok_or_else(|| {
        InterpreterError::NativeCall(format!("module not in registry: {url}").into())
    })?;

    if loaded.state != ModuleState::Unlinked {
        return Ok(());
    }

    // For now, just advance state. Actual import resolution happens at
    // evaluate time since we need the runtime to resolve values.
    let loaded = registry.get_mut(url).unwrap();
    loaded.state = ModuleState::Linked;
    Ok(())
}

/// §16.2.1.6.2 — Evaluate a single module.
///
/// 1. Set state to Evaluating.
/// 2. Build initial register window with resolved import values.
/// 3. Execute the module's entry function.
/// 4. Capture exports into the namespace.
fn evaluate_module(
    url: &str,
    interpreter: &crate::Interpreter,
    runtime: &mut RuntimeState,
    registry: &mut ModuleRegistry,
) -> Result<(), InterpreterError> {
    {
        let loaded = registry.get(url).ok_or_else(|| {
            InterpreterError::NativeCall(format!("module not in registry: {url}").into())
        })?;

        if loaded.state == ModuleState::Evaluated || loaded.state == ModuleState::Evaluating {
            return Ok(());
        }
    }

    registry.get_mut(url).unwrap().state = ModuleState::Evaluating;

    // Pre-populate global object with import values from dependency namespaces.
    // In module mode, import bindings resolve via GetGlobal (no local allocation).
    populate_import_globals(url, registry, runtime)?;

    let module = &registry.get(url).unwrap().module;

    // Execute the module entry function.
    let global = runtime.intrinsics().global_object();
    let registers = [RegisterValue::from_object_handle(global.0)];

    interpreter.execute_with_runtime(
        module,
        module.entry(),
        &registers,
        runtime,
    )?;

    // Capture exports from the global object into the module namespace.
    // Module compilation emits SetGlobal for all exports.
    capture_exports(url, runtime, registry)?;

    registry.get_mut(url).unwrap().state = ModuleState::Evaluated;
    Ok(())
}

/// Pre-populates the global object with import values from dependency namespaces.
/// In module mode, import bindings are NOT local variables — they resolve via
/// `GetGlobal` at the use site. This function sets those globals before execution.
fn populate_import_globals(
    url: &str,
    registry: &ModuleRegistry,
    runtime: &mut RuntimeState,
) -> Result<(), InterpreterError> {
    let loaded = registry.get(url).ok_or_else(|| {
        InterpreterError::NativeCall(format!("module not in registry: {url}").into())
    })?;

    // Collect all import bindings to avoid borrow conflicts.
    let bindings: Vec<(String, String, String)> = loaded
        .module
        .imports()
        .iter()
        .flat_map(|import_record| {
            let source_url = import_record.specifier.to_string();
            import_record.bindings.iter().map(move |binding| {
                let (export_name, local_name) = match binding {
                    ImportBinding::Named { imported, local } => {
                        (imported.to_string(), local.to_string())
                    }
                    ImportBinding::Default { local } => ("default".to_string(), local.to_string()),
                    ImportBinding::Namespace { local } => ("*".to_string(), local.to_string()),
                };
                (source_url.clone(), export_name, local_name)
            })
        })
        .collect();

    for (source_url, export_name, local_name) in bindings {
        let value = if export_name == "*" {
            build_namespace_object(&source_url, registry, runtime)
        } else {
            registry
                .get_export(&source_url, &export_name)
                .unwrap_or_default()
        };
        runtime.install_global_value(&local_name, value);
    }

    Ok(())
}

/// Builds a JS object representing the module namespace (`import * as ns`).
fn build_namespace_object(
    source_url: &str,
    registry: &ModuleRegistry,
    runtime: &mut RuntimeState,
) -> RegisterValue {
    let ns_object = runtime.alloc_object();
    if let Some(loaded) = registry.get(source_url) {
        for (name, value) in &loaded.namespace {
            let prop = runtime.intern_property_name(name);
            runtime
                .objects_mut()
                .set_property(ns_object, prop, *value)
                .ok();
        }
    }
    RegisterValue::from_object_handle(ns_object.0)
}

/// Captures export values from the global object into the module namespace.
/// Module compilation emits SetGlobal for all exported bindings.
fn capture_exports(
    url: &str,
    runtime: &mut RuntimeState,
    registry: &mut ModuleRegistry,
) -> Result<(), InterpreterError> {
    let loaded = registry.get(url).ok_or_else(|| {
        InterpreterError::NativeCall(format!("module not in registry: {url}").into())
    })?;

    // Collect export records first to avoid borrow conflicts.
    let export_records: Vec<ExportRecord> = loaded.module.exports().to_vec();

    for export in &export_records {
        match export {
            ExportRecord::Named { local, exported } => {
                // Look up the local binding's value from the global object.
                // (Module top-level `var`/`let`/`const` are compiled as globals in script mode.)
                let value = lookup_module_local(local, runtime);
                registry
                    .get_mut(url)
                    .unwrap()
                    .namespace
                    .insert(exported.to_string(), value);
            }
            ExportRecord::Default { local } => {
                let value = lookup_module_local(local, runtime);
                registry
                    .get_mut(url)
                    .unwrap()
                    .namespace
                    .insert("default".to_string(), value);
            }
            ExportRecord::ReExportNamed {
                specifier,
                imported,
                exported,
            } => {
                let value = registry
                    .get_export(specifier, imported)
                    .unwrap_or_default();
                registry
                    .get_mut(url)
                    .unwrap()
                    .namespace
                    .insert(exported.to_string(), value);
            }
            ExportRecord::ReExportAll { specifier } => {
                // Copy all exports from the source module.
                let source_exports: Vec<(String, RegisterValue)> = registry
                    .get(specifier.as_ref())
                    .map(|m| {
                        m.namespace
                            .iter()
                            .filter(|(k, _)| k.as_str() != "default") // `export *` excludes default
                            .map(|(k, v)| (k.clone(), *v))
                            .collect()
                    })
                    .unwrap_or_default();
                for (name, value) in source_exports {
                    registry
                        .get_mut(url)
                        .unwrap()
                        .namespace
                        .insert(name, value);
                }
            }
            ExportRecord::ReExportNamespace {
                specifier,
                exported,
            } => {
                let ns_value = build_namespace_object(specifier, registry, runtime);
                registry
                    .get_mut(url)
                    .unwrap()
                    .namespace
                    .insert(exported.to_string(), ns_value);
            }
        }
    }

    Ok(())
}

/// Looks up a module-level local binding's value.
/// Module top-level bindings are stored as properties on the global object.
fn lookup_module_local(name: &str, runtime: &mut RuntimeState) -> RegisterValue {
    let global = runtime.intrinsics().global_object();
    let prop = runtime.intern_property_name(name);
    runtime
        .own_property_value(global, prop)
        .unwrap_or_default()
}
