//! VM runtime - the main entry point
//!
//! The runtime manages module loading, context creation, and execution.

use indexmap::IndexMap;
use std::sync::{Arc, Mutex};

use otter_vm_bytecode::Module;

use crate::context::VmContext;
use crate::error::VmResult;
use crate::gc::GcRef;
use crate::globals;
use crate::interpreter::Interpreter;
use crate::intrinsics::Intrinsics;
use crate::object::JsObject;
use crate::realm::{RealmId, RealmRecord, RealmRegistry};
use crate::value::Value;

/// Maximum number of compiled modules to cache before evicting the oldest (FIFO).
const MAX_MODULE_CACHE_SIZE: usize = 512;

/// The VM runtime
///
/// This is the main entry point for executing JavaScript.
/// It is `Send + Sync` and can be shared across threads.
pub struct VmRuntime {
    /// Loaded modules — bounded FIFO cache.
    /// Oldest entry is evicted when the cache exceeds `MAX_MODULE_CACHE_SIZE`.
    /// Closures holding `Arc<Module>` extend the module's lifetime beyond eviction.
    modules: Mutex<IndexMap<String, Arc<Module>>>,
    /// Global object template
    #[allow(dead_code)]
    global_template: GcRef<JsObject>,
    /// Runtime configuration
    config: RuntimeConfig,
    /// Memory manager for this runtime
    memory_manager: Arc<crate::memory::MemoryManager>,
    /// Intrinsic `%Function.prototype%` object (ES2023 §10.3.1).
    /// Created once at runtime init, shared across contexts.
    function_prototype: GcRef<JsObject>,
    /// All intrinsic objects and well-known symbols.
    /// Created once at runtime init, shared across contexts.
    intrinsics: Intrinsics,
    /// Realm registry for cross-realm lookups.
    realm_registry: std::sync::Arc<RealmRegistry>,
    /// Default realm id.
    default_realm_id: RealmId,
}

/// Runtime configuration
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Maximum stack depth
    pub max_stack_depth: usize,
    /// Maximum heap size in bytes
    pub max_heap_size: usize,
    /// Enable strict mode by default
    pub strict_mode: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_stack_depth: 10000,
            max_heap_size: 512 * 1024 * 1024, // 512 MB
            strict_mode: true,
        }
    }
}

impl VmRuntime {
    /// Create a new runtime with default configuration
    pub fn new() -> Self {
        Self::with_config(RuntimeConfig::default())
    }

    /// Create a new runtime with custom configuration
    pub fn with_config(config: RuntimeConfig) -> Self {
        let memory_manager = Arc::new(crate::memory::MemoryManager::new(config.max_heap_size));
        let realm_registry = RealmRegistry::new();
        let default_realm_id = realm_registry.allocate_id();

        // Create intrinsic %Function.prototype% FIRST, before any other objects.
        // Per ES2023 §10.3.1, every built-in function object must have this
        // as its [[Prototype]]. By creating it up-front, all native functions
        // can receive it at construction time
        let function_prototype = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        function_prototype.mark_as_intrinsic();
        // Realm id for the default realm.
        function_prototype.define_property(
            crate::object::PropertyKey::string("__realm_id__"),
            crate::object::PropertyDescriptor::builtin_data(Value::int32(default_realm_id as i32)),
        );

        // Stage 1: Allocate all intrinsic objects (empty, no properties yet)
        let intrinsics = Intrinsics::allocate(&memory_manager, function_prototype);
        // Stage 2: Wire prototype chains (Object.prototype -> null, etc.)
        intrinsics.wire_prototype_chains();
        // Stage 3: Initialize core intrinsic properties (toString, valueOf, etc.)
        intrinsics.init_core(&memory_manager);

        let global = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        global.define_property(
            crate::object::PropertyKey::string("__realm_id__"),
            crate::object::PropertyDescriptor::builtin_data(Value::int32(default_realm_id as i32)),
        );
        globals::setup_global_object(global, function_prototype, Some(&intrinsics));
        // Install intrinsic constructors on global (Object, Function, etc.)
        intrinsics.install_on_global(global, &memory_manager);

        // Register default realm in registry.
        realm_registry.insert(RealmRecord {
            id: default_realm_id,
            intrinsics: intrinsics.clone(),
            function_prototype,
            global,
        });

        Self {
            modules: Mutex::new(IndexMap::new()),
            global_template: global,
            function_prototype,
            intrinsics,
            memory_manager,
            config,
            realm_registry,
            default_realm_id,
        }
    }

    /// Load a module from bytecode.
    ///
    /// Inserts the module into the bounded FIFO cache.  When the cache reaches
    /// `MAX_MODULE_CACHE_SIZE` the oldest entry is evicted before insertion.
    /// Closures that have already captured an `Arc<Module>` reference keep the
    /// module alive beyond its eviction from the cache.
    pub fn load_module(&self, module: Module) -> Arc<Module> {
        let url = module.source_url.clone();
        let module = Arc::new(module);
        let mut cache = self.modules.lock().unwrap();
        // Evict oldest entry if at capacity (but not if the URL already exists).
        if !cache.contains_key(&url) && cache.len() >= MAX_MODULE_CACHE_SIZE {
            cache.shift_remove_index(0);
        }
        cache.insert(url, module.clone());
        module
    }

    /// Get a loaded module by URL
    pub fn get_module(&self, url: &str) -> Option<Arc<Module>> {
        self.modules.lock().unwrap().get(url).cloned()
    }

    /// Create a new execution context
    pub fn create_context(&self) -> VmContext {
        self.create_context_in_realm(self.default_realm_id)
    }

    /// Create a new realm with its own intrinsics and Function.prototype.
    pub fn create_realm(&self) -> RealmId {
        let realm_id = self.realm_registry.allocate_id();
        let mm = self.memory_manager.clone();

        let function_prototype = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        function_prototype.mark_as_intrinsic();
        function_prototype.define_property(
            crate::object::PropertyKey::string("__realm_id__"),
            crate::object::PropertyDescriptor::builtin_data(Value::int32(realm_id as i32)),
        );

        let intrinsics = Intrinsics::allocate(&mm, function_prototype);
        intrinsics.wire_prototype_chains();
        intrinsics.init_core(&mm);

        let global = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        global.define_property(
            crate::object::PropertyKey::string("__realm_id__"),
            crate::object::PropertyDescriptor::builtin_data(Value::int32(realm_id as i32)),
        );
        globals::setup_global_object(global, function_prototype, Some(&intrinsics));
        // Install intrinsic constructors on the realm global
        intrinsics.install_on_global(global, &mm);

        self.realm_registry.insert(RealmRecord {
            id: realm_id,
            intrinsics,
            function_prototype,
            global,
        });

        realm_id
    }

    /// Create a new execution context in the given realm.
    pub fn create_context_in_realm(&self, realm_id: RealmId) -> VmContext {
        let realm = self
            .realm_registry
            .get(realm_id)
            .unwrap_or_else(|| RealmRecord {
                id: self.default_realm_id,
                intrinsics: self.intrinsics.clone(),
                function_prototype: self.function_prototype,
                global: self.global_template,
            });

        let global = realm.global;

        let mut ctx = VmContext::with_config(
            global,
            self.config.max_stack_depth,
            crate::context::DEFAULT_MAX_NATIVE_DEPTH,
            Arc::clone(&self.memory_manager),
        );
        ctx.set_function_prototype_intrinsic(realm.function_prototype);
        ctx.set_generator_prototype_intrinsic(realm.intrinsics.generator_prototype);
        ctx.set_async_generator_prototype_intrinsic(realm.intrinsics.async_generator_prototype);
        ctx.set_realm(realm.id, Arc::clone(&self.realm_registry));
        ctx
    }

    /// Get the intrinsic `%Function.prototype%` object.
    pub fn function_prototype(&self) -> GcRef<JsObject> {
        self.function_prototype
    }

    /// Get the intrinsics registry (all intrinsic objects and well-known symbols).
    pub fn intrinsics(&self) -> &Intrinsics {
        &self.intrinsics
    }

    /// Get the realm registry.
    pub fn realm_registry(&self) -> &Arc<RealmRegistry> {
        &self.realm_registry
    }

    /// Execute a module
    pub fn execute_module(&self, module: &Module) -> VmResult<Value> {
        let mut ctx = self.create_context();
        let interpreter = Interpreter::new();
        let result = interpreter.execute(module, &mut ctx);
        ctx.teardown();
        result
    }

    /// Execute a module with an existing context
    pub fn execute_module_with_context(
        &self,
        module: &Module,
        ctx: &mut VmContext,
    ) -> VmResult<Value> {
        self.execute_module_with_context_and_locals(module, ctx, None)
    }

    /// Execute a module with an existing context and initial local variables
    pub fn execute_module_with_context_and_locals(
        &self,
        module: &Module,
        ctx: &mut VmContext,
        initial_locals: Option<std::collections::HashMap<u16, Value>>,
    ) -> VmResult<Value> {
        let interpreter = Interpreter::new();
        interpreter.execute_arc_with_locals(Arc::new(module.clone()), ctx, initial_locals)
    }

    /// Get runtime configuration
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    /// Get number of loaded modules
    pub fn module_count(&self) -> usize {
        self.modules.lock().unwrap().len()
    }

    /// Get the memory manager for this runtime
    pub fn memory_manager(&self) -> &Arc<crate::memory::MemoryManager> {
        &self.memory_manager
    }

    /// Replace the default realm with a freshly created one.
    ///
    /// Creates a new realm (fresh intrinsics, global, Function.prototype),
    /// sets it as the default, removes the old realm from the registry
    /// (dropping GcRef roots so old objects can be GC'd), and clears the
    /// module cache.
    ///
    /// After this call, `create_context()` will use the new realm.
    /// Extensions are re-applied by Otter::eval() automatically.
    pub fn reset_default_realm(&mut self) {
        let old_realm_id = self.default_realm_id;

        // Create new realm (allocate → wire → init_core → install_on_global)
        let new_realm_id = self.create_realm();

        // Get new realm record to update cached fields
        let new_realm = self
            .realm_registry
            .get(new_realm_id)
            .expect("freshly created realm must exist in registry");

        // Swap default
        self.default_realm_id = new_realm_id;
        self.intrinsics = new_realm.intrinsics.clone();
        self.function_prototype = new_realm.function_prototype;
        self.global_template = new_realm.global;

        // Drop old realm roots → GC can collect old objects
        self.realm_registry.remove(old_realm_id);

        // Clear module cache (compiled modules may reference old realm state)
        self.modules.lock().unwrap().clear();
    }
}

impl Default for VmRuntime {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: VmRuntime uses thread-safe containers
unsafe impl Send for VmRuntime {}
unsafe impl Sync for VmRuntime {}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm_bytecode::operand::Register;
    use otter_vm_bytecode::{Function, Instruction};

    fn create_simple_module() -> Module {
        // Create a module that returns 42
        let mut builder = Module::builder("test.js");

        let func = Function::builder()
            .name("main")
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 42,
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        builder.add_function(func);
        builder.build()
    }

    #[test]
    fn test_runtime_creation() {
        let runtime = VmRuntime::new();
        assert_eq!(runtime.module_count(), 0);
    }

    #[test]
    fn test_load_module() {
        let runtime = VmRuntime::new();
        let module = create_simple_module();

        runtime.load_module(module);
        assert_eq!(runtime.module_count(), 1);
        assert!(runtime.get_module("test.js").is_some());
    }

    #[test]
    fn test_runtime_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<VmRuntime>();
    }
}
