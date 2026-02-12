//! Structured Clone Algorithm
//!
//! Implements the structured clone algorithm for transferring values between workers.
//! See: https://html.spec.whatwg.org/multipage/structured-data.html
//!
//! Key features:
//! - Handles circular references
//! - Preserves object identity within a clone operation
//! - Supports SharedArrayBuffer transfer (shares, not copies)
//! - Throws on non-cloneable values (functions, symbols, etc.)

use crate::gc::GcRef;
use crate::object::{JsObject, PropertyKey};
use crate::{JsDataView, JsTypedArray};
use crate::value::{HeapRef, Value};
use rustc_hash::FxHashMap;
use std::sync::Arc;

// Re-export for tests
#[cfg(test)]
use crate::shared_buffer::SharedArrayBuffer;
#[cfg(test)]
use crate::string::JsString;

/// Error during structured clone
#[derive(Debug, Clone)]
pub enum StructuredCloneError {
    /// Value cannot be cloned (functions, symbols, etc.)
    NotCloneable(&'static str),
    /// Circular reference detected (internal, should be handled)
    CircularReference,
    /// Data Clone Error (e.g. detached buffer)
    DataCloneError(&'static str),
}

impl std::fmt::Display for StructuredCloneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotCloneable(typ) => write!(f, "Cannot clone {}", typ),
            Self::CircularReference => write!(f, "Circular reference detected"),
            Self::DataCloneError(msg) => write!(f, "DataCloneError: {}", msg),
        }
    }
}

impl std::error::Error for StructuredCloneError {}

/// Performs the structured clone algorithm
pub struct StructuredCloner {
    /// Map from source pointer to cloned value (for circular reference handling)
    memory: FxHashMap<usize, Value>,
    memory_manager: Arc<crate::memory::MemoryManager>,
}

impl StructuredCloner {
    /// Create a new cloner
    pub fn new(memory_manager: Arc<crate::memory::MemoryManager>) -> Self {
        Self {
            memory: FxHashMap::default(),
            memory_manager,
        }
    }

    /// Clone a value using the structured clone algorithm
    pub fn clone(&mut self, value: &Value) -> Result<Value, StructuredCloneError> {
        self.internal_clone(value)
    }

    fn internal_clone(&mut self, value: &Value) -> Result<Value, StructuredCloneError> {
        // Handle primitives (these are copied directly)
        if value.is_undefined() {
            return Ok(Value::undefined());
        }
        if value.is_null() {
            return Ok(Value::null());
        }
        if let Some(b) = value.as_boolean() {
            return Ok(Value::boolean(b));
        }
        if let Some(n) = value.as_number() {
            return Ok(Value::number(n));
        }

        // Handle heap-allocated types
        match value.heap_ref() {
            Some(HeapRef::String(s)) => {
                // Strings are immutable, can share the GcRef (it's Copy)
                Ok(Value::string(*s))
            }

            Some(HeapRef::SharedArrayBuffer(sab)) => {
                // SharedArrayBuffer: share the same underlying buffer (not cloned!)
                Ok(Value::shared_array_buffer(*sab))
            }

            Some(HeapRef::Object(obj)) => self.clone_object(*obj),

            Some(HeapRef::Array(arr)) => self.clone_array(*arr),

            Some(HeapRef::Function(_)) => Err(StructuredCloneError::NotCloneable("function")),

            Some(HeapRef::NativeFunction(_)) => Err(StructuredCloneError::NotCloneable("function")),

            Some(HeapRef::Symbol(_)) => Err(StructuredCloneError::NotCloneable("symbol")),

            Some(HeapRef::BigInt(bi)) => {
                // Clone BigInt
                Ok(Value::bigint(bi.value.clone()))
            }

            Some(HeapRef::Promise(_)) => Err(StructuredCloneError::NotCloneable("promise")),

            Some(HeapRef::Proxy(_)) => Err(StructuredCloneError::NotCloneable("proxy")),

            Some(HeapRef::RegExp(r)) => {
                // Clone RegExp
                // New object with same pattern/flags
                // NOTE: This does not restrictively clone all properties yet, just the basic regex part.
                // Improving strict spec compliance later if needed.
                let new_regex = GcRef::new(crate::regexp::JsRegExp::new(
                    r.pattern.clone(),
                    r.flags.clone(),
                    None,
                    self.memory_manager.clone(),
                ));
                Ok(Value::regex(new_regex))
            }

            Some(HeapRef::Generator(_)) => Err(StructuredCloneError::NotCloneable("generator")),
            Some(HeapRef::ArrayBuffer(ab)) => {
                let len = ab.byte_length();
                // Slice creates a copy
                if let Some(new_ab) = ab.slice(0, len) {
                    Ok(Value::array_buffer(GcRef::new(new_ab)))
                } else {
                    Err(StructuredCloneError::DataCloneError(
                        "ArrayBuffer is detached",
                    ))
                }
            }
            Some(HeapRef::TypedArray(ta)) => self.clone_typed_array(*ta),
            Some(HeapRef::DataView(dv)) => self.clone_data_view(*dv),

            Some(HeapRef::MapData(_)) => Err(StructuredCloneError::NotCloneable("MapData")),
            Some(HeapRef::SetData(_)) => Err(StructuredCloneError::NotCloneable("SetData")),
            Some(HeapRef::EphemeronTable(_)) => {
                Err(StructuredCloneError::NotCloneable("EphemeronTable"))
            }

            None => Ok(Value::undefined()),
        }
    }

    fn clone_object(&mut self, obj: GcRef<JsObject>) -> Result<Value, StructuredCloneError> {
        let ptr = obj.as_ptr() as usize;

        // Check for circular reference
        if let Some(cloned) = self.memory.get(&ptr) {
            return Ok(cloned.clone());
        }

        // Create new object
        let new_obj = GcRef::new(JsObject::new(Value::null(), self.memory_manager.clone()));
        let new_value = Value::object(new_obj);

        // Register before cloning properties (to handle circular refs)
        self.memory.insert(ptr, new_value.clone());

        // Clone all own properties
        for key in obj.own_keys() {
            if let Some(val) = obj.get(&key) {
                let cloned_val = self.internal_clone(&val)?;
                let _ = new_obj.set(key, cloned_val);
            }
        }

        Ok(new_value)
    }

    fn clone_array(&mut self, arr: GcRef<JsObject>) -> Result<Value, StructuredCloneError> {
        let ptr = arr.as_ptr() as usize;

        // Check for circular reference
        if let Some(cloned) = self.memory.get(&ptr) {
            return Ok(cloned.clone());
        }

        // Create new array
        let len = arr.array_length();
        let new_arr = GcRef::new(JsObject::array(len, self.memory_manager.clone()));
        let new_value = Value::array(new_arr);

        // Register before cloning elements
        self.memory.insert(ptr, new_value.clone());

        // Clone all elements
        for i in 0..len {
            let key = PropertyKey::Index(i as u32);
            if let Some(val) = arr.get(&key) {
                let cloned_val = self.internal_clone(&val)?;
                let _ = new_arr.set(key, cloned_val);
            }
        }

        Ok(new_value)
    }

    fn clone_typed_array(
        &mut self,
        ta: GcRef<JsTypedArray>,
    ) -> Result<Value, StructuredCloneError> {
        let ptr = ta.as_ptr() as usize;
        if let Some(cloned) = self.memory.get(&ptr) {
            return Ok(cloned.clone());
        }

        if ta.is_detached() {
            return Err(StructuredCloneError::DataCloneError(
                "ArrayBuffer is detached",
            ));
        }

        let src_buffer = ta.buffer();
        let buffer_len = src_buffer.byte_length();
        let new_buffer = src_buffer
            .slice(0, buffer_len)
            .ok_or(StructuredCloneError::DataCloneError("ArrayBuffer is detached"))?;
        let new_buffer = GcRef::new(new_buffer);

        let new_obj = GcRef::new(JsObject::new(ta.object.prototype(), self.memory_manager.clone()));
        let new_ta = JsTypedArray::new(
            new_obj,
            new_buffer,
            ta.kind(),
            ta.byte_offset(),
            ta.length(),
        )
        .map_err(StructuredCloneError::DataCloneError)?;
        let new_value = Value::typed_array(GcRef::new(new_ta));
        self.memory.insert(ptr, new_value.clone());
        Ok(new_value)
    }

    fn clone_data_view(&mut self, dv: GcRef<JsDataView>) -> Result<Value, StructuredCloneError> {
        let ptr = dv.as_ptr() as usize;
        if let Some(cloned) = self.memory.get(&ptr) {
            return Ok(cloned.clone());
        }

        if dv.is_detached() {
            return Err(StructuredCloneError::DataCloneError(
                "ArrayBuffer is detached",
            ));
        }

        let src_buffer = dv.buffer();
        let buffer_len = src_buffer.byte_length();
        let new_buffer = src_buffer
            .slice(0, buffer_len)
            .ok_or(StructuredCloneError::DataCloneError("ArrayBuffer is detached"))?;
        let new_buffer = GcRef::new(new_buffer);

        let new_dv = JsDataView::new(new_buffer, dv.byte_offset(), Some(dv.byte_length()))
            .map_err(StructuredCloneError::DataCloneError)?;
        let new_value = Value::data_view(GcRef::new(new_dv));
        self.memory.insert(ptr, new_value.clone());
        Ok(new_value)
    }
}

/// Convenience function to clone a value
pub fn structured_clone(
    value: &Value,
    memory_manager: Arc<crate::memory::MemoryManager>,
) -> Result<Value, StructuredCloneError> {
    StructuredCloner::new(memory_manager).clone(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::array_buffer::JsArrayBuffer;
    use crate::data_view::JsDataView;
    use crate::typed_array::{JsTypedArray, TypedArrayKind};

    #[test]
    fn test_clone_primitives() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let mut cloner = StructuredCloner::new(memory_manager.clone());

        assert!(cloner.clone(&Value::undefined()).unwrap().is_undefined());
        assert!(cloner.clone(&Value::null()).unwrap().is_null());
        assert_eq!(
            cloner.clone(&Value::boolean(true)).unwrap().as_boolean(),
            Some(true)
        );
        assert_eq!(
            cloner.clone(&Value::int32(42)).unwrap().as_number(),
            Some(42.0)
        );
    }

    #[test]
    fn test_clone_string() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let mut cloner = StructuredCloner::new(memory_manager.clone());
        let s = Value::string(JsString::intern("hello"));
        let cloned = cloner.clone(&s).unwrap();
        assert!(cloned.is_string());
        assert_eq!(cloned.as_string().unwrap().as_str(), "hello");
    }

    #[test]
    fn test_clone_object() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let mut cloner = StructuredCloner::new(memory_manager.clone());
        let obj = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        let _ = obj.set(PropertyKey::string("x"), Value::int32(1));
        let _ = obj.set(PropertyKey::string("y"), Value::int32(2));

        let val = Value::object(obj);
        let cloned = cloner.clone(&val).unwrap();

        assert!(cloned.is_object());
        let cloned_obj = cloned.as_object().unwrap();
        assert_eq!(
            cloned_obj.get(&PropertyKey::string("x")),
            Some(Value::int32(1))
        );
        assert_eq!(
            cloned_obj.get(&PropertyKey::string("y")),
            Some(Value::int32(2))
        );
    }

    #[test]
    fn test_shared_array_buffer_shares_memory() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let mut cloner = StructuredCloner::new(memory_manager.clone());
        let sab = GcRef::new(SharedArrayBuffer::new(4));
        let _ = sab.set(0, 42);

        let val = Value::shared_array_buffer(sab);
        let cloned = cloner.clone(&val).unwrap();

        // SharedArrayBuffer should share the same memory
        let cloned_sab = cloned.as_shared_array_buffer().unwrap();
        assert_eq!(cloned_sab.get(0), Some(42));

        // Modify through clone, should affect original
        let _ = cloned_sab.set(0, 100);
        assert_eq!(sab.get(0), Some(100));
    }

    #[test]
    fn test_function_not_cloneable() {
        use crate::object::JsObject;
        use crate::value::Closure;
        use otter_vm_bytecode::Module;

        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let mut cloner = StructuredCloner::new(memory_manager.clone());
        let dummy_module = Arc::new(Module::builder("test.js").build());
        let func = Value::function(GcRef::new(Closure {
            function_index: 0,
            module: dummy_module,
            upvalues: vec![],
            is_async: false,
            is_generator: false,
            object: GcRef::new(JsObject::new(Value::null(), memory_manager.clone())),
            home_object: None,
        }));

        let result = cloner.clone(&func);
        assert!(matches!(
            result,
            Err(StructuredCloneError::NotCloneable("function"))
        ));
    }

    #[test]
    fn test_clone_typed_array_copies_underlying_buffer() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let mut cloner = StructuredCloner::new(memory_manager.clone());

        let buffer = GcRef::new(JsArrayBuffer::new(8, None, memory_manager.clone()));
        let object = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        let ta = JsTypedArray::new(object, buffer, TypedArrayKind::Int16, 2, 2).unwrap();
        let ta = GcRef::new(ta);
        assert!(ta.set(0, 10.0));
        assert!(ta.set(1, 20.0));

        let cloned_val = cloner.clone(&Value::typed_array(ta)).unwrap();
        let cloned_ta = cloned_val.as_typed_array().unwrap();

        assert_eq!(cloned_ta.kind(), TypedArrayKind::Int16);
        assert_eq!(cloned_ta.byte_offset(), 2);
        assert_eq!(cloned_ta.length(), 2);
        assert_eq!(cloned_ta.get(0), Some(10.0));
        assert_eq!(cloned_ta.get(1), Some(20.0));

        assert!(cloned_ta.set(0, 99.0));
        assert_eq!(cloned_ta.get(0), Some(99.0));
        assert_eq!(ta.get(0), Some(10.0));
    }

    #[test]
    fn test_clone_data_view_copies_underlying_buffer() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let mut cloner = StructuredCloner::new(memory_manager.clone());

        let buffer = GcRef::new(JsArrayBuffer::new(8, None, memory_manager.clone()));
        let dv = JsDataView::new(buffer, 1, Some(4)).unwrap();
        let dv = GcRef::new(dv);
        dv.set_uint8(0, 11).unwrap();
        dv.set_uint8(1, 22).unwrap();

        let cloned_val = cloner.clone(&Value::data_view(dv)).unwrap();
        let cloned_dv = cloned_val.as_data_view().unwrap();

        assert_eq!(cloned_dv.byte_offset(), 1);
        assert_eq!(cloned_dv.byte_length(), 4);
        assert_eq!(cloned_dv.get_uint8(0).unwrap(), 11);
        assert_eq!(cloned_dv.get_uint8(1).unwrap(), 22);

        cloned_dv.set_uint8(0, 99).unwrap();
        assert_eq!(cloned_dv.get_uint8(0).unwrap(), 99);
        assert_eq!(dv.get_uint8(0).unwrap(), 11);
    }
}
