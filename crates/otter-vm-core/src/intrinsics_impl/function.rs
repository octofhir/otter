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

use crate::builtin_builder::{BuiltInBuilder, IntrinsicContext, IntrinsicObject};
use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::realm::RealmId;
use crate::string::JsString;
use crate::value::Value;
use otter_macros::dive;
use std::sync::Arc;

pub struct FunctionIntrinsic;

impl IntrinsicObject for FunctionIntrinsic {
    fn init(ctx: &IntrinsicContext) {
        let mm = ctx.mm();
        init_function_prototype(ctx.fn_proto(), &mm);

        if let Some(global) = ctx.global_opt() {
            let realm_id = ctx
                .fn_proto()
                .get(&PropertyKey::string("__realm_id__"))
                .and_then(|v| v.as_int32())
                .map(|id| id as RealmId)
                .unwrap_or(0);

            BuiltInBuilder::new(
                mm.clone(),
                ctx.fn_proto(),
                ctx.intrinsics().function_constructor,
                ctx.intrinsics().function_prototype,
                "Function",
            )
            .inherits(ctx.obj_proto())
            .constructor_fn(create_function_constructor(realm_id), 1)
            .build_and_install(&global);

            // §27.3.1: GeneratorFunction.prototype.[[Prototype]] = Function.prototype
            BuiltInBuilder::new(
                mm.clone(),
                ctx.fn_proto(),
                ctx.alloc_constructor(),
                ctx.intrinsics().generator_function_prototype,
                "GeneratorFunction",
            )
            .inherits(ctx.fn_proto())
            .constructor_fn(create_generator_function_constructor(realm_id), 1)
            .build_and_install(&global);

            // §27.7.1: AsyncFunction.prototype.[[Prototype]] = Function.prototype
            BuiltInBuilder::new(
                mm.clone(),
                ctx.fn_proto(),
                ctx.alloc_constructor(),
                ctx.intrinsics().async_function_prototype,
                "AsyncFunction",
            )
            .inherits(ctx.fn_proto())
            .constructor_fn(create_async_function_constructor(realm_id), 1)
            .build_and_install(&global);

            // §27.4.1: AsyncGeneratorFunction.prototype.[[Prototype]] = Function.prototype
            BuiltInBuilder::new(
                mm.clone(),
                ctx.fn_proto(),
                ctx.alloc_constructor(),
                ctx.intrinsics().async_generator_function_prototype,
                "AsyncGeneratorFunction",
            )
            .inherits(ctx.fn_proto())
            .constructor_fn(create_async_generator_function_constructor(realm_id), 1)
            .build_and_install(&global);

            // §27.4.1 AsyncGeneratorFunction.prototype adjustments:
            // constructor is {writable: false, configurable: true} (not writable like normal ctors)
            {
                let agf_proto = ctx.intrinsics().async_generator_function_prototype;
                // Override constructor to writable:false
                if let Some(ctor_val) = agf_proto.get(&PropertyKey::string("constructor")) {
                    agf_proto.define_property(
                        PropertyKey::string("constructor"),
                        PropertyDescriptor::data_with_attrs(
                            ctor_val,
                            PropertyAttributes {
                                writable: false,
                                enumerable: false,
                                configurable: true,
                            },
                        ),
                    );
                }
                // §27.4.1.1 AsyncGeneratorFunction.prototype.prototype = %AsyncGeneratorPrototype%
                agf_proto.define_property(
                    PropertyKey::string("prototype"),
                    PropertyDescriptor::data_with_attrs(
                        Value::object(ctx.intrinsics().async_generator_prototype),
                        PropertyAttributes {
                            writable: false,
                            enumerable: false,
                            configurable: true,
                        },
                    ),
                );
                // §27.4.1.2 AsyncGeneratorFunction.prototype[@@toStringTag] = "AsyncGeneratorFunction"
                let to_string_tag_sym = crate::intrinsics::well_known::to_string_tag_symbol();
                agf_proto.define_property(
                    PropertyKey::Symbol(to_string_tag_sym),
                    PropertyDescriptor::data_with_attrs(
                        Value::string(JsString::intern("AsyncGeneratorFunction")),
                        PropertyAttributes {
                            writable: false,
                            enumerable: false,
                            configurable: true,
                        },
                    ),
                );
            }

            // Same for GeneratorFunction.prototype (§27.3.1)
            {
                let gf_proto = ctx.intrinsics().generator_function_prototype;
                if let Some(ctor_val) = gf_proto.get(&PropertyKey::string("constructor")) {
                    gf_proto.define_property(
                        PropertyKey::string("constructor"),
                        PropertyDescriptor::data_with_attrs(
                            ctor_val,
                            PropertyAttributes {
                                writable: false,
                                enumerable: false,
                                configurable: true,
                            },
                        ),
                    );
                }
                gf_proto.define_property(
                    PropertyKey::string("prototype"),
                    PropertyDescriptor::data_with_attrs(
                        Value::object(ctx.intrinsics().generator_prototype),
                        PropertyAttributes {
                            writable: false,
                            enumerable: false,
                            configurable: true,
                        },
                    ),
                );
                let to_string_tag_sym = crate::intrinsics::well_known::to_string_tag_symbol();
                gf_proto.define_property(
                    PropertyKey::Symbol(to_string_tag_sym),
                    PropertyDescriptor::data_with_attrs(
                        Value::string(JsString::intern("GeneratorFunction")),
                        PropertyAttributes {
                            writable: false,
                            enumerable: false,
                            configurable: true,
                        },
                    ),
                );
            }

            global.define_property(
                PropertyKey::string("GeneratorFunctionPrototype"),
                crate::object::PropertyDescriptor::data_with_attrs(
                    Value::object(ctx.intrinsics().generator_function_prototype),
                    crate::object::PropertyAttributes::permanent(),
                ),
            );
            global.define_property(
                PropertyKey::string("AsyncFunctionPrototype"),
                crate::object::PropertyDescriptor::data_with_attrs(
                    Value::object(ctx.intrinsics().async_function_prototype),
                    crate::object::PropertyAttributes::permanent(),
                ),
            );
            global.define_property(
                PropertyKey::string("AsyncGeneratorFunctionPrototype"),
                crate::object::PropertyDescriptor::data_with_attrs(
                    Value::object(ctx.intrinsics().async_generator_function_prototype),
                    crate::object::PropertyAttributes::permanent(),
                ),
            );

            if let Some(call_fn) = ctx.fn_proto().get(&PropertyKey::string("call")) {
                let _ = global.set(PropertyKey::string("__Function_call"), call_fn);
            }
            if let Some(apply_fn) = ctx.fn_proto().get(&PropertyKey::string("apply")) {
                let _ = global.set(PropertyKey::string("__Function_apply"), apply_fn);
            }
        }
    }
}

#[dive(name = "toString", length = 0)]
fn function_to_string(
    this_val: &Value,
    _args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    if this_val.is_function()
        && let Some(closure) = this_val.as_function()
    {
        return Ok(Value::string(JsString::intern(&format!(
            "function {}() {{ [native code] }}",
            if closure.is_async { "async " } else { "" }
        ))));
    }
    if this_val.is_native_function() {
        return Ok(Value::string(JsString::intern(
            "function () { [native code] }",
        )));
    }
    if let Some(obj) = this_val.as_object()
        && obj.has(&PropertyKey::string("__boundFunction__"))
    {
        return Ok(Value::string(JsString::intern(
            "function bound() { [native code] }",
        )));
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
        // CreateListFromArrayLike: works with any object that has a length property
        let len = arr_obj
            .get(&PropertyKey::string("length"))
            .and_then(|v| v.as_number())
            .unwrap_or(0.0) as usize;
        let mut extracted = Vec::with_capacity(len.min(1024));
        for i in 0..len {
            // Indexed values on `arguments` and other array-like objects live in
            // element storage, so use ordinary object Get semantics here.
            extracted.push(
                arr_obj
                    .get(&PropertyKey::Index(i as u32))
                    .unwrap_or_default(),
            );
        }
        extracted
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
    // Step 1: If IsCallable(Target) is false, throw a TypeError
    if !this_val.is_callable() {
        return Err(VmError::type_error(
            "Function.prototype.bind requires a callable target",
        ));
    }

    let this_arg = args.first().cloned().unwrap_or(Value::undefined());
    let fn_proto = ncx
        .ctx
        .function_prototype()
        .map(Value::object)
        .unwrap_or_else(Value::null);

    let bound = GcRef::new(JsObject::new(fn_proto));
    let _ = bound.set(PropertyKey::string("__boundFunction__"), *this_val);
    let _ = bound.set(PropertyKey::string("__boundThis__"), this_arg);

    if args.len() > 1 {
        let arr = GcRef::new(JsObject::new(Value::null()));
        for (i, arg) in args[1..].iter().enumerate() {
            let _ = arr.set(PropertyKey::Index(i as u32), *arg);
        }
        let _ = arr.set(
            PropertyKey::string("length"),
            Value::int32((args.len() - 1) as i32),
        );
        let _ = bound.set(PropertyKey::string("__boundArgs__"), Value::object(arr));
    }

    // Step 5: Get target function name for bound name
    let target_name = if let Some(obj) = this_val.as_object() {
        obj.get(&PropertyKey::string("name"))
            .and_then(|v| v.as_string())
            .map(|s| s.as_str().to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let bound_name = format!("bound {}", target_name);
    bound.define_property(
        PropertyKey::string("name"),
        crate::object::PropertyDescriptor::function_length(Value::string(JsString::intern(
            &bound_name,
        ))),
    );

    // Step 6: Calculate length = max(0, targetLen - bound args count)
    let bound_args_len = if args.len() > 1 { args.len() - 1 } else { 0 };
    let target_len = if let Some(closure) = this_val.as_function() {
        closure
            .object
            .get(&PropertyKey::string("length"))
            .and_then(|v| v.as_number())
            .unwrap_or(0.0) as i32
    } else if let Some(obj) = this_val.as_object() {
        obj.get(&PropertyKey::string("length"))
            .and_then(|v| v.as_number())
            .unwrap_or(0.0) as i32
    } else {
        0
    };
    let new_length = (target_len - bound_args_len as i32).max(0);
    bound.define_property(
        PropertyKey::string("length"),
        crate::object::PropertyDescriptor::function_length(Value::int32(new_length)),
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

    // §20.2.3.1-2: Function.prototype.caller and Function.prototype.arguments
    // are accessor properties whose get and set are both %ThrowTypeError%.
    let throw_type_error = Value::native_function(
        |_this: &Value, _args: &[Value], _ncx: &mut crate::context::NativeContext<'_>| {
            Err(VmError::type_error(
                "'caller', 'callee', and 'arguments' properties may not be accessed on strict mode functions or the arguments objects for calls to them",
            ))
        },
        mm.clone(),
    );
    for prop_name in &["caller", "arguments"] {
        fn_proto.define_property(
            PropertyKey::string(prop_name),
            PropertyDescriptor::Accessor {
                get: Some(throw_type_error),
                set: Some(throw_type_error),
                attributes: PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: true,
                },
            },
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

/// Check if `source` contains `keyword` as a standalone token (not inside string literals).
fn contains_keyword_token(source: &str, keyword: &str) -> bool {
    let bytes = source.as_bytes();
    let kw_bytes = keyword.as_bytes();
    let kw_len = kw_bytes.len();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        // Skip string literals
        if b == b'\'' || b == b'"' || b == b'`' {
            let quote = b;
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                if bytes[i] == b'\\' {
                    i += 1; // skip escaped char
                }
                i += 1;
            }
            i += 1; // skip closing quote
            continue;
        }
        // Check for keyword match
        if i + kw_len <= bytes.len() && &bytes[i..i + kw_len] == kw_bytes {
            // Verify it's a standalone token (not part of a larger identifier)
            let before_ok = i == 0
                || !bytes[i - 1].is_ascii_alphanumeric()
                    && bytes[i - 1] != b'_'
                    && bytes[i - 1] != b'$';
            let after_ok = i + kw_len >= bytes.len()
                || !bytes[i + kw_len].is_ascii_alphanumeric()
                    && bytes[i + kw_len] != b'_'
                    && bytes[i + kw_len] != b'$';
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
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

        // §20.2.1.1.1 CreateDynamicFunction steps 28-29:
        // Check for yield/await in generator/async params
        let joined_params = params.join(",");
        if matches!(
            kind,
            DynamicFunctionKind::Generator | DynamicFunctionKind::AsyncGenerator
        ) {
            if contains_keyword_token(&joined_params, "yield") {
                return Err(VmError::SyntaxError(
                    "yield expression is not allowed in formal parameters of a generator function"
                        .to_string(),
                ));
            }
        }
        if matches!(
            kind,
            DynamicFunctionKind::Async | DynamicFunctionKind::AsyncGenerator
        ) {
            if contains_keyword_token(&joined_params, "await") {
                return Err(VmError::SyntaxError(
                    "await expression is not allowed in formal parameters of an async function"
                        .to_string(),
                ));
            }
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
