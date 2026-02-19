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
use crate::intrinsics_impl::helpers::MapKey;
use crate::map_data::{MapData, SetData};
use crate::object::{JsObject, PropertyKey};
use crate::value::{HeapRef, Value};
use crate::{JsDataView, JsTypedArray};
use rustc_hash::FxHashMap;
use std::sync::Arc;

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

            Some(HeapRef::MapData(md)) => self.clone_map_data(*md),
            Some(HeapRef::SetData(sd)) => self.clone_set_data(*sd),
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

        // Detect special object types via marker properties.
        // Must happen before generic property cloning because these objects have
        // internal slots (__map_data__, __set_data__) that contain non-cloneable
        // Value types when treated generically.

        // Date: has __timestamp__ (f64 ms since epoch)
        if let Some(ts) = obj.get(&PropertyKey::string("__timestamp__")) {
            return self.clone_date(obj, ptr, ts);
        }

        // Map: has __is_map__ and __map_data__
        if obj
            .get(&PropertyKey::string("__is_map__"))
            .and_then(|v| v.as_boolean())
            == Some(true)
        {
            if let Some(map_data) = obj
                .get(&PropertyKey::string("__map_data__"))
                .and_then(|v| v.as_map_data())
            {
                return self.clone_map(obj, ptr, map_data);
            }
        }

        // Set: has __is_set__ and __set_data__
        if obj
            .get(&PropertyKey::string("__is_set__"))
            .and_then(|v| v.as_boolean())
            == Some(true)
        {
            if let Some(set_data) = obj
                .get(&PropertyKey::string("__set_data__"))
                .and_then(|v| v.as_set_data())
            {
                return self.clone_set(obj, ptr, set_data);
            }
        }

        // Error: has __is_error__
        if obj
            .get(&PropertyKey::string("__is_error__"))
            .and_then(|v| v.as_boolean())
            == Some(true)
        {
            return self.clone_error(obj, ptr);
        }

        // Generic object: clone all own properties
        let new_obj = GcRef::new(JsObject::new(obj.prototype(), self.memory_manager.clone()));
        let new_value = Value::object(new_obj);
        self.memory.insert(ptr, new_value.clone());

        for key in obj.own_keys() {
            if let Some(val) = obj.get(&key) {
                let cloned_val = self.internal_clone(&val)?;
                let _ = new_obj.set(key, cloned_val);
            }
        }

        Ok(new_value)
    }

    /// Clone a Date object — preserves __timestamp__ and prototype.
    fn clone_date(
        &mut self,
        obj: GcRef<JsObject>,
        ptr: usize,
        timestamp: Value,
    ) -> Result<Value, StructuredCloneError> {
        let new_obj = GcRef::new(JsObject::new(obj.prototype(), self.memory_manager.clone()));
        let _ = new_obj.set(PropertyKey::string("__timestamp__"), timestamp);
        let new_value = Value::object(new_obj);
        self.memory.insert(ptr, new_value.clone());
        Ok(new_value)
    }

    /// Clone a Map object — creates a new MapData with recursively cloned entries.
    fn clone_map(
        &mut self,
        obj: GcRef<JsObject>,
        ptr: usize,
        source_data: GcRef<MapData>,
    ) -> Result<Value, StructuredCloneError> {
        let new_data = GcRef::new(MapData::new());
        let new_obj = GcRef::new(JsObject::new(obj.prototype(), self.memory_manager.clone()));
        let _ = new_obj.set(
            PropertyKey::string("__is_map__"),
            Value::boolean(true),
        );
        let _ = new_obj.set(
            PropertyKey::string("__map_data__"),
            Value::map_data(new_data),
        );
        let new_value = Value::object(new_obj);
        self.memory.insert(ptr, new_value.clone());

        // Iterate source entries and clone each key-value pair
        let entries = source_data.for_each_entries();
        for (key, value) in entries {
            let cloned_key = self.internal_clone(&key)?;
            let cloned_value = self.internal_clone(&value)?;
            new_data.set(MapKey(cloned_key), cloned_value);
        }

        Ok(new_value)
    }

    /// Clone a Set object — creates a new SetData with recursively cloned entries.
    fn clone_set(
        &mut self,
        obj: GcRef<JsObject>,
        ptr: usize,
        source_data: GcRef<SetData>,
    ) -> Result<Value, StructuredCloneError> {
        let new_data = GcRef::new(SetData::new());
        let new_obj = GcRef::new(JsObject::new(obj.prototype(), self.memory_manager.clone()));
        let _ = new_obj.set(
            PropertyKey::string("__is_set__"),
            Value::boolean(true),
        );
        let _ = new_obj.set(
            PropertyKey::string("__set_data__"),
            Value::set_data(new_data),
        );
        let new_value = Value::object(new_obj);
        self.memory.insert(ptr, new_value.clone());

        // Iterate source entries and clone each value
        let entries = source_data.for_each_entries();
        for value in entries {
            let cloned_value = self.internal_clone(&value)?;
            new_data.add(MapKey(cloned_value));
        }

        Ok(new_value)
    }

    /// Clone an Error object — preserves name, message, cause, and stack frames.
    fn clone_error(
        &mut self,
        obj: GcRef<JsObject>,
        ptr: usize,
    ) -> Result<Value, StructuredCloneError> {
        let new_obj = GcRef::new(JsObject::new(obj.prototype(), self.memory_manager.clone()));
        let _ = new_obj.set(
            PropertyKey::string("__is_error__"),
            Value::boolean(true),
        );
        let new_value = Value::object(new_obj);
        self.memory.insert(ptr, new_value.clone());

        // Copy standard Error properties
        if let Some(name) = obj.get(&PropertyKey::string("name")) {
            let cloned_name = self.internal_clone(&name)?;
            let _ = new_obj.set(PropertyKey::string("name"), cloned_name);
        }
        if let Some(message) = obj.get(&PropertyKey::string("message")) {
            let cloned_message = self.internal_clone(&message)?;
            let _ = new_obj.set(PropertyKey::string("message"), cloned_message);
        }
        // cause can be any value — recursively clone it
        if let Some(cause) = obj.get(&PropertyKey::string("cause")) {
            let cloned_cause = self.internal_clone(&cause)?;
            let _ = new_obj.set(PropertyKey::string("cause"), cloned_cause);
        }
        // Stack frames (internal array of frame objects)
        if let Some(frames) = obj.get(&PropertyKey::string("__stack_frames__")) {
            let cloned_frames = self.internal_clone(&frames)?;
            let _ = new_obj.set(PropertyKey::string("__stack_frames__"), cloned_frames);
        }
        // stack (lazy string, may not be present)
        if let Some(stack) = obj.get(&PropertyKey::string("stack")) {
            let cloned_stack = self.internal_clone(&stack)?;
            let _ = new_obj.set(PropertyKey::string("stack"), cloned_stack);
        }

        Ok(new_value)
    }

    /// Clone a standalone MapData value (can appear in nested contexts).
    fn clone_map_data(
        &mut self,
        source: GcRef<MapData>,
    ) -> Result<Value, StructuredCloneError> {
        let ptr = source.as_ptr() as usize;
        if let Some(cloned) = self.memory.get(&ptr) {
            return Ok(cloned.clone());
        }

        let new_data = GcRef::new(MapData::new());
        let new_value = Value::map_data(new_data);
        self.memory.insert(ptr, new_value.clone());

        let entries = source.for_each_entries();
        for (key, value) in entries {
            let cloned_key = self.internal_clone(&key)?;
            let cloned_value = self.internal_clone(&value)?;
            new_data.set(MapKey(cloned_key), cloned_value);
        }

        Ok(new_value)
    }

    /// Clone a standalone SetData value (can appear in nested contexts).
    fn clone_set_data(
        &mut self,
        source: GcRef<SetData>,
    ) -> Result<Value, StructuredCloneError> {
        let ptr = source.as_ptr() as usize;
        if let Some(cloned) = self.memory.get(&ptr) {
            return Ok(cloned.clone());
        }

        let new_data = GcRef::new(SetData::new());
        let new_value = Value::set_data(new_data);
        self.memory.insert(ptr, new_value.clone());

        let entries = source.for_each_entries();
        for value in entries {
            let cloned_value = self.internal_clone(&value)?;
            new_data.add(MapKey(cloned_value));
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
        let new_buffer =
            src_buffer
                .slice(0, buffer_len)
                .ok_or(StructuredCloneError::DataCloneError(
                    "ArrayBuffer is detached",
                ))?;
        let new_buffer = GcRef::new(new_buffer);

        let new_obj = GcRef::new(JsObject::new(
            ta.object.prototype(),
            self.memory_manager.clone(),
        ));
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
        let new_buffer =
            src_buffer
                .slice(0, buffer_len)
                .ok_or(StructuredCloneError::DataCloneError(
                    "ArrayBuffer is detached",
                ))?;
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
    use crate::intrinsics_impl::helpers::MapKey;
    use crate::map_data::{MapData, SetData};
    use crate::typed_array::{JsTypedArray, TypedArrayKind};

    #[test]
    fn test_clone_primitives() {
        let _rt = crate::runtime::VmRuntime::new();
        let memory_manager = _rt.memory_manager().clone();
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
        let _rt = crate::runtime::VmRuntime::new();
        let memory_manager = _rt.memory_manager().clone();
        let mut cloner = StructuredCloner::new(memory_manager.clone());
        let s = Value::string(JsString::intern("hello"));
        let cloned = cloner.clone(&s).unwrap();
        assert!(cloned.is_string());
        assert_eq!(cloned.as_string().unwrap().as_str(), "hello");
    }

    #[test]
    fn test_clone_object() {
        let _rt = crate::runtime::VmRuntime::new();
        let memory_manager = _rt.memory_manager().clone();
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
        let _rt = crate::runtime::VmRuntime::new();
        let memory_manager = _rt.memory_manager().clone();
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

        let _rt = crate::runtime::VmRuntime::new();
        let memory_manager = _rt.memory_manager().clone();
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
        let _rt = crate::runtime::VmRuntime::new();
        let memory_manager = _rt.memory_manager().clone();
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
        let _rt = crate::runtime::VmRuntime::new();
        let memory_manager = _rt.memory_manager().clone();
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

    #[test]
    fn test_clone_date() {
        let _rt = crate::runtime::VmRuntime::new();
        let mm = _rt.memory_manager().clone();
        let mut cloner = StructuredCloner::new(mm.clone());

        // Simulate a Date object: JsObject with __timestamp__
        let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        let timestamp = 1700000000000.0_f64; // 2023-11-14T22:13:20Z
        let _ = obj.set(
            PropertyKey::string("__timestamp__"),
            Value::number(timestamp),
        );

        let val = Value::object(obj);
        let cloned = cloner.clone(&val).unwrap();

        let cloned_obj = cloned.as_object().unwrap();
        let cloned_ts = cloned_obj
            .get(&PropertyKey::string("__timestamp__"))
            .unwrap();
        assert_eq!(cloned_ts.as_number(), Some(timestamp));

        // Verify independence: changing original doesn't affect clone
        let _ = obj.set(
            PropertyKey::string("__timestamp__"),
            Value::number(0.0),
        );
        let still_ts = cloned_obj
            .get(&PropertyKey::string("__timestamp__"))
            .unwrap();
        assert_eq!(still_ts.as_number(), Some(timestamp));
    }

    #[test]
    fn test_clone_date_nan() {
        let _rt = crate::runtime::VmRuntime::new();
        let mm = _rt.memory_manager().clone();
        let mut cloner = StructuredCloner::new(mm.clone());

        // Invalid Date has NaN timestamp
        let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        let _ = obj.set(
            PropertyKey::string("__timestamp__"),
            Value::nan(),
        );

        let val = Value::object(obj);
        let cloned = cloner.clone(&val).unwrap();
        let cloned_obj = cloned.as_object().unwrap();
        let ts = cloned_obj
            .get(&PropertyKey::string("__timestamp__"))
            .unwrap();
        assert!(ts.as_number().unwrap().is_nan());
    }

    #[test]
    fn test_clone_map() {
        let _rt = crate::runtime::VmRuntime::new();
        let mm = _rt.memory_manager().clone();
        let mut cloner = StructuredCloner::new(mm.clone());

        // Build a Map-like object
        let data = GcRef::new(MapData::new());
        data.set(MapKey(Value::string(JsString::intern("a"))), Value::int32(1));
        data.set(MapKey(Value::string(JsString::intern("b"))), Value::int32(2));

        let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        let _ = obj.set(PropertyKey::string("__is_map__"), Value::boolean(true));
        let _ = obj.set(
            PropertyKey::string("__map_data__"),
            Value::map_data(data),
        );

        let val = Value::object(obj);
        let cloned = cloner.clone(&val).unwrap();

        let cloned_obj = cloned.as_object().unwrap();
        assert_eq!(
            cloned_obj
                .get(&PropertyKey::string("__is_map__"))
                .and_then(|v| v.as_boolean()),
            Some(true)
        );

        let cloned_data = cloned_obj
            .get(&PropertyKey::string("__map_data__"))
            .unwrap()
            .as_map_data()
            .unwrap();
        assert_eq!(cloned_data.size(), 2);
        assert_eq!(
            cloned_data.get(&MapKey(Value::string(JsString::intern("a")))),
            Some(Value::int32(1))
        );
        assert_eq!(
            cloned_data.get(&MapKey(Value::string(JsString::intern("b")))),
            Some(Value::int32(2))
        );

        // Verify independence
        data.set(MapKey(Value::string(JsString::intern("c"))), Value::int32(3));
        assert_eq!(cloned_data.size(), 2); // clone unaffected
    }

    #[test]
    fn test_clone_set() {
        let _rt = crate::runtime::VmRuntime::new();
        let mm = _rt.memory_manager().clone();
        let mut cloner = StructuredCloner::new(mm.clone());

        let data = GcRef::new(SetData::new());
        data.add(MapKey(Value::int32(10)));
        data.add(MapKey(Value::int32(20)));
        data.add(MapKey(Value::int32(30)));

        let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        let _ = obj.set(PropertyKey::string("__is_set__"), Value::boolean(true));
        let _ = obj.set(
            PropertyKey::string("__set_data__"),
            Value::set_data(data),
        );

        let val = Value::object(obj);
        let cloned = cloner.clone(&val).unwrap();

        let cloned_obj = cloned.as_object().unwrap();
        let cloned_data = cloned_obj
            .get(&PropertyKey::string("__set_data__"))
            .unwrap()
            .as_set_data()
            .unwrap();
        assert_eq!(cloned_data.size(), 3);
        assert!(cloned_data.has(&MapKey(Value::int32(10))));
        assert!(cloned_data.has(&MapKey(Value::int32(20))));
        assert!(cloned_data.has(&MapKey(Value::int32(30))));

        // Verify independence
        data.add(MapKey(Value::int32(40)));
        assert_eq!(cloned_data.size(), 3);
    }

    #[test]
    fn test_clone_error() {
        let _rt = crate::runtime::VmRuntime::new();
        let mm = _rt.memory_manager().clone();
        let mut cloner = StructuredCloner::new(mm.clone());

        let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        let _ = obj.set(PropertyKey::string("__is_error__"), Value::boolean(true));
        let _ = obj.set(
            PropertyKey::string("name"),
            Value::string(JsString::intern("TypeError")),
        );
        let _ = obj.set(
            PropertyKey::string("message"),
            Value::string(JsString::intern("something went wrong")),
        );

        let val = Value::object(obj);
        let cloned = cloner.clone(&val).unwrap();

        let cloned_obj = cloned.as_object().unwrap();
        assert_eq!(
            cloned_obj
                .get(&PropertyKey::string("__is_error__"))
                .and_then(|v| v.as_boolean()),
            Some(true)
        );
        assert_eq!(
            cloned_obj
                .get(&PropertyKey::string("name"))
                .and_then(|v| v.as_string())
                .map(|s| s.as_str().to_string()),
            Some("TypeError".to_string())
        );
        assert_eq!(
            cloned_obj
                .get(&PropertyKey::string("message"))
                .and_then(|v| v.as_string())
                .map(|s| s.as_str().to_string()),
            Some("something went wrong".to_string())
        );
    }

    #[test]
    fn test_clone_error_with_cause() {
        let _rt = crate::runtime::VmRuntime::new();
        let mm = _rt.memory_manager().clone();
        let mut cloner = StructuredCloner::new(mm.clone());

        // Inner cause error
        let cause_obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        let _ = cause_obj.set(PropertyKey::string("__is_error__"), Value::boolean(true));
        let _ = cause_obj.set(
            PropertyKey::string("name"),
            Value::string(JsString::intern("Error")),
        );
        let _ = cause_obj.set(
            PropertyKey::string("message"),
            Value::string(JsString::intern("root cause")),
        );

        // Outer error with cause
        let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        let _ = obj.set(PropertyKey::string("__is_error__"), Value::boolean(true));
        let _ = obj.set(
            PropertyKey::string("name"),
            Value::string(JsString::intern("Error")),
        );
        let _ = obj.set(
            PropertyKey::string("message"),
            Value::string(JsString::intern("wrapper")),
        );
        let _ = obj.set(
            PropertyKey::string("cause"),
            Value::object(cause_obj),
        );

        let val = Value::object(obj);
        let cloned = cloner.clone(&val).unwrap();

        let cloned_obj = cloned.as_object().unwrap();
        let cloned_cause = cloned_obj
            .get(&PropertyKey::string("cause"))
            .unwrap();
        let cloned_cause_obj = cloned_cause.as_object().unwrap();
        assert_eq!(
            cloned_cause_obj
                .get(&PropertyKey::string("message"))
                .and_then(|v| v.as_string())
                .map(|s| s.as_str().to_string()),
            Some("root cause".to_string())
        );
    }

    #[test]
    fn test_clone_map_with_object_values() {
        let _rt = crate::runtime::VmRuntime::new();
        let mm = _rt.memory_manager().clone();
        let mut cloner = StructuredCloner::new(mm.clone());

        // Map with object values that should be deeply cloned
        let inner_obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        let _ = inner_obj.set(PropertyKey::string("x"), Value::int32(42));

        let data = GcRef::new(MapData::new());
        data.set(
            MapKey(Value::string(JsString::intern("key"))),
            Value::object(inner_obj),
        );

        let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        let _ = obj.set(PropertyKey::string("__is_map__"), Value::boolean(true));
        let _ = obj.set(PropertyKey::string("__map_data__"), Value::map_data(data));

        let cloned = cloner.clone(&Value::object(obj)).unwrap();

        let cloned_data = cloned
            .as_object()
            .unwrap()
            .get(&PropertyKey::string("__map_data__"))
            .unwrap()
            .as_map_data()
            .unwrap();
        let cloned_inner = cloned_data
            .get(&MapKey(Value::string(JsString::intern("key"))))
            .unwrap();
        let cloned_inner_obj = cloned_inner.as_object().unwrap();
        assert_eq!(
            cloned_inner_obj.get(&PropertyKey::string("x")),
            Some(Value::int32(42))
        );

        // Verify deep independence
        let _ = inner_obj.set(PropertyKey::string("x"), Value::int32(999));
        assert_eq!(
            cloned_inner_obj.get(&PropertyKey::string("x")),
            Some(Value::int32(42))
        );
    }
}
