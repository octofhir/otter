use std::cell::RefCell;
use std::collections::HashMap;

use crate::gc::GcRef;
use crate::value::Symbol;

/// Per-isolate symbol registry for Symbol.for() / Symbol.keyFor()
///
/// Each VmContext owns its own registry. Uses RefCell since all access
/// is from the single VM thread.
pub struct SymbolRegistry {
    map: RefCell<HashMap<String, GcRef<Symbol>>>,
}

// SAFETY: SymbolRegistry is only accessed from the single VM thread.
// Thread confinement is enforced by the Isolate abstraction.
unsafe impl Send for SymbolRegistry {}
unsafe impl Sync for SymbolRegistry {}

impl SymbolRegistry {
    pub fn new() -> Self {
        Self {
            map: RefCell::new(HashMap::new()),
        }
    }

    pub fn get(&self, key: &str) -> Option<GcRef<Symbol>> {
        self.map.borrow().get(key).cloned()
    }

    pub fn insert(&self, key: String, symbol: GcRef<Symbol>) {
        self.map.borrow_mut().insert(key, symbol);
    }

    pub fn key_for(&self, symbol: &GcRef<Symbol>) -> Option<String> {
        let map = self.map.borrow();
        for (key, value) in map.iter() {
            if std::ptr::eq(value.as_ptr(), symbol.as_ptr()) {
                return Some(key.clone());
            }
        }
        None
    }

    pub fn trace_roots(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        let map = self.map.borrow();
        for symbol in map.values() {
            tracer(symbol.header() as *const _);
        }
    }
}

impl Default for SymbolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
