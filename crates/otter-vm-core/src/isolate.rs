//! JavaScript Isolate — an independent VM execution environment.
//!
//! An `Isolate` encapsulates all per-VM state: runtime, context, memory manager,
//! GC registry, and string table. It is the unit of thread confinement.
//!
//! # Thread Safety Model (V8/Deno pattern)
//!
//! - **`Isolate` is `Send` but NOT `Sync`**: Can be moved between threads,
//!   but only one thread may access it at a time.
//! - **`IsolateHandle` is `Send + Sync`**: A lightweight handle for cross-thread
//!   operations (interrupt, terminate). Contains only atomics.
//! - **`IsolateGuard`**: RAII guard returned by `enter()`. Sets up thread-locals
//!   and tears them down on drop.
//!
//! # Usage
//!
//! ```ignore
//! let mut isolate = Isolate::new(IsolateConfig::default());
//!
//! // Enter the isolate on the current thread
//! {
//!     let guard = isolate.enter();
//!     // ... execute JS via guard.runtime() / guard.context() ...
//! } // guard drops → thread-locals cleared
//!
//! // Move isolate to another thread
//! std::thread::spawn(move || {
//!     let guard = isolate.enter();
//!     // ... execute on new thread ...
//! });
//! ```
//!
//! # Cross-Thread Operations
//!
//! ```ignore
//! let handle = isolate.handle();
//! // From any thread:
//! handle.interrupt();  // cooperative interrupt
//! handle.terminate();  // request termination
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::context::VmContext;
use crate::memory::MemoryManager;
use crate::runtime::{RuntimeConfig, VmRuntime};
use crate::string;

/// Configuration for creating a new Isolate.
#[derive(Debug, Clone)]
pub struct IsolateConfig {
    /// Maximum call stack depth
    pub max_stack_depth: usize,
    /// Maximum native call depth
    pub max_native_depth: usize,
    /// Maximum heap size in bytes
    pub max_heap_size: usize,
    /// Enable strict mode by default
    pub strict_mode: bool,
}

impl Default for IsolateConfig {
    fn default() -> Self {
        Self {
            max_stack_depth: 10_000,
            max_native_depth: 256,
            max_heap_size: 512 * 1024 * 1024, // 512 MB
            strict_mode: true,
        }
    }
}

/// A complete JavaScript isolate.
///
/// Encapsulates all per-VM state: runtime, context, memory manager, and
/// associated thread-locals. Only one thread may access an Isolate at a time
/// (enforced by `&mut self` on `enter()`).
///
/// # Send but not Sync
///
/// `Isolate` is `Send` (can be moved between threads) but NOT `Sync`
/// (cannot be shared between threads simultaneously). This matches the
/// V8 Locker/Unlocker pattern via Rust ownership.
pub struct Isolate {
    /// The VM execution context (registers, call stack, locals).
    /// MUST be declared before `runtime` — Rust drops fields in declaration
    /// order, and context teardown uses the GC registry owned by runtime.
    context: VmContext,
    /// The VM runtime (module cache, intrinsics, GC registry).
    /// Dropped AFTER context, so the registry is still alive during teardown.
    runtime: VmRuntime,
    /// Memory manager for this isolate
    memory_manager: Arc<MemoryManager>,
    /// Thread-safe handle for cross-thread operations
    handle: IsolateHandle,
    /// Whether the isolate is currently entered (on some thread)
    entered: bool,
}

// SAFETY: Isolate can be moved between threads. All interior types that use
// RefCell/UnsafeCell are protected by the invariant that only one thread
// accesses the Isolate at a time (enforced by `&mut self` on `enter()`).
// The non-Sync types (RefCell, Cell, UnsafeCell) inside JsObject, Shape,
// JsGenerator etc. are safe because they are never shared between threads —
// the Isolate is moved, not shared.
unsafe impl Send for Isolate {}
// Isolate is intentionally NOT Sync.

impl Isolate {
    /// Create a new isolate with the given configuration.
    ///
    /// The isolate is not entered — call `enter()` before executing JS.
    pub fn new(config: IsolateConfig) -> Self {
        let runtime_config = RuntimeConfig {
            max_stack_depth: config.max_stack_depth,
            max_heap_size: config.max_heap_size,
            strict_mode: config.strict_mode,
        };

        // VmRuntime::with_config creates its own GC registry + MemoryManager
        // and sets thread-locals during construction.
        let runtime = VmRuntime::with_config(runtime_config);
        let memory_manager = runtime.memory_manager().clone();

        // Create context via the runtime (sets up intrinsics, realm, etc.)
        let context = runtime.create_context();

        // Create IsolateHandle with shared interrupt flag from context
        let interrupt_flag = context.interrupt_flag();
        let handle = IsolateHandle {
            interrupt_flag,
            terminated: Arc::new(AtomicBool::new(false)),
        };

        // Clear thread-locals — will be re-set on enter()
        MemoryManager::clear_thread_default();
        otter_vm_gc::clear_thread_registry();

        Self {
            runtime,
            context,
            memory_manager,
            handle,
            entered: false,
        }
    }

    /// Enter the isolate on the current thread.
    ///
    /// Sets up thread-local state and returns an `IsolateGuard` that clears
    /// it on drop.
    ///
    /// # Thread-Local State Managed
    ///
    /// | Thread-Local | Set on Enter | Cleared on Exit |
    /// |---|---|---|
    /// | `THREAD_MEMORY_MANAGER` | Yes | Yes |
    /// | `STRING_TABLE` | Auto-init per thread | No (cached) |
    /// | `THREAD_REGISTRY` | Auto-init per thread | No (leaked static) |
    /// | `CAPABILITIES` | Managed by Otter runtime | Managed by Otter runtime |
    /// | `WRITE_BARRIER_BUF` | Auto-init per thread | Transient |
    /// | `GC_DEALLOC_IN_PROGRESS` | Auto-init per thread | Transient |
    /// | Well-known strings | Lazy-init per thread | No (cached) |
    ///
    /// # Panics
    ///
    /// Panics if the isolate is already entered. An isolate can only be
    /// entered on one thread at a time.
    pub fn enter(&mut self) -> IsolateGuard<'_> {
        assert!(
            !self.entered,
            "Isolate::enter() called while already entered"
        );
        self.entered = true;

        // Set thread-local memory manager for GcRef::new() allocation tracking
        MemoryManager::set_thread_default(self.memory_manager.clone());

        // Set thread-local GC registry so gc_alloc() uses this runtime's registry
        // SAFETY: pointer remains valid — VmRuntime owns the Box and outlives the guard
        unsafe { otter_vm_gc::set_thread_registry(self.runtime.gc_registry()) };

        // Note: STRING_TABLE is per-thread and auto-initialized (lazy cache).
        // Well-known strings are also lazy per-thread.
        // These work for "one isolate per thread". For future thread migration
        // (Step B), they would need to be moved into per-Isolate state.

        IsolateGuard { isolate: self }
    }

    /// Get a thread-safe handle for cross-thread operations.
    ///
    /// The handle can interrupt or terminate the isolate from any thread.
    pub fn handle(&self) -> IsolateHandle {
        self.handle.clone()
    }

    /// Get the memory manager for this isolate.
    pub fn memory_manager(&self) -> &Arc<MemoryManager> {
        &self.memory_manager
    }

    /// Access the runtime (module cache, intrinsics).
    pub fn runtime(&self) -> &VmRuntime {
        &self.runtime
    }

    /// Access the runtime mutably.
    pub fn runtime_mut(&mut self) -> &mut VmRuntime {
        &mut self.runtime
    }

    /// Access the execution context.
    pub fn context(&self) -> &VmContext {
        &self.context
    }

    /// Access the execution context mutably.
    pub fn context_mut(&mut self) -> &mut VmContext {
        &mut self.context
    }
}

impl Drop for Isolate {
    fn drop(&mut self) {
        if self.entered {
            string::clear_global_string_table();
            self.entered = false;
        }
        // Fields drop in declaration order:
        //   1. context (no Drop impl, just freed)
        //   2. runtime (VmRuntime::drop: dealloc_all, clear thread-locals)
        //   3. memory_manager, handle, entered
    }
}

/// RAII guard for an entered isolate.
///
/// While this guard exists, thread-local state is set up for the isolate.
/// When dropped, thread-locals are cleared, allowing the isolate to be
/// moved to another thread.
pub struct IsolateGuard<'a> {
    isolate: &'a mut Isolate,
}

impl<'a> IsolateGuard<'a> {
    /// Access the VM runtime.
    pub fn runtime(&self) -> &VmRuntime {
        &self.isolate.runtime
    }

    /// Access the VM runtime mutably.
    pub fn runtime_mut(&mut self) -> &mut VmRuntime {
        &mut self.isolate.runtime
    }

    /// Access the execution context.
    pub fn context(&self) -> &VmContext {
        &self.isolate.context
    }

    /// Access the execution context mutably.
    pub fn context_mut(&mut self) -> &mut VmContext {
        &mut self.isolate.context
    }

    /// Access the memory manager.
    pub fn memory_manager(&self) -> &Arc<MemoryManager> {
        &self.isolate.memory_manager
    }

    /// Access the GC allocation registry.
    pub fn gc_registry(&self) -> &otter_vm_gc::AllocationRegistry {
        self.isolate.runtime.gc_registry()
    }
}

impl Drop for IsolateGuard<'_> {
    fn drop(&mut self) {
        // Clear thread-local state
        MemoryManager::clear_thread_default();
        otter_vm_gc::clear_thread_registry();

        self.isolate.entered = false;
    }
}

/// Thread-safe handle to an Isolate.
///
/// Can be used from any thread to interrupt or terminate the isolate.
/// Contains only atomic fields, so it is `Send + Sync`.
#[derive(Clone)]
pub struct IsolateHandle {
    /// Shared interrupt flag (also held by VmContext)
    interrupt_flag: Arc<AtomicBool>,
    /// Termination request flag
    terminated: Arc<AtomicBool>,
}

// IsolateHandle is naturally Send + Sync (only contains Arc<AtomicBool>)

impl IsolateHandle {
    /// Request a cooperative interrupt.
    ///
    /// The VM will check this flag at regular intervals and yield control.
    /// This is non-blocking — the interrupt will be serviced at the next
    /// safepoint (typically every ~10,000 instructions).
    pub fn interrupt(&self) {
        self.interrupt_flag.store(true, Ordering::Release);
    }

    /// Request termination of the isolate.
    ///
    /// This sets both the interrupt flag and the termination flag.
    /// The VM will stop execution at the next safepoint.
    pub fn terminate(&self) {
        self.terminated.store(true, Ordering::Release);
        self.interrupt_flag.store(true, Ordering::Release);
    }

    /// Check if termination has been requested.
    pub fn is_terminated(&self) -> bool {
        self.terminated.load(Ordering::Acquire)
    }

    /// Check if an interrupt has been requested.
    pub fn is_interrupted(&self) -> bool {
        self.interrupt_flag.load(Ordering::Acquire)
    }

    /// Clear the interrupt flag (called after handling the interrupt).
    pub fn clear_interrupt(&self) {
        self.interrupt_flag.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_isolate_create_and_enter() {
        let mut isolate = Isolate::new(IsolateConfig::default());

        // Enter isolate
        {
            let _guard = isolate.enter();
            // Memory manager should be set
            assert!(MemoryManager::current().is_some());
        }

        // After guard drop, memory manager should be cleared
        assert!(MemoryManager::current().is_none());
    }

    #[test]
    fn test_isolate_handle_interrupt() {
        let isolate = Isolate::new(IsolateConfig::default());
        let handle = isolate.handle();

        assert!(!handle.is_interrupted());

        handle.interrupt();
        assert!(handle.is_interrupted());

        handle.clear_interrupt();
        assert!(!handle.is_interrupted());
    }

    #[test]
    fn test_isolate_handle_terminate() {
        let isolate = Isolate::new(IsolateConfig::default());
        let handle = isolate.handle();

        assert!(!handle.is_terminated());

        handle.terminate();
        assert!(handle.is_terminated());
        assert!(handle.is_interrupted()); // terminate also sets interrupt
    }

    #[test]
    fn test_isolate_send_between_threads() {
        let mut isolate = Isolate::new(IsolateConfig::default());

        // Enter and exit on main thread
        {
            let _guard = isolate.enter();
        }

        // Move to another thread
        let handle = std::thread::spawn(move || {
            let _guard = isolate.enter();
            assert!(MemoryManager::current().is_some());
            // isolate drops here, entered=true, so drop clears thread-locals
        });

        handle.join().unwrap();
    }

    #[test]
    fn test_enter_exit_reenter() {
        let mut isolate = Isolate::new(IsolateConfig::default());

        // Enter, do work, exit
        {
            let _guard = isolate.enter();
            assert!(MemoryManager::current().is_some());
        }

        // Re-enter after exit — should work
        {
            let _guard = isolate.enter();
            assert!(MemoryManager::current().is_some());
        }

        assert!(MemoryManager::current().is_none());
    }

    #[test]
    fn test_isolate_handle_clone_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<IsolateHandle>();
    }

    #[test]
    fn test_multi_isolate_parallel() {
        // Two isolates on two threads, both running simultaneously
        let isolate1 = Isolate::new(IsolateConfig::default());
        let isolate2 = Isolate::new(IsolateConfig::default());

        let h1 = std::thread::spawn(move || {
            let mut iso = isolate1;
            let guard = iso.enter();
            // Verify MM is set for this thread
            assert!(MemoryManager::current().is_some());
            // Access context/runtime
            let _global = guard.context().global();
            drop(guard);
        });

        let h2 = std::thread::spawn(move || {
            let mut iso = isolate2;
            let guard = iso.enter();
            assert!(MemoryManager::current().is_some());
            let _global = guard.context().global();
            drop(guard);
        });

        h1.join().unwrap();
        h2.join().unwrap();
    }

    #[test]
    fn test_isolate_lifecycle_stress() {
        // Rapid create/destroy cycles — tests for resource leaks
        for _ in 0..20 {
            let mut isolate = Isolate::new(IsolateConfig {
                max_heap_size: 16 * 1024 * 1024, // 16MB (smaller for stress)
                ..IsolateConfig::default()
            });
            {
                let _guard = isolate.enter();
                assert!(MemoryManager::current().is_some());
            }
            assert!(MemoryManager::current().is_none());
            // isolate drops here
        }
    }

    #[test]
    fn test_isolate_execute_js() {
        use crate::interpreter::Interpreter;
        use otter_vm_bytecode::{Function, Instruction, Module, Register};

        let mut isolate = Isolate::new(IsolateConfig::default());
        let mut guard = isolate.enter();

        // Create a simple module that returns 42
        let mut builder = Module::builder("test_isolate.js");
        let func = Function::builder()
            .name("main")
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 42,
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();
        builder.add_function(func);
        let module = builder.build();

        let interpreter = Interpreter::new();
        let result = interpreter.execute(&module, guard.context_mut());
        assert!(result.is_ok());
        assert_eq!(result.unwrap().as_int32(), Some(42));
    }

    #[test]
    fn test_isolate_interrupt_from_other_thread() {
        let mut isolate = Isolate::new(IsolateConfig::default());
        let handle = isolate.handle();

        let interrupt_thread = std::thread::spawn(move || {
            // Small delay then interrupt
            std::thread::sleep(std::time::Duration::from_millis(10));
            handle.interrupt();
        });

        {
            let guard = isolate.enter();
            // Wait for interrupt
            interrupt_thread.join().unwrap();
            // The context should see the interrupt
            assert!(guard.context().is_interrupted());
        }
    }

    #[test]
    fn test_isolate_is_send_not_sync() {
        fn assert_send<T: Send>() {}
        assert_send::<Isolate>();

        // Isolate should NOT be Sync — this is enforced by not implementing Sync.
        // We can't have a compile-time negative test, but we document the intent.
    }
}
