//! Native `node:util` extension.
//!
//! Provides `util.format()`, `util.types.*`, `util.inspect()`, `util.isPrimitive()`,
//! `util.isDeepStrictEqual()`, `util.stripVTControlCharacters()`, etc.
//!
//! All functions use `#[js_class]` / `#[js_static]` macros for consistent codegen.

use std::sync::Arc;

use otter_macros::{js_class, js_static};
use otter_vm_core::context::NativeContext;
use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::intrinsics_impl::helpers::strict_equal;
use otter_vm_core::object::{JsObject, PropertyDescriptor, PropertyKey};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::extension_v2::{OtterExtension, Profile};
use otter_vm_runtime::registration::RegistrationContext;

// ---------------------------------------------------------------------------
// OtterExtension
// ---------------------------------------------------------------------------

pub struct NodeUtilExtension;

impl OtterExtension for NodeUtilExtension {
    fn name(&self) -> &str {
        "node_util"
    }

    fn profiles(&self) -> &[Profile] {
        static P: [Profile; 2] = [Profile::SafeCore, Profile::Full];
        &P
    }

    fn deps(&self) -> &[&str] {
        &[]
    }

    fn module_specifiers(&self) -> &[&str] {
        static S: [&str; 2] = ["node:util", "util"];
        &S
    }

    fn install(&self, _ctx: &mut RegistrationContext) -> Result<(), VmError> {
        Ok(())
    }

    fn load_module(
        &self,
        _specifier: &str,
        ctx: &mut RegistrationContext,
    ) -> Option<GcRef<JsObject>> {
        let types_obj = build_types_object(ctx);

        type DeclFn = fn() -> (
            &'static str,
            Arc<
                dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError>
                    + Send
                    + Sync,
            >,
            u32,
        );

        let util_fns: &[DeclFn] = &[
            Util::format_decl,
            Util::inspect_decl,
            Util::is_primitive_decl,
            Util::is_deep_strict_equal_decl,
            Util::strip_vt_control_characters_decl,
            Util::get_call_sites_decl,
        ];

        let mut ns_builder = ctx.module_namespace();
        ns_builder = ns_builder
            .property("default", Value::undefined())
            .property("types", Value::object(types_obj));

        for decl in util_fns {
            let (name, func, length) = decl();
            ns_builder = ns_builder.function(name, func, length);
        }

        let ns = ns_builder.build();

        // Set default to the namespace itself
        let _ = ns.set(PropertyKey::string("default"), Value::object(ns));

        Some(ns)
    }
}

pub fn node_util_extension() -> Box<dyn OtterExtension> {
    Box::new(NodeUtilExtension)
}

// ---------------------------------------------------------------------------
// Util functions via #[js_class]
// ---------------------------------------------------------------------------

#[js_class(name = "Util")]
pub struct Util;

#[js_class]
impl Util {
    #[js_static(name = "format", length = 1)]
    pub fn format(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        if args.is_empty() {
            return Ok(Value::string(JsString::intern("")));
        }

        let fmt_str = args[0]
            .as_string()
            .map(|s| s.as_str().to_string())
            .unwrap_or_else(|| format_value(&args[0]));

        let mut result = String::new();
        let mut arg_idx = 1;
        let mut chars = fmt_str.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '%' {
                if let Some(&spec) = chars.peek() {
                    match spec {
                        's' => {
                            chars.next();
                            if arg_idx < args.len() {
                                result.push_str(&format_value(&args[arg_idx]));
                                arg_idx += 1;
                            } else {
                                result.push_str("%s");
                            }
                        }
                        'd' | 'i' => {
                            chars.next();
                            if arg_idx < args.len() {
                                let n = args[arg_idx].as_number().unwrap_or(f64::NAN);
                                if spec == 'i' {
                                    result.push_str(&(n as i64).to_string());
                                } else if n.fract() == 0.0 && n.is_finite() {
                                    result.push_str(&(n as i64).to_string());
                                } else {
                                    result.push_str(&n.to_string());
                                }
                                arg_idx += 1;
                            } else {
                                result.push('%');
                                result.push(spec);
                            }
                        }
                        'f' => {
                            chars.next();
                            if arg_idx < args.len() {
                                let n = args[arg_idx].as_number().unwrap_or(f64::NAN);
                                result.push_str(&n.to_string());
                                arg_idx += 1;
                            } else {
                                result.push_str("%f");
                            }
                        }
                        'j' => {
                            chars.next();
                            if arg_idx < args.len() {
                                result.push_str(&format_value(&args[arg_idx]));
                                arg_idx += 1;
                            } else {
                                result.push_str("%j");
                            }
                        }
                        'o' | 'O' => {
                            chars.next();
                            if arg_idx < args.len() {
                                result.push_str(&format_value(&args[arg_idx]));
                                arg_idx += 1;
                            } else {
                                result.push('%');
                                result.push(spec);
                            }
                        }
                        '%' => {
                            chars.next();
                            result.push('%');
                        }
                        _ => {
                            result.push('%');
                        }
                    }
                } else {
                    result.push('%');
                }
            } else {
                result.push(c);
            }
        }

        // Append remaining args separated by spaces
        while arg_idx < args.len() {
            result.push(' ');
            result.push_str(&format_value(&args[arg_idx]));
            arg_idx += 1;
        }

        Ok(Value::string(JsString::new_gc(&result)))
    }

    #[js_static(name = "inspect", length = 1)]
    pub fn inspect(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let val = args.first().cloned().unwrap_or(Value::undefined());
        let s = format_value(&val);
        Ok(Value::string(JsString::new_gc(&s)))
    }

    #[js_static(name = "isPrimitive", length = 1)]
    pub fn is_primitive(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let val = args.first().cloned().unwrap_or(Value::undefined());
        let is_prim = val.is_undefined()
            || val.is_null()
            || val.as_boolean().is_some()
            || val.as_number().is_some()
            || val.as_string().is_some()
            || val.as_symbol().is_some();
        Ok(Value::boolean(is_prim))
    }

    #[js_static(name = "isDeepStrictEqual", length = 2)]
    pub fn is_deep_strict_equal(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let a = args.first().cloned().unwrap_or(Value::undefined());
        let b = args.get(1).cloned().unwrap_or(Value::undefined());
        Ok(Value::boolean(deep_strict_equal(&a, &b, 0)))
    }

    #[js_static(name = "stripVTControlCharacters", length = 1)]
    pub fn strip_vt_control_characters(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let val = args.first().cloned().unwrap_or(Value::undefined());
        let s = val
            .as_string()
            .map(|s| s.as_str().to_string())
            .unwrap_or_default();
        let stripped = strip_ansi(&s);
        Ok(Value::string(JsString::new_gc(&stripped)))
    }

    #[js_static(name = "getCallSites", length = 0)]
    pub fn get_call_sites(
        _this: &Value,
        _args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        // Stub: return empty array
        Ok(Value::object(GcRef::new(JsObject::array(
            0,
            ncx.memory_manager().clone(),
        ))))
    }
}

// ---------------------------------------------------------------------------
// util.types via #[js_class]
// ---------------------------------------------------------------------------

#[js_class(name = "UtilTypes")]
pub struct UtilTypes;

#[js_class]
impl UtilTypes {
    #[js_static(name = "isArray", length = 1)]
    pub fn is_array(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let val = args.first().cloned().unwrap_or(Value::undefined());
        Ok(Value::boolean(
            val.as_object().is_some_and(|o| o.is_array()),
        ))
    }

    #[js_static(name = "isDate", length = 1)]
    pub fn is_date(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let val = args.first().cloned().unwrap_or(Value::undefined());
        Ok(Value::boolean(val.as_object().is_some_and(|o| {
            o.get(&PropertyKey::string("__timestamp__")).is_some()
        })))
    }

    #[js_static(name = "isMap", length = 1)]
    pub fn is_map(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let val = args.first().cloned().unwrap_or(Value::undefined());
        Ok(Value::boolean(val.as_object().is_some_and(|o| {
            o.get(&PropertyKey::string("__map_data__"))
                .and_then(|v| v.as_map_data())
                .is_some()
        })))
    }

    #[js_static(name = "isSet", length = 1)]
    pub fn is_set(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let val = args.first().cloned().unwrap_or(Value::undefined());
        Ok(Value::boolean(val.as_object().is_some_and(|o| {
            o.get(&PropertyKey::string("__set_data__"))
                .and_then(|v| v.as_set_data())
                .is_some()
        })))
    }

    #[js_static(name = "isWeakMap", length = 1)]
    pub fn is_weak_map(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let val = args.first().cloned().unwrap_or(Value::undefined());
        Ok(Value::boolean(val.as_object().is_some_and(|o| {
            o.get(&PropertyKey::string("__weakmap_entries__")).is_some()
        })))
    }

    #[js_static(name = "isWeakSet", length = 1)]
    pub fn is_weak_set(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let val = args.first().cloned().unwrap_or(Value::undefined());
        Ok(Value::boolean(val.as_object().is_some_and(|o| {
            o.get(&PropertyKey::string("__weakset_entries__")).is_some()
        })))
    }

    #[js_static(name = "isRegExp", length = 1)]
    pub fn is_reg_exp(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let val = args.first().cloned().unwrap_or(Value::undefined());
        Ok(Value::boolean(val.as_regex().is_some()))
    }

    #[js_static(name = "isPromise", length = 1)]
    pub fn is_promise(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let val = args.first().cloned().unwrap_or(Value::undefined());
        Ok(Value::boolean(val.as_promise().is_some()))
    }

    #[js_static(name = "isProxy", length = 1)]
    pub fn is_proxy(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let val = args.first().cloned().unwrap_or(Value::undefined());
        Ok(Value::boolean(val.as_proxy().is_some()))
    }

    #[js_static(name = "isTypedArray", length = 1)]
    pub fn is_typed_array(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let val = args.first().cloned().unwrap_or(Value::undefined());
        Ok(Value::boolean(val.is_typed_array()))
    }

    #[js_static(name = "isArrayBuffer", length = 1)]
    pub fn is_array_buffer(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let val = args.first().cloned().unwrap_or(Value::undefined());
        Ok(Value::boolean(val.is_array_buffer()))
    }

    #[js_static(name = "isNativeError", length = 1)]
    pub fn is_native_error(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let val = args.first().cloned().unwrap_or(Value::undefined());
        Ok(Value::boolean(val.as_object().is_some_and(|o| {
            o.get(&PropertyKey::string("stack")).is_some()
                && o.get(&PropertyKey::string("message")).is_some()
        })))
    }

    #[js_static(name = "isGeneratorFunction", length = 1)]
    pub fn is_generator_function(
        _this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        // TODO: proper generator function detection
        Ok(Value::boolean(false))
    }

    #[js_static(name = "isAsyncFunction", length = 1)]
    pub fn is_async_function(
        _this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        // TODO: proper async function detection
        Ok(Value::boolean(false))
    }
}

fn build_types_object(ctx: &RegistrationContext) -> GcRef<JsObject> {
    type DeclFn = fn() -> (
        &'static str,
        Arc<dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync>,
        u32,
    );

    let type_fns: &[DeclFn] = &[
        UtilTypes::is_array_decl,
        UtilTypes::is_date_decl,
        UtilTypes::is_map_decl,
        UtilTypes::is_set_decl,
        UtilTypes::is_weak_map_decl,
        UtilTypes::is_weak_set_decl,
        UtilTypes::is_reg_exp_decl,
        UtilTypes::is_promise_decl,
        UtilTypes::is_proxy_decl,
        UtilTypes::is_typed_array_decl,
        UtilTypes::is_array_buffer_decl,
        UtilTypes::is_native_error_decl,
        UtilTypes::is_generator_function_decl,
        UtilTypes::is_async_function_decl,
    ];

    let obj = ctx.new_object();
    for decl in type_fns {
        let (name, func, length) = decl();
        let fn_val = make_fn(ctx, name, func, length);
        let _ = obj.set(PropertyKey::string(name), fn_val);
    }
    obj
}

// ---------------------------------------------------------------------------
// Shared helpers (pub for assert_ext)
// ---------------------------------------------------------------------------

/// Helper to create a native function with proper name/length.
pub fn make_fn(
    ctx: &RegistrationContext,
    name: &str,
    f: Arc<dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync>,
    length: u32,
) -> Value {
    let fn_obj = GcRef::new(JsObject::new(
        Value::object(ctx.fn_proto()),
        ctx.mm().clone(),
    ));
    fn_obj.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::function_length(Value::number(length as f64)),
    );
    fn_obj.define_property(
        PropertyKey::string("name"),
        PropertyDescriptor::function_length(Value::string(JsString::intern(name))),
    );
    Value::native_function_with_proto_and_object(f, ctx.mm().clone(), ctx.fn_proto(), fn_obj)
}

/// Simple value formatter (used by format, inspect, and assert).
pub fn format_value(val: &Value) -> String {
    if val.is_undefined() {
        "undefined".to_string()
    } else if val.is_null() {
        "null".to_string()
    } else if let Some(b) = val.as_boolean() {
        if b { "true" } else { "false" }.to_string()
    } else if let Some(n) = val.as_number() {
        if n.fract() == 0.0 && n.is_finite() && n.abs() < (i64::MAX as f64) {
            (n as i64).to_string()
        } else {
            n.to_string()
        }
    } else if let Some(s) = val.as_string() {
        s.as_str().to_string()
    } else if let Some(_sym) = val.as_symbol() {
        "Symbol(...)".to_string()
    } else if let Some(obj) = val.as_object() {
        if obj.is_array() {
            let len = obj.array_length();
            let mut parts = Vec::with_capacity(len.min(5));
            for i in 0..len.min(5) {
                if let Some(v) = obj.get(&PropertyKey::Index(i as u32)) {
                    parts.push(format_value(&v));
                }
            }
            if len > 5 {
                parts.push(format!("... {} more items", len - 5));
            }
            format!("[ {} ]", parts.join(", "))
        } else {
            "[Object]".to_string()
        }
    } else {
        format!("{:?}", val)
    }
}

/// Deep strict equality comparison (shared with assert).
pub fn deep_strict_equal(a: &Value, b: &Value, depth: usize) -> bool {
    const MAX_DEEP_EQUAL_DEPTH: usize = 100;

    if depth > MAX_DEEP_EQUAL_DEPTH {
        return false;
    }

    // Strict equal primitives
    if strict_equal(a, b) {
        return true;
    }

    // Both must be objects
    let (obj_a, obj_b) = match (a.as_object(), b.as_object()) {
        (Some(a), Some(b)) => (a, b),
        _ => return false,
    };

    // Same pointer = equal
    if obj_a.as_ptr() == obj_b.as_ptr() {
        return true;
    }

    // Arrays
    if obj_a.is_array() || obj_b.is_array() {
        if !(obj_a.is_array() && obj_b.is_array()) {
            return false;
        }
        let len_a = obj_a.array_length();
        let len_b = obj_b.array_length();
        if len_a != len_b {
            return false;
        }
        for i in 0..len_a {
            let va = obj_a
                .get(&PropertyKey::Index(i as u32))
                .unwrap_or(Value::undefined());
            let vb = obj_b
                .get(&PropertyKey::Index(i as u32))
                .unwrap_or(Value::undefined());
            if !deep_strict_equal(&va, &vb, depth + 1) {
                return false;
            }
        }
        return true;
    }

    // Plain objects: compare own enumerable keys
    let keys_a = obj_a.own_keys();
    let keys_b = obj_b.own_keys();

    if keys_a.len() != keys_b.len() {
        return false;
    }

    for key in &keys_a {
        let va = obj_a.get(key).unwrap_or(Value::undefined());
        let vb = match obj_b.get(key) {
            Some(v) => v,
            None => return false,
        };
        if !deep_strict_equal(&va, &vb, depth + 1) {
            return false;
        }
    }

    true
}

/// Strip ANSI escape sequences from a string.
fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if let Some(next) = chars.next() {
                if next == '[' {
                    for c2 in chars.by_ref() {
                        if c2.is_ascii_alphabetic() || c2 == '~' {
                            break;
                        }
                    }
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_value_primitives() {
        assert_eq!(format_value(&Value::undefined()), "undefined");
        assert_eq!(format_value(&Value::null()), "null");
        assert_eq!(format_value(&Value::boolean(true)), "true");
        assert_eq!(format_value(&Value::number(42.0)), "42");
        assert_eq!(
            format_value(&Value::string(JsString::intern("hello"))),
            "hello"
        );
    }

    #[test]
    fn test_strip_ansi() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("no escape"), "no escape");
    }

    #[test]
    fn test_deep_strict_equal_primitives() {
        assert!(deep_strict_equal(
            &Value::number(1.0),
            &Value::number(1.0),
            0
        ));
        assert!(!deep_strict_equal(
            &Value::number(1.0),
            &Value::number(2.0),
            0
        ));
        assert!(deep_strict_equal(
            &Value::string(JsString::intern("a")),
            &Value::string(JsString::intern("a")),
            0
        ));
    }

    #[test]
    fn test_util_metadata() {
        assert_eq!(Util::JS_CLASS_NAME, "Util");
        assert_eq!(UtilTypes::JS_CLASS_NAME, "UtilTypes");
    }

    #[test]
    fn test_util_decl_functions() {
        let (name, _func, length) = Util::format_decl();
        assert_eq!(name, "format");
        assert_eq!(length, 1);

        let (name, _func, length) = Util::inspect_decl();
        assert_eq!(name, "inspect");
        assert_eq!(length, 1);

        let (name, _func, length) = UtilTypes::is_array_decl();
        assert_eq!(name, "isArray");
        assert_eq!(length, 1);
    }
}
