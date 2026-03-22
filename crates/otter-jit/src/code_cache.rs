//! Code cache — stores compiled functions keyed by function identity.
//!
//! The cache is thread-local (single-threaded VM). Functions are keyed
//! by their raw pointer address, which is stable for the function's lifetime.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::code_memory::CompiledFunction;

/// Key for the code cache: raw pointer to the bytecode Function.
/// This is stable because Functions are Arc-held by their Module.
type CacheKey = usize;

/// Thread-local code cache.
struct CodeCache {
    entries: HashMap<CacheKey, CompiledFunction>,
}

thread_local! {
    static CACHE: RefCell<CodeCache> = RefCell::new(CodeCache {
        entries: HashMap::new(),
    });
}

/// Look up a compiled function by its bytecode Function pointer.
pub fn get(func_ptr: *const otter_vm_bytecode::Function) -> Option<*const u8> {
    CACHE.with(|cache| {
        cache
            .borrow()
            .entries
            .get(&(func_ptr as usize))
            .map(|cf| cf.entry)
    })
}

/// Store a compiled function in the cache.
pub fn insert(func_ptr: *const otter_vm_bytecode::Function, compiled: CompiledFunction) {
    CACHE.with(|cache| {
        cache
            .borrow_mut()
            .entries
            .insert(func_ptr as usize, compiled);
    });
}

/// Check if a function is already compiled.
pub fn contains(func_ptr: *const otter_vm_bytecode::Function) -> bool {
    CACHE.with(|cache| cache.borrow().entries.contains_key(&(func_ptr as usize)))
}

/// Number of compiled functions in the cache.
pub fn len() -> usize {
    CACHE.with(|cache| cache.borrow().entries.len())
}

/// Clear the entire cache (e.g., on prototype chain mutation).
pub fn clear() {
    CACHE.with(|cache| cache.borrow_mut().entries.clear());
}

/// Remove a single function from the cache.
pub fn invalidate(func_ptr: *const otter_vm_bytecode::Function) {
    CACHE.with(|cache| {
        cache.borrow_mut().entries.remove(&(func_ptr as usize));
    });
}
