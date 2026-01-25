//! Extension system for registering ops and JS code
//!
//! Extensions bundle operations (sync and async) with optional JavaScript setup code.
//! This allows native Rust functionality to be exposed to the JavaScript runtime.
//!
//! ## Operation Types
//!
//! - **JSON ops** (`op_sync`, `op_async`): Work with `serde_json::Value`, good for simple data
//! - **Native ops** (`op_native`): Work with VM `Value` directly, needed for object identity
//!   operations like `Object.freeze()`, `WeakMap`, etc.

use otter_vm_core::value::Value as VmValue;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Result type for JSON operations
pub type OpResult = Result<JsonValue, String>;

/// Result type for native operations (works with VM Value)
pub type NativeOpResult = Result<VmValue, String>;

/// Future type for async operations
pub type OpFuture = Pin<Box<dyn Future<Output = OpResult> + Send>>;

/// Type for sync operation handler (JSON-based)
pub type SyncOpFn = Arc<dyn Fn(&[JsonValue]) -> OpResult + Send + Sync>;

/// Type for async operation handler (JSON-based)
pub type AsyncOpFn = Arc<dyn Fn(&[JsonValue]) -> OpFuture + Send + Sync>;

/// Type for native sync operation handler (works with VM Value)
pub type NativeSyncOpFn = Arc<dyn Fn(&[VmValue]) -> NativeOpResult + Send + Sync>;

/// Operation handler type
#[derive(Clone)]
pub enum OpHandler {
    /// Synchronous JSON operation
    Sync(SyncOpFn),
    /// Asynchronous JSON operation
    Async(AsyncOpFn),
    /// Native synchronous operation (works with VM Value directly)
    Native(NativeSyncOpFn),
}

impl OpHandler {
    /// Execute sync JSON operation (errors if async or native)
    pub fn call_sync(&self, args: &[JsonValue]) -> OpResult {
        match self {
            OpHandler::Sync(f) => f(args),
            OpHandler::Async(_) => Err("Cannot call async op synchronously".to_string()),
            OpHandler::Native(_) => Err("Cannot call native op with JSON args".to_string()),
        }
    }

    /// Execute native sync operation (works with VM Value)
    pub fn call_native(&self, args: &[VmValue]) -> NativeOpResult {
        match self {
            OpHandler::Native(f) => f(args),
            OpHandler::Sync(_) => Err("Cannot call JSON op with native args".to_string()),
            OpHandler::Async(_) => Err("Cannot call async op with native args".to_string()),
        }
    }

    /// Get async future (returns None if sync or native)
    pub fn call_async(&self, args: &[JsonValue]) -> Option<OpFuture> {
        match self {
            OpHandler::Sync(_) | OpHandler::Native(_) => None,
            OpHandler::Async(f) => Some(f(args)),
        }
    }

    /// Check if this is a sync JSON operation
    pub fn is_sync(&self) -> bool {
        matches!(self, OpHandler::Sync(_))
    }

    /// Check if this is an async operation
    pub fn is_async(&self) -> bool {
        matches!(self, OpHandler::Async(_))
    }

    /// Check if this is a native operation
    pub fn is_native(&self) -> bool {
        matches!(self, OpHandler::Native(_))
    }
}

/// A single operation definition
#[derive(Clone)]
pub struct Op {
    /// Operation name (used to register as global function)
    pub name: String,
    /// Handler function
    pub handler: OpHandler,
}

/// Extension bundle
///
/// An extension groups related operations and optional JavaScript setup code.
/// Extensions can declare dependencies on other extensions.
#[derive(Clone)]
pub struct Extension {
    /// Extension name (unique identifier)
    name: String,
    /// Operations provided by this extension
    ops: Vec<Op>,
    /// JavaScript setup code chunks (run after ops are registered, in insertion order)
    js: Vec<String>,
    /// Pre-compiled JS chunks (if any)
    compiled_js: Vec<Arc<otter_vm_bytecode::Module>>,
    /// Dependencies (names of other extensions that must be loaded first)
    deps: Vec<String>,
}

impl Extension {
    /// Create a new extension with the given name
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ops: Vec::new(),
            js: Vec::new(),
            compiled_js: Vec::new(),
            deps: Vec::new(),
        }
    }

    /// Add operations to the extension (builder pattern)
    pub fn with_ops(mut self, ops: Vec<Op>) -> Self {
        self.ops = ops;
        self
    }

    /// Add JavaScript setup code (builder pattern)
    pub fn with_js(mut self, js: impl Into<String>) -> Self {
        self.js.push(js.into());
        self
    }

    /// Add dependencies (builder pattern)
    pub fn with_deps(mut self, deps: Vec<String>) -> Self {
        self.deps = deps;
        self
    }

    /// Add pre-compiled JS module (builder pattern)
    pub fn with_compiled_js(mut self, module: Arc<otter_vm_bytecode::Module>) -> Self {
        self.compiled_js.push(module);
        self
    }

    /// Get extension name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get operations
    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    /// Take operations (consumes them)
    pub fn take_ops(&mut self) -> Vec<Op> {
        std::mem::take(&mut self.ops)
    }

    /// Get JavaScript setup code
    pub fn js(&self) -> &[String] {
        &self.js
    }

    /// Get dependencies
    pub fn deps(&self) -> &[String] {
        &self.deps
    }

    /// Pre-compile all JS chunks
    pub fn pre_compile(&mut self) -> Result<(), String> {
        // Skip compilation if already done matching JS chunks
        if !self.compiled_js.is_empty() && self.compiled_js.len() == self.js.len() {
            return Ok(());
        }

        self.compiled_js.clear();
        for js in &self.js {
            let compiler = otter_vm_compiler::Compiler::new();
            let module = compiler.compile(js, "setup.js").map_err(|e| {
                format!("Failed to compile extension JS for '{}': {}", self.name, e)
            })?;
            self.compiled_js.push(Arc::new(module));
        }
        Ok(())
    }

    /// Get pre-compiled JS modules
    pub fn compiled_js(&self) -> &[Arc<otter_vm_bytecode::Module>] {
        &self.compiled_js
    }
}

/// Helper to create a sync operation
///
/// # Example
/// ```ignore
/// op_sync("math_add", |args| {
///     let a = args.get(0).and_then(|v| v.as_f64()).unwrap_or(0.0);
///     let b = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
///     Ok(serde_json::json!(a + b))
/// })
/// ```
pub fn op_sync<F>(name: impl Into<String>, handler: F) -> Op
where
    F: Fn(&[JsonValue]) -> OpResult + Send + Sync + 'static,
{
    Op {
        name: name.into(),
        handler: OpHandler::Sync(Arc::new(handler)),
    }
}

/// Helper to create an async operation
///
/// # Example
/// ```ignore
/// op_async("fetch_data", |args| async move {
///     // ... async work ...
///     Ok(serde_json::json!({"status": "ok"}))
/// })
/// ```
///
/// # Note on Capabilities
///
/// The handler is called on the main thread BEFORE spawning the async task.
/// This allows permission checks (e.g., `capabilities_context::can_net()`) to work
/// because they run while `CapabilitiesGuard` is active on the main thread.
/// The returned future is then spawned to tokio.
pub fn op_async<F, Fut>(name: impl Into<String>, handler: F) -> Op
where
    F: Fn(&[JsonValue]) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = OpResult> + Send + 'static,
{
    let handler = Arc::new(handler);
    Op {
        name: name.into(),
        handler: OpHandler::Async(Arc::new(move |args: &[JsonValue]| {
            // IMPORTANT: Call handler on main thread, not inside async block!
            // This allows permission checks to access thread-local CapabilitiesGuard.
            let future = handler(args);
            Box::pin(future)
        })),
    }
}

/// Helper to create a native operation that works with VM Value directly
///
/// Use this for operations that need object identity, like:
/// - `Object.freeze()` / `Object.isFrozen()`
/// - `Object.seal()` / `Object.isSealed()`
/// - `WeakMap` / `WeakSet` operations
///
/// # Example
/// ```ignore
/// use otter_vm_core::value::Value;
/// use otter_vm_core::object::JsObject;
///
/// op_native("__Object_freeze", |args| {
///     let obj = args.first().ok_or("Missing argument")?;
///     if let Some(obj_ref) = obj.as_object() {
///         obj_ref.freeze();
///     }
///     Ok(obj.clone())
/// })
/// ```
pub fn op_native<F>(name: impl Into<String>, handler: F) -> Op
where
    F: Fn(&[VmValue]) -> NativeOpResult + Send + Sync + 'static,
{
    Op {
        name: name.into(),
        handler: OpHandler::Native(Arc::new(handler)),
    }
}

/// Extension registry
///
/// Manages registered extensions and provides lookup for operations.
pub struct ExtensionRegistry {
    /// Registered extensions by name
    extensions: HashMap<String, Extension>,
    /// All operations by name (for fast lookup)
    ops: HashMap<String, OpHandler>,
    /// Order in which extensions were registered (for JS execution order)
    load_order: Vec<String>,
}

impl ExtensionRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self {
            extensions: HashMap::new(),
            ops: HashMap::new(),
            load_order: Vec::new(),
        }
    }

    /// Pre-compile all extensions in the registry
    pub fn pre_compile_all(&mut self) -> Result<(), String> {
        for ext in self.extensions.values_mut() {
            ext.pre_compile()?;
        }
        Ok(())
    }

    /// Register an extension
    ///
    /// Returns error if:
    /// - A dependency is not yet registered
    /// - An operation name conflicts with an existing one
    pub fn register(&mut self, mut ext: Extension) -> Result<(), String> {
        // Check if already registered
        if self.extensions.contains_key(&ext.name) {
            return Err(format!("Extension already registered: {}", ext.name));
        }

        // Check dependencies
        for dep in &ext.deps {
            if !self.extensions.contains_key(dep) {
                return Err(format!(
                    "Missing dependency '{}' for extension '{}'",
                    dep, ext.name
                ));
            }
        }

        // Check for op name conflicts
        for op in &ext.ops {
            if self.ops.contains_key(&op.name) {
                return Err(format!(
                    "Operation '{}' already registered (extension '{}')",
                    op.name, ext.name
                ));
            }
        }

        // Register operations
        for op in ext.take_ops() {
            self.ops.insert(op.name, op.handler);
        }

        // Track load order and store extension
        self.load_order.push(ext.name.clone());
        self.extensions.insert(ext.name.clone(), ext);

        Ok(())
    }

    /// Get an operation handler by name
    pub fn get_op(&self, name: &str) -> Option<&OpHandler> {
        self.ops.get(name)
    }

    /// Get an extension by name
    pub fn get_extension(&self, name: &str) -> Option<&Extension> {
        self.extensions.get(name)
    }

    /// Check if an extension is registered
    pub fn has_extension(&self, name: &str) -> bool {
        self.extensions.contains_key(name)
    }

    /// Get all JavaScript setup code in load order
    pub fn all_js(&self) -> Vec<&str> {
        self.load_order
            .iter()
            .flat_map(|name| {
                self.extensions
                    .get(name)
                    .into_iter()
                    .flat_map(|ext| ext.js().iter().map(|s| s.as_str()))
            })
            .collect()
    }

    /// Get all pre-compiled JS modules in load order
    pub fn all_compiled_js(&self) -> Vec<Arc<otter_vm_bytecode::Module>> {
        self.load_order
            .iter()
            .flat_map(|name| {
                self.extensions
                    .get(name)
                    .into_iter()
                    .flat_map(|ext| ext.compiled_js().iter().cloned())
            })
            .collect()
    }

    /// Get all operation names
    pub fn op_names(&self) -> impl Iterator<Item = &str> {
        self.ops.keys().map(|s| s.as_str())
    }

    /// Get number of registered extensions
    pub fn extension_count(&self) -> usize {
        self.extensions.len()
    }

    /// Get number of registered operations
    pub fn op_count(&self) -> usize {
        self.ops.len()
    }
}

impl Default for ExtensionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extension_creation() {
        let ext = Extension::new("test")
            .with_ops(vec![op_sync("test_add", |args| {
                let a = args.first().and_then(|v| v.as_i64()).unwrap_or(0);
                let b = args.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
                Ok(json!(a + b))
            })])
            .with_js("console.log('test loaded');");

        assert_eq!(ext.name(), "test");
        assert_eq!(ext.ops().len(), 1);
        assert_eq!(ext.js().len(), 1);
        assert_eq!(ext.js()[0], "console.log('test loaded');");
    }

    #[test]
    fn test_registry_register() {
        let mut registry = ExtensionRegistry::new();

        let ext = Extension::new("math").with_ops(vec![op_sync("math_add", |args| {
            let a = args.first().and_then(|v| v.as_f64()).unwrap_or(0.0);
            let b = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
            Ok(json!(a + b))
        })]);

        registry.register(ext).unwrap();
        assert!(registry.has_extension("math"));
        assert!(registry.get_op("math_add").is_some());
    }

    #[test]
    fn test_registry_dependencies() {
        let mut registry = ExtensionRegistry::new();

        // Try to register extension with missing dependency
        let ext = Extension::new("dependent").with_deps(vec!["base".to_string()]);

        let result = registry.register(ext);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Missing dependency"));

        // Register base first
        let base = Extension::new("base");
        registry.register(base).unwrap();

        // Now dependent should work
        let dependent = Extension::new("dependent").with_deps(vec!["base".to_string()]);
        registry.register(dependent).unwrap();

        assert!(registry.has_extension("base"));
        assert!(registry.has_extension("dependent"));
    }

    #[test]
    fn test_registry_op_conflict() {
        let mut registry = ExtensionRegistry::new();

        let ext1 = Extension::new("ext1").with_ops(vec![op_sync("shared_op", |_| Ok(json!(1)))]);

        let ext2 = Extension::new("ext2").with_ops(vec![op_sync("shared_op", |_| Ok(json!(2)))]);

        registry.register(ext1).unwrap();
        let result = registry.register(ext2);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already registered"));
    }

    #[test]
    fn test_sync_op_execution() {
        let mut registry = ExtensionRegistry::new();

        let ext = Extension::new("math").with_ops(vec![op_sync("multiply", |args| {
            let a = args.first().and_then(|v| v.as_i64()).unwrap_or(1);
            let b = args.get(1).and_then(|v| v.as_i64()).unwrap_or(1);
            Ok(json!(a * b))
        })]);

        registry.register(ext).unwrap();

        let op = registry.get_op("multiply").unwrap();
        let result = op.call_sync(&[json!(6), json!(7)]).unwrap();
        assert_eq!(result, json!(42));
    }

    #[test]
    fn test_async_op_creation() {
        let op = op_async("async_test", |args| {
            let val = args.first().and_then(|v| v.as_i64()).unwrap_or(0);
            async move { Ok(json!(val * 2)) }
        });

        assert_eq!(op.name, "async_test");
        assert!(op.handler.is_async());
    }

    #[test]
    fn test_js_load_order() {
        let mut registry = ExtensionRegistry::new();

        registry
            .register(Extension::new("first").with_js("// first"))
            .unwrap();
        registry
            .register(Extension::new("second").with_js("// second"))
            .unwrap();
        registry.register(Extension::new("no_js")).unwrap();
        registry
            .register(Extension::new("third").with_js("// third"))
            .unwrap();

        let js = registry.all_js();
        assert_eq!(js, vec!["// first", "// second", "// third"]);
    }

    #[test]
    fn test_duplicate_extension() {
        let mut registry = ExtensionRegistry::new();

        registry.register(Extension::new("dup")).unwrap();
        let result = registry.register(Extension::new("dup"));

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already registered"));
    }
}
