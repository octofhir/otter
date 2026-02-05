use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use crate::gc::GcRef;
use crate::value::Symbol;

pub struct SymbolRegistry {
    map: Mutex<HashMap<String, GcRef<Symbol>>>,
}

impl SymbolRegistry {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, key: &str) -> Option<GcRef<Symbol>> {
        self.map.lock().get(key).cloned()
    }

    pub fn insert(&self, key: String, symbol: GcRef<Symbol>) {
        self.map.lock().insert(key, symbol);
    }

    pub fn key_for(&self, symbol: &GcRef<Symbol>) -> Option<String> {
        let map = self.map.lock();
        for (key, value) in map.iter() {
            if std::ptr::eq(value.as_ptr(), symbol.as_ptr()) {
                return Some(key.clone());
            }
        }
        None
    }

    pub fn trace_roots(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        let map = self.map.lock();
        for symbol in map.values() {
            tracer(symbol.header() as *const _);
        }
    }
}

static GLOBAL_SYMBOL_REGISTRY: OnceLock<Arc<SymbolRegistry>> = OnceLock::new();

pub fn global_symbol_registry() -> Arc<SymbolRegistry> {
    GLOBAL_SYMBOL_REGISTRY
        .get_or_init(|| Arc::new(SymbolRegistry::new()))
        .clone()
}
