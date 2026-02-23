//! WeakRef and FinalizationRegistry constructors and prototype methods (ES2021+)
//!
//! ## WeakRef
//! - `new WeakRef(target)` — creates a weak reference to `target`
//! - `WeakRef.prototype.deref()` — returns the target if still alive, else `undefined`
//!
//! ## FinalizationRegistry
//! - `new FinalizationRegistry(callback)` — creates a registry with cleanup callback
//! - `FinalizationRegistry.prototype.register(target, heldValue, [unregisterToken])` — register target
//! - `FinalizationRegistry.prototype.unregister(token)` — remove registrations for token

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use crate::weak_gc;
use std::sync::Arc;

// ============================================================================
// Internal slot keys
// ============================================================================
const WEAKREF_CELL: &str = "__weakref_cell__";
const WEAKREF_MARKER: &str = "__is_weakref__";
const FINREG_DATA: &str = "__finreg_data__";
const FINREG_CALLBACK: &str = "__finreg_callback__";
const FINREG_HELD_VALUES: &str = "__finreg_held__";
const FINREG_TOKENS: &str = "__finreg_tokens__";
const FINREG_MARKER: &str = "__is_finreg__";

fn pk(s: &str) -> PropertyKey {
    PropertyKey::String(JsString::intern(s))
}

fn make_builtin<F>(
    name: &str,
    length: i32,
    f: F,
    mm: Arc<MemoryManager>,
    fn_proto: GcRef<JsObject>,
) -> Value
where
    F: Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError>
        + Send
        + Sync
        + 'static,
{
    let val = Value::native_function_with_proto(f, mm, fn_proto);
    if let Some(obj) = val.native_function_object() {
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::int32(length)),
        );
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern(name))),
        );
        let _ = obj.set(pk("__non_constructor"), Value::boolean(true));
    }
    val
}

/// Check if a value is a valid WeakRef/FinalizationRegistry target (must be an object).
fn is_valid_target(value: &Value) -> bool {
    // Must have a gc_header (heap-allocated) and not be a primitive wrapper
    if value.gc_header().is_none() {
        return false;
    }
    // Reject string, number, boolean, undefined, null, symbol, bigint
    !value.is_string()
        && !value.is_number()
        && !value.is_boolean()
        && !value.is_undefined()
        && !value.is_null()
        && value.as_symbol().is_none()
        && !value.is_bigint()
}

// ============================================================================
// WeakRef constructor + deref()
// ============================================================================

fn weakref_constructor(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let target = args.first().cloned().unwrap_or_else(Value::undefined);

    if !is_valid_target(&target) {
        return Err(VmError::type_error("WeakRef: target must be an object"));
    }

    let target_header = target.gc_header().unwrap();
    let cell = GcRef::new(otter_vm_gc::WeakRefCell::new(target_header));
    let mm = ncx.memory_manager().clone();

    let weak_ref_proto = ncx
        .global()
        .get(&pk("WeakRef"))
        .and_then(|v| v.as_object().or_else(|| v.native_function_object()))
        .and_then(|o| o.get(&pk("prototype")))
        .and_then(|v| v.as_object())
        .map(Value::object)
        .unwrap_or(Value::null());

    let obj = GcRef::new(JsObject::new(weak_ref_proto, mm));
    let _ = obj.set(pk(WEAKREF_MARKER), Value::boolean(true));
    let _ = obj.set(pk(WEAKREF_CELL), Value::weak_ref(cell));

    // Store target in untraced side table (NOT as a property, which would keep it alive)
    weak_gc::register_weak_ref_target(cell, target);

    Ok(Value::object(obj))
}

fn weakref_deref(
    this_val: &Value,
    _args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let obj = this_val
        .as_object()
        .ok_or_else(|| VmError::type_error("WeakRef.prototype.deref called on non-object"))?;

    if obj
        .get(&pk(WEAKREF_MARKER))
        .and_then(|v| v.as_boolean())
        != Some(true)
    {
        return Err(VmError::type_error(
            "WeakRef.prototype.deref called on non-WeakRef",
        ));
    }

    let cell = obj
        .get(&pk(WEAKREF_CELL))
        .and_then(|v| v.as_weak_ref())
        .ok_or_else(|| VmError::type_error("WeakRef: invalid internal state"))?;

    if cell.is_alive() {
        // Look up target from untraced side table
        Ok(weak_gc::get_weak_ref_target(&cell).unwrap_or_else(Value::undefined))
    } else {
        Ok(Value::undefined())
    }
}

// ============================================================================
// FinalizationRegistry constructor + register/unregister
// ============================================================================

fn finreg_constructor(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let callback = args.first().cloned().unwrap_or_else(Value::undefined);

    if !callback.is_function() && !callback.is_native_function() {
        return Err(VmError::type_error(
            "FinalizationRegistry: callback must be a function",
        ));
    }

    let data = GcRef::new(otter_vm_gc::FinalizationRegistryData::new());
    let mm = ncx.memory_manager().clone();

    let finreg_proto = ncx
        .global()
        .get(&pk("FinalizationRegistry"))
        .and_then(|v| v.as_object().or_else(|| v.native_function_object()))
        .and_then(|o| o.get(&pk("prototype")))
        .and_then(|v| v.as_object())
        .map(Value::object)
        .unwrap_or(Value::null());

    let obj = GcRef::new(JsObject::new(finreg_proto, mm.clone()));
    let _ = obj.set(pk(FINREG_MARKER), Value::boolean(true));
    let _ = obj.set(pk(FINREG_DATA), Value::finalization_registry(data));
    let _ = obj.set(pk(FINREG_CALLBACK), callback);

    // Create arrays for held values and tokens (indexed by entry_index)
    let held_arr = GcRef::new(JsObject::array(0, mm.clone()));
    let token_arr = GcRef::new(JsObject::array(0, mm));
    let _ = obj.set(pk(FINREG_HELD_VALUES), Value::array(held_arr));
    let _ = obj.set(pk(FINREG_TOKENS), Value::array(token_arr));

    // Register for GC sweep tracking
    weak_gc::register_finalization_registry(data, obj);

    Ok(Value::object(obj))
}

fn finreg_register(
    this_val: &Value,
    args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let obj = this_val
        .as_object()
        .ok_or_else(|| VmError::type_error("FinalizationRegistry.prototype.register called on non-object"))?;

    if obj
        .get(&pk(FINREG_MARKER))
        .and_then(|v| v.as_boolean())
        != Some(true)
    {
        return Err(VmError::type_error(
            "FinalizationRegistry.prototype.register called on non-FinalizationRegistry",
        ));
    }

    let target = args.first().cloned().unwrap_or_else(Value::undefined);
    let held_value = args.get(1).cloned().unwrap_or_else(Value::undefined);
    let unregister_token = args.get(2).cloned().unwrap_or_else(Value::undefined);

    if !is_valid_target(&target) {
        return Err(VmError::type_error(
            "FinalizationRegistry.register: target must be an object",
        ));
    }

    let target_header = target.gc_header().unwrap();

    let data = obj
        .get(&pk(FINREG_DATA))
        .and_then(|v| v.as_finalization_registry())
        .ok_or_else(|| VmError::type_error("FinalizationRegistry: invalid internal state"))?;

    // Register and get entry index
    let idx = data.register(target_header);

    // Store held value at entry index
    if let Some(held_arr) = obj
        .get(&pk(FINREG_HELD_VALUES))
        .and_then(|v| v.as_array())
    {
        let _ = held_arr.set(PropertyKey::Index(idx), held_value);
    }

    // Store unregister token at entry index
    if let Some(token_arr) = obj
        .get(&pk(FINREG_TOKENS))
        .and_then(|v| v.as_array())
    {
        let _ = token_arr.set(PropertyKey::Index(idx), unregister_token);
    }

    Ok(Value::undefined())
}

fn finreg_unregister(
    this_val: &Value,
    args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let obj = this_val
        .as_object()
        .ok_or_else(|| VmError::type_error("FinalizationRegistry.prototype.unregister called on non-object"))?;

    if obj
        .get(&pk(FINREG_MARKER))
        .and_then(|v| v.as_boolean())
        != Some(true)
    {
        return Err(VmError::type_error(
            "FinalizationRegistry.prototype.unregister called on non-FinalizationRegistry",
        ));
    }

    let token = args.first().cloned().unwrap_or_else(Value::undefined);

    if !is_valid_target(&token) {
        return Err(VmError::type_error(
            "FinalizationRegistry.unregister: token must be an object",
        ));
    }

    // Find all entry indices that match this token
    let token_arr = obj
        .get(&pk(FINREG_TOKENS))
        .and_then(|v| v.as_array());

    let data = obj
        .get(&pk(FINREG_DATA))
        .and_then(|v| v.as_finalization_registry())
        .ok_or_else(|| VmError::type_error("FinalizationRegistry: invalid internal state"))?;

    if let Some(tokens) = token_arr {
        // Walk token array and find matching entries
        let mut matching_indices = Vec::new();
        let len = tokens
            .get(&pk("length"))
            .and_then(|v| v.as_number())
            .unwrap_or(0.0) as u32;
        for i in 0..len {
            if let Some(stored_token) = tokens.get(&PropertyKey::Index(i)) {
                // Identity comparison for object tokens
                if let (Some(a), Some(b)) = (token.gc_header(), stored_token.gc_header())
                    && std::ptr::eq(a, b)
                {
                    matching_indices.push(i);
                }
            }
        }

        if !matching_indices.is_empty() {
            let removed = data.unregister_indices(&matching_indices);
            return Ok(Value::boolean(removed));
        }
    }

    Ok(Value::boolean(false))
}

// ============================================================================
// Public initialization
// ============================================================================

/// Initialize WeakRef prototype.
pub fn init_weak_ref_prototype(
    weak_ref_prototype: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    symbol_to_string_tag: GcRef<crate::value::Symbol>,
) {
    // WeakRef.prototype.deref()
    weak_ref_prototype.define_property(
        pk("deref"),
        PropertyDescriptor::builtin_method(make_builtin(
            "deref",
            0,
            weakref_deref,
            mm.clone(),
            fn_proto,
        )),
    );

    // [Symbol.toStringTag] = "WeakRef"
    weak_ref_prototype.define_property(
        PropertyKey::Symbol(symbol_to_string_tag),
        PropertyDescriptor::Data {
            value: Value::string(JsString::intern("WeakRef")),
            attributes: PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        },
    );
}

/// Initialize FinalizationRegistry prototype.
pub fn init_finalization_registry_prototype(
    finreg_prototype: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    symbol_to_string_tag: GcRef<crate::value::Symbol>,
) {
    // FinalizationRegistry.prototype.register()
    finreg_prototype.define_property(
        pk("register"),
        PropertyDescriptor::builtin_method(make_builtin(
            "register",
            2,
            finreg_register,
            mm.clone(),
            fn_proto,
        )),
    );

    // FinalizationRegistry.prototype.unregister()
    finreg_prototype.define_property(
        pk("unregister"),
        PropertyDescriptor::builtin_method(make_builtin(
            "unregister",
            1,
            finreg_unregister,
            mm.clone(),
            fn_proto,
        )),
    );

    // [Symbol.toStringTag] = "FinalizationRegistry"
    finreg_prototype.define_property(
        PropertyKey::Symbol(symbol_to_string_tag),
        PropertyDescriptor::Data {
            value: Value::string(JsString::intern("FinalizationRegistry")),
            attributes: PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        },
    );
}

/// Install WeakRef and FinalizationRegistry as global constructors.
pub fn install_weakref_constructors(
    global: GcRef<JsObject>,
    weak_ref_prototype: GcRef<JsObject>,
    finreg_prototype: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // WeakRef constructor
    let ctor = Value::native_function_with_proto(weakref_constructor, mm.clone(), fn_proto);
    if let Some(ctor_obj) = ctor.native_function_object() {
        ctor_obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::int32(1)),
        );
        ctor_obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("WeakRef"))),
        );
        ctor_obj.define_property(
            PropertyKey::string("prototype"),
            PropertyDescriptor::Data {
                value: Value::object(weak_ref_prototype),
                attributes: PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: false,
                },
            },
        );
    }

    weak_ref_prototype.define_property(
        PropertyKey::string("constructor"),
        PropertyDescriptor::Data {
            value: ctor.clone(),
            attributes: PropertyAttributes {
                writable: true,
                enumerable: false,
                configurable: true,
            },
        },
    );

    global.define_property(
        PropertyKey::string("WeakRef"),
        PropertyDescriptor::Data {
            value: ctor,
            attributes: PropertyAttributes {
                writable: true,
                enumerable: false,
                configurable: true,
            },
        },
    );

    // FinalizationRegistry constructor
    let fr_ctor =
        Value::native_function_with_proto(finreg_constructor, mm.clone(), fn_proto);
    if let Some(ctor_obj) = fr_ctor.native_function_object() {
        ctor_obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::int32(1)),
        );
        ctor_obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern(
                "FinalizationRegistry",
            ))),
        );
        ctor_obj.define_property(
            PropertyKey::string("prototype"),
            PropertyDescriptor::Data {
                value: Value::object(finreg_prototype),
                attributes: PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: false,
                },
            },
        );
    }

    finreg_prototype.define_property(
        PropertyKey::string("constructor"),
        PropertyDescriptor::Data {
            value: fr_ctor.clone(),
            attributes: PropertyAttributes {
                writable: true,
                enumerable: false,
                configurable: true,
            },
        },
    );

    global.define_property(
        PropertyKey::string("FinalizationRegistry"),
        PropertyDescriptor::Data {
            value: fr_ctor,
            attributes: PropertyAttributes {
                writable: true,
                enumerable: false,
                configurable: true,
            },
        },
    );
}
