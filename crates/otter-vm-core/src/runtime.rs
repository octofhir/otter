//! VM runtime - the main entry point
//!
//! The runtime manages module loading, context creation, and execution.

use dashmap::DashMap;
use std::sync::Arc;

use otter_vm_bytecode::Module;

use crate::context::VmContext;
use crate::error::VmResult;
use crate::globals;
use crate::interpreter::Interpreter;
use crate::object::JsObject;
use crate::value::Value;

/// The VM runtime
///
/// This is the main entry point for executing JavaScript.
/// It is `Send + Sync` and can be shared across threads.
pub struct VmRuntime {
    /// Loaded modules
    modules: DashMap<String, Arc<Module>>,
    /// Global object template
    #[allow(dead_code)]
    global_template: Arc<JsObject>,
    /// Runtime configuration
    config: RuntimeConfig,
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
        let global = Arc::new(JsObject::new(None));
        globals::setup_global_object(&global);

        Self {
            modules: DashMap::new(),
            global_template: global,
            config,
        }
    }

    /// Load a module from bytecode
    pub fn load_module(&self, module: Module) -> Arc<Module> {
        let url = module.source_url.clone();
        let module = Arc::new(module);
        self.modules.insert(url, module.clone());
        module
    }

    /// Get a loaded module by URL
    pub fn get_module(&self, url: &str) -> Option<Arc<Module>> {
        self.modules.get(url).map(|m| m.clone())
    }

    /// Create a new execution context
    pub fn create_context(&self) -> VmContext {
        // Clone global object for isolation
        // TODO: Proper cloning with prototype chain
        let global = Arc::new(JsObject::new(None));
        globals::setup_global_object(&global);
        VmContext::new(global)
    }

    /// Execute a module
    pub fn execute_module(&self, module: &Module) -> VmResult<Value> {
        let mut ctx = self.create_context();
        let mut interpreter = Interpreter::new();

        interpreter.execute(module, &mut ctx)
    }

    /// Execute a module with an existing context
    pub fn execute_module_with_context(
        &self,
        module: &Module,
        ctx: &mut VmContext,
    ) -> VmResult<Value> {
        let mut interpreter = Interpreter::new();
        interpreter.execute(module, ctx)
    }

    /// Get runtime configuration
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    /// Get number of loaded modules
    pub fn module_count(&self) -> usize {
        self.modules.len()
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
