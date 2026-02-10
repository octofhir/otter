//! Symbol built-in
//!
//! Provides Symbol primitive and well-known symbols:
//! - Symbol() - creates unique symbols
//! - Symbol.for() - global symbol registry
//! - Symbol.keyFor() - get key from global symbol
//! - Well-known symbols: iterator, asyncIterator, toStringTag, hasInstance, toPrimitive

use otter_vm_core::gc::GcRef;
use otter_vm_core::string::JsString;
use otter_vm_core::value::{Symbol, Value};
use otter_vm_core::{VmError, memory};
use otter_vm_runtime::{Op, op_native_with_mm as op_native};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

/// Global symbol ID counter
static SYMBOL_ID_COUNTER: AtomicU64 = AtomicU64::new(1000); // Start after well-known symbols

/// Global symbol registry for Symbol.for() / Symbol.keyFor()
static GLOBAL_REGISTRY: OnceLock<RwLock<HashMap<String, GcRef<Symbol>>>> = OnceLock::new();

/// Well-known symbol IDs (fixed, pre-defined)
pub mod well_known {
    pub const ITERATOR: u64 = 1;
    pub const ASYNC_ITERATOR: u64 = 2;
    pub const TO_STRING_TAG: u64 = 3;
    pub const HAS_INSTANCE: u64 = 4;
    pub const TO_PRIMITIVE: u64 = 5;
    pub const IS_CONCAT_SPREADABLE: u64 = 6;
    pub const MATCH: u64 = 7;
    pub const MATCH_ALL: u64 = 8;
    pub const REPLACE: u64 = 9;
    pub const SEARCH: u64 = 10;
    pub const SPLIT: u64 = 11;
    pub const SPECIES: u64 = 12;
    pub const UNSCOPABLES: u64 = 13;
}

/// Get next unique symbol ID
fn next_symbol_id() -> u64 {
    SYMBOL_ID_COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// Get or initialize the global registry
fn global_registry() -> &'static RwLock<HashMap<String, GcRef<Symbol>>> {
    GLOBAL_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Get Symbol ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Symbol_create", symbol_create),
        op_native("__Symbol_for", symbol_for),
        op_native("__Symbol_keyFor", symbol_key_for),
        op_native("__Symbol_toString", symbol_to_string),
        op_native("__Symbol_valueOf", symbol_value_of),
        op_native("__Symbol_description", symbol_description),
        op_native("__Symbol_getWellKnown", symbol_get_well_known),
        op_native("__Symbol_getId", symbol_get_id),
    ]
}

// =============================================================================
// Symbol methods
// =============================================================================

/// Create a new unique symbol with optional description
/// Args: [description: string | undefined]
fn symbol_create(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let description = args.first().and_then(|v| {
        if v.is_undefined() {
            None
        } else {
            v.as_string().map(|s| s.as_str().to_string())
        }
    });

    let sym = GcRef::new(Symbol {
        description,
        id: next_symbol_id(),
    });

    Ok(Value::symbol(sym))
}

/// Symbol.for(key) - get or create global symbol
/// Args: [key: string]
fn symbol_for(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let key = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.as_str().to_string())
        .ok_or("Symbol.for requires a string key")?;

    let registry = global_registry();

    // Try to get existing symbol
    {
        let read = registry.read().unwrap();
        if let Some(sym) = read.get(&key) {
            return Ok(Value::symbol(*sym));
        }
    }

    // Create new symbol and register it
    let sym = GcRef::new(Symbol {
        description: Some(key.clone()),
        id: next_symbol_id(),
    });

    {
        let mut write = registry.write().unwrap();
        // Double-check in case another thread added it
        if let Some(existing) = write.get(&key) {
            return Ok(Value::symbol(*existing));
        }
        write.insert(key, sym);
    }

    Ok(Value::symbol(sym))
}

/// Symbol.keyFor(symbol) - get key for global symbol
/// Args: [symbol: symbol]
fn symbol_key_for(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let sym = args
        .first()
        .and_then(|v| v.as_symbol())
        .ok_or("Symbol.keyFor requires a symbol")?;

    let registry = global_registry();
    let read = registry.read().unwrap();

    for (key, registered_sym) in read.iter() {
        if registered_sym.id == sym.id {
            return Ok(Value::string(JsString::intern(key)));
        }
    }

    // Symbol is not in global registry
    Ok(Value::undefined())
}

/// Symbol.prototype.toString() - returns "Symbol(description)"
fn symbol_to_string(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let sym = args
        .first()
        .and_then(|v| v.as_symbol())
        .ok_or("Symbol.toString requires a symbol")?;

    let result = if let Some(desc) = &sym.description {
        format!("Symbol({})", desc)
    } else {
        "Symbol()".to_string()
    };

    Ok(Value::string(JsString::intern(&result)))
}

/// Symbol.prototype.valueOf() - returns the symbol itself
fn symbol_value_of(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let val = args.first().ok_or("Symbol.valueOf requires a symbol")?;

    if val.is_symbol() {
        Ok(val.clone())
    } else {
        Err(VmError::type_error("Symbol.valueOf requires a symbol"))
    }
}

/// Symbol.prototype.description getter
fn symbol_description(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let sym = args
        .first()
        .and_then(|v| v.as_symbol())
        .ok_or("Symbol.description requires a symbol")?;

    match &sym.description {
        Some(desc) => Ok(Value::string(JsString::intern(desc))),
        None => Ok(Value::undefined()),
    }
}

/// Get well-known symbol by name
/// Args: [name: string]
fn symbol_get_well_known(
    args: &[Value],
    _mm: Arc<memory::MemoryManager>,
) -> Result<Value, VmError> {
    let name_js = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or("Symbol.getWellKnown requires a string name")?;
    let name = name_js.as_str();

    let (id, desc) = match name {
        "iterator" => (well_known::ITERATOR, "Symbol.iterator"),
        "asyncIterator" => (well_known::ASYNC_ITERATOR, "Symbol.asyncIterator"),
        "toStringTag" => (well_known::TO_STRING_TAG, "Symbol.toStringTag"),
        "hasInstance" => (well_known::HAS_INSTANCE, "Symbol.hasInstance"),
        "toPrimitive" => (well_known::TO_PRIMITIVE, "Symbol.toPrimitive"),
        "isConcatSpreadable" => (
            well_known::IS_CONCAT_SPREADABLE,
            "Symbol.isConcatSpreadable",
        ),
        "match" => (well_known::MATCH, "Symbol.match"),
        "matchAll" => (well_known::MATCH_ALL, "Symbol.matchAll"),
        "replace" => (well_known::REPLACE, "Symbol.replace"),
        "search" => (well_known::SEARCH, "Symbol.search"),
        "split" => (well_known::SPLIT, "Symbol.split"),
        "species" => (well_known::SPECIES, "Symbol.species"),
        "unscopables" => (well_known::UNSCOPABLES, "Symbol.unscopables"),
        _ => {
            return Err(VmError::type_error(format!(
                "Unknown well-known symbol: {}",
                name
            )));
        }
    };

    let sym = GcRef::new(Symbol {
        description: Some(desc.to_string()),
        id,
    });

    Ok(Value::symbol(sym))
}

/// Get symbol ID (for internal use, e.g., property key conversion)
fn symbol_get_id(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, VmError> {
    let sym = args
        .first()
        .and_then(|v| v.as_symbol())
        .ok_or("Symbol.getId requires a symbol")?;

    // Return as number (f64 can represent u64 up to 2^53 safely)
    Ok(Value::number(sym.id as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn str_val(s: &str) -> Value {
        Value::string(JsString::intern(s))
    }

    #[test]
    fn test_symbol_create() {
        let mm = Arc::new(memory::MemoryManager::test());
        let result = symbol_create(&[str_val("test")], mm).unwrap();
        assert!(result.is_symbol());

        let sym = result.as_symbol().unwrap();
        assert_eq!(sym.description.as_deref(), Some("test"));
    }

    #[test]
    fn test_symbol_create_no_description() {
        let mm = Arc::new(memory::MemoryManager::test());
        let result = symbol_create(&[Value::undefined()], mm).unwrap();
        assert!(result.is_symbol());

        let sym = result.as_symbol().unwrap();
        assert!(sym.description.is_none());
    }

    #[test]
    fn test_symbol_uniqueness() {
        let mm = Arc::new(memory::MemoryManager::test());
        let sym1 = symbol_create(&[str_val("test")], mm.clone()).unwrap();
        let sym2 = symbol_create(&[str_val("test")], mm).unwrap();

        let id1 = sym1.as_symbol().unwrap().id;
        let id2 = sym2.as_symbol().unwrap().id;

        assert_ne!(
            id1, id2,
            "Two Symbol() calls should create different symbols"
        );
    }

    #[test]
    fn test_symbol_for_creates_and_reuses() {
        let mm = Arc::new(memory::MemoryManager::test());
        let key = "my.global.symbol";
        let sym1 = symbol_for(&[str_val(key)], mm.clone()).unwrap();
        let sym2 = symbol_for(&[str_val(key)], mm).unwrap();

        let id1 = sym1.as_symbol().unwrap().id;
        let id2 = sym2.as_symbol().unwrap().id;

        assert_eq!(
            id1, id2,
            "Symbol.for should return same symbol for same key"
        );
    }

    #[test]
    fn test_symbol_key_for() {
        let mm = Arc::new(memory::MemoryManager::test());
        let key = "test.key.for";
        let sym = symbol_for(&[str_val(key)], mm.clone()).unwrap();

        let result = symbol_key_for(&[sym], mm).unwrap();
        let found_key = result.as_string().unwrap();
        assert_eq!(found_key.as_str(), key);
    }

    #[test]
    fn test_symbol_key_for_non_global() {
        let mm = Arc::new(memory::MemoryManager::test());
        let sym = symbol_create(&[str_val("local")], mm.clone()).unwrap();
        let result = symbol_key_for(&[sym], mm).unwrap();
        assert!(result.is_undefined());
    }

    #[test]
    fn test_symbol_to_string() {
        let mm = Arc::new(memory::MemoryManager::test());
        let sym = symbol_create(&[str_val("mySymbol")], mm.clone()).unwrap();
        let result = symbol_to_string(&[sym], mm).unwrap();
        let s = result.as_string().unwrap();
        assert_eq!(s.as_str(), "Symbol(mySymbol)");
    }

    #[test]
    fn test_symbol_to_string_no_description() {
        let mm = Arc::new(memory::MemoryManager::test());
        let sym = symbol_create(&[Value::undefined()], mm.clone()).unwrap();
        let result = symbol_to_string(&[sym], mm).unwrap();
        let s = result.as_string().unwrap();
        assert_eq!(s.as_str(), "Symbol()");
    }

    #[test]
    fn test_symbol_value_of() {
        let mm = Arc::new(memory::MemoryManager::test());
        let sym = symbol_create(&[str_val("test")], mm.clone()).unwrap();
        let result = symbol_value_of(std::slice::from_ref(&sym), mm).unwrap();

        // Same symbol instance
        let orig_id = sym.as_symbol().unwrap().id;
        let result_id = result.as_symbol().unwrap().id;
        assert_eq!(orig_id, result_id);
    }

    #[test]
    fn test_symbol_description() {
        let mm = Arc::new(memory::MemoryManager::test());
        let sym = symbol_create(&[str_val("desc")], mm.clone()).unwrap();
        let result = symbol_description(&[sym], mm).unwrap();
        let s = result.as_string().unwrap();
        assert_eq!(s.as_str(), "desc");
    }

    #[test]
    fn test_symbol_get_well_known() {
        let mm = Arc::new(memory::MemoryManager::test());
        let sym = symbol_get_well_known(&[str_val("iterator")], mm).unwrap();
        assert!(sym.is_symbol());

        let sym_ref = sym.as_symbol().unwrap();
        assert_eq!(sym_ref.id, well_known::ITERATOR);
        assert_eq!(sym_ref.description.as_deref(), Some("Symbol.iterator"));
    }

    #[test]
    fn test_well_known_symbols_have_fixed_ids() {
        let mm = Arc::new(memory::MemoryManager::test());
        // Get the same well-known symbol twice
        let sym1 = symbol_get_well_known(&[str_val("iterator")], mm.clone()).unwrap();
        let sym2 = symbol_get_well_known(&[str_val("iterator")], mm).unwrap();

        let id1 = sym1.as_symbol().unwrap().id;
        let id2 = sym2.as_symbol().unwrap().id;

        assert_eq!(id1, id2, "Well-known symbols should have same fixed ID");
        assert_eq!(id1, well_known::ITERATOR);
    }

    #[test]
    fn test_symbol_get_id() {
        let mm = Arc::new(memory::MemoryManager::test());
        let sym = symbol_create(&[str_val("test")], mm.clone()).unwrap();
        let result = symbol_get_id(std::slice::from_ref(&sym), mm).unwrap();

        let expected_id = sym.as_symbol().unwrap().id;
        let actual_id = result.as_number().unwrap() as u64;
        assert_eq!(actual_id, expected_id);
    }
}
