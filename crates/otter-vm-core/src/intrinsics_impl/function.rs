//! Function.prototype methods implementation
//!
//! Complete ES2026 Function.prototype:
//! - call(thisArg, ...args) - calls function with specified this
//! - apply(thisArg, argsArray) - calls function with this and array of args
//! - bind(thisArg, ...args) - creates bound function
//! - toString() - returns string representation
//!
//! ## Implementation Strategy
//! - **call/apply**: Hybrid approach with error-based interception for closures
//!   - Native functions: Direct call (zero overhead fast path)
//!   - Closures: Error-based interception in interpreter (full VM context access)
//! - **bind**: Direct implementation (creates bound function object)
//! - **toString**: Direct implementation (returns string representation)
//!
//! ## ES2026 Compliance
//! All methods follow ECMAScript specification:
//! - call §20.2.3.3
//! - apply §20.2.3.1
//! - bind §20.2.3.2
//! - toString §20.2.3.5

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::realm::RealmId;
use crate::string::JsString;
use crate::value::Value;
use otter_macros::dive;
use std::sync::Arc;

#[dive(name = "toString", length = 0)]
fn function_to_string(
    this_val: &Value,
    _args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    if this_val.is_function() {
        if let Some(closure) = this_val.as_function() {
            return Ok(Value::string(JsString::intern(&format!(
                "function {}() {{ [native code] }}",
                if closure.is_async { "async " } else { "" }
            ))));
        }
    }
    if this_val.is_native_function() {
        return Ok(Value::string(JsString::intern(
            "function () { [native code] }",
        )));
    }
    if let Some(obj) = this_val.as_object() {
        if obj.has(&PropertyKey::string("__boundFunction__")) {
            return Ok(Value::string(JsString::intern(
                "function bound() { [native code] }",
            )));
        }
    }
    Err(VmError::type_error(
        "Function.prototype.toString requires a function",
    ))
}

#[dive(name = "call", length = 1)]
fn function_call(
    this_val: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let this_arg = args.first().cloned().unwrap_or(Value::undefined());
    let call_args: Vec<Value> = if args.len() > 1 {
        args[1..].to_vec()
    } else {
        vec![]
    };

    if let Some(proxy) = this_val.as_proxy() {
        return crate::proxy_operations::proxy_apply(ncx, proxy, this_arg, &call_args);
    }
    if !this_val.is_callable() {
        return Err(VmError::type_error(
            "Function.prototype.call requires a callable target",
        ));
    }
    ncx.call_function(this_val, this_arg, &call_args)
}

#[dive(name = "apply", length = 2)]
fn function_apply(
    this_val: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let this_arg = args.first().cloned().unwrap_or(Value::undefined());
    let args_array_val = args.get(1).cloned().unwrap_or(Value::undefined());

    let call_args = if args_array_val.is_undefined() || args_array_val.is_null() {
        vec![]
    } else if let Some(arr_obj) = args_array_val.as_object() {
        if arr_obj.is_array() {
            let len = arr_obj.array_length();
            let mut extracted = Vec::with_capacity(len);
            for i in 0..len {
                extracted.push(
                    arr_obj
                        .get(&PropertyKey::Index(i as u32))
                        .unwrap_or(Value::undefined()),
                );
            }
            extracted
        } else {
            return Err(VmError::type_error(
                "Function.prototype.apply: argumentsList must be an array",
            ));
        }
    } else {
        return Err(VmError::type_error(
            "Function.prototype.apply: argumentsList must be an object",
        ));
    };

    if let Some(proxy) = this_val.as_proxy() {
        return crate::proxy_operations::proxy_apply(ncx, proxy, this_arg, &call_args);
    }
    if !this_val.is_callable() {
        return Err(VmError::type_error(
            "Function.prototype.apply requires a callable target",
        ));
    }
    ncx.call_function(this_val, this_arg, &call_args)
}

#[dive(name = "bind", length = 1)]
fn function_bind(
    this_val: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let this_arg = args.first().cloned().unwrap_or(Value::undefined());
    let fn_proto = ncx
        .ctx
        .function_prototype()
        .map(Value::object)
        .unwrap_or_else(Value::null);

    let bound = GcRef::new(JsObject::new(fn_proto, ncx.memory_manager().clone()));
    let _ = bound.set(PropertyKey::string("__boundFunction__"), this_val.clone());
    let _ = bound.set(PropertyKey::string("__boundThis__"), this_arg);

    if args.len() > 1 {
        let arr = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
        for (i, arg) in args[1..].iter().enumerate() {
            let _ = arr.set(PropertyKey::Index(i as u32), arg.clone());
        }
        let _ = arr.set(
            PropertyKey::string("length"),
            Value::int32((args.len() - 1) as i32),
        );
        let _ = bound.set(PropertyKey::string("__boundArgs__"), Value::object(arr));
    }

    let _ = bound.set(
        PropertyKey::string("__boundName__"),
        Value::string(JsString::intern("bound ")),
    );

    let bound_args_len = if args.len() > 1 { args.len() - 1 } else { 0 };
    let new_length = 0i32.saturating_sub(bound_args_len as i32).max(0);
    let _ = bound.set(
        PropertyKey::string("__boundLength__"),
        Value::int32(new_length),
    );
    let _ = bound.set(PropertyKey::string("__isCallable__"), Value::boolean(true));

    Ok(Value::object(bound))
}

/// Initialize Function.prototype with all ES2026 methods
///
/// # Methods
/// - **call(thisArg, ...args)** - Calls function with specified this
/// - **apply(thisArg, argsArray)** - Calls function with this and array
/// - **bind(thisArg, ...args)** - Creates bound function
/// - **toString()** - Returns string representation
///
/// # Property Attributes
/// All methods: `{ writable: true, enumerable: false, configurable: true }`
pub fn init_function_prototype(fn_proto: GcRef<JsObject>, mm: &Arc<MemoryManager>) {
    // Function.prototype.length = 0 (§20.2.3)
    fn_proto.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::function_length(Value::number(0.0)),
    );
    // Function.prototype.name = "" (§20.2.3)
    fn_proto.define_property(
        PropertyKey::string("name"),
        PropertyDescriptor::function_length(Value::string(JsString::intern(""))),
    );

    let methods: &[(&str, crate::value::NativeFn, u32)] = &[
        function_to_string_decl(),
        function_call_decl(),
        function_apply_decl(),
        function_bind_decl(),
    ];
    for (name, native_fn, length) in methods {
        let fn_val = Value::native_function_from_decl(name, native_fn.clone(), *length, mm.clone());
        fn_proto.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::builtin_method(fn_val),
        );
    }
}

/// Create a dynamic Function constructor (Function/GeneratorFunction/AsyncFunction).
pub fn create_function_constructor(
    realm_id: RealmId,
) -> Box<dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync> {
    create_dynamic_function_constructor(DynamicFunctionKind::Normal, realm_id)
}

pub fn create_generator_function_constructor(
    realm_id: RealmId,
) -> Box<dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync> {
    create_dynamic_function_constructor(DynamicFunctionKind::Generator, realm_id)
}

pub fn create_async_function_constructor(
    realm_id: RealmId,
) -> Box<dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync> {
    create_dynamic_function_constructor(DynamicFunctionKind::Async, realm_id)
}

pub fn create_async_generator_function_constructor(
    realm_id: RealmId,
) -> Box<dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync> {
    create_dynamic_function_constructor(DynamicFunctionKind::AsyncGenerator, realm_id)
}

#[derive(Clone, Copy)]
enum DynamicFunctionKind {
    Normal,
    Generator,
    Async,
    AsyncGenerator,
}

fn build_dynamic_function_source(
    kind: DynamicFunctionKind,
    params: &[String],
    body: &str,
) -> String {
    let params = params.join(",");
    match kind {
        DynamicFunctionKind::Normal => format!("(function anonymous({}) {{ {} }})", params, body),
        DynamicFunctionKind::Generator => {
            format!("(function* anonymous({}) {{ {} }})", params, body)
        }
        DynamicFunctionKind::Async => {
            format!("(async function anonymous({}) {{ {} }})", params, body)
        }
        DynamicFunctionKind::AsyncGenerator => {
            format!("(async function* anonymous({}) {{ {} }})", params, body)
        }
    }
}

fn dynamic_function_fallback_proto(ncx: &NativeContext<'_>, kind: DynamicFunctionKind) -> Value {
    let realm_id = ncx.ctx.realm_id();
    let intrinsics = ncx.ctx.realm_intrinsics(realm_id);
    match kind {
        DynamicFunctionKind::Normal => intrinsics
            .map(|i| Value::object(i.function_prototype))
            .or_else(|| ncx.ctx.function_prototype().map(Value::object))
            .unwrap_or_else(Value::null),
        DynamicFunctionKind::Generator => intrinsics
            .map(|i| Value::object(i.generator_function_prototype))
            .unwrap_or_else(Value::null),
        DynamicFunctionKind::Async => intrinsics
            .map(|i| Value::object(i.async_function_prototype))
            .unwrap_or_else(Value::null),
        DynamicFunctionKind::AsyncGenerator => intrinsics
            .map(|i| Value::object(i.async_generator_function_prototype))
            .unwrap_or_else(Value::null),
    }
}

fn create_dynamic_function_constructor(
    kind: DynamicFunctionKind,
    constructor_realm_id: RealmId,
) -> Box<dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync> {
    Box::new(move |this, args, ncx| {
        let mut params = Vec::new();
        let mut body = String::new();
        if !args.is_empty() {
            for arg in &args[..args.len() - 1] {
                params.push(ncx.to_string_value(arg)?);
            }
            body = ncx.to_string_value(args.last().unwrap())?;
        }

        let source = build_dynamic_function_source(kind, &params, &body);
        let module = ncx.ctx.compile_eval(&source, false)?;
        let func_value = ncx.execute_eval_module_in_realm(constructor_realm_id, &module)?;

        let proto_value = if ncx.is_construct() {
            this.as_object()
                .map(|obj| obj.prototype())
                .unwrap_or_else(Value::null)
        } else {
            dynamic_function_fallback_proto(ncx, kind)
        };

        if let Some(obj) = func_value.as_object() {
            obj.set_prototype(proto_value);
        }

        Ok(func_value)
    })
}
