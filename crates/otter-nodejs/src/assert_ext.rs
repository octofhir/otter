//! Native `node:assert` extension.
//!
//! `assert` is both a callable function AND a namespace object.
//! Provides: assert/ok, equal, notEqual, strictEqual, notStrictEqual,
//! deepEqual, deepStrictEqual, throws, doesNotThrow, match, doesNotMatch,
//! ifError, rejects, strict.
//!
//! All methods use `#[js_class]` / `#[js_static]` macros for consistent codegen.

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

use crate::util_ext::{deep_strict_equal, format_value, make_fn};

// ---------------------------------------------------------------------------
// OtterExtension
// ---------------------------------------------------------------------------

pub struct NodeAssertExtension;

impl OtterExtension for NodeAssertExtension {
    fn name(&self) -> &str {
        "node_assert"
    }

    fn profiles(&self) -> &[Profile] {
        static P: [Profile; 2] = [Profile::SafeCore, Profile::Full];
        &P
    }

    fn deps(&self) -> &[&str] {
        &["node_util"]
    }

    fn module_specifiers(&self) -> &[&str] {
        static S: [&str; 4] = [
            "node:assert",
            "assert",
            "node:assert/strict",
            "assert/strict",
        ];
        &S
    }

    fn install(&self, _ctx: &mut RegistrationContext) -> Result<(), VmError> {
        Ok(())
    }

    fn load_module(
        &self,
        specifier: &str,
        ctx: &mut RegistrationContext,
    ) -> Option<GcRef<JsObject>> {
        let is_strict = specifier.contains("strict");
        let assert_fn = build_assert_function(ctx, is_strict);
        let ns = ctx.new_object();
        let _ = ns.set(PropertyKey::string("default"), assert_fn.clone());

        // Copy all properties from assert_fn (which is a function-object with methods)
        if let Some(fn_obj) = assert_fn.as_object() {
            for key in fn_obj.own_keys() {
                if let Some(val) = fn_obj.get(&key) {
                    let _ = ns.set(key, val);
                }
            }
        }

        Some(ns)
    }
}

pub fn node_assert_extension() -> Box<dyn OtterExtension> {
    Box::new(NodeAssertExtension)
}

// ---------------------------------------------------------------------------
// Assert methods via #[js_class]
// ---------------------------------------------------------------------------

#[js_class(name = "Assert")]
pub struct Assert;

#[js_class]
impl Assert {
    #[js_static(name = "ok", length = 1)]
    pub fn ok(_this: &Value, args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
        assert_ok_impl(args)
    }

    #[js_static(name = "equal", length = 2)]
    pub fn equal(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let actual = args.first().cloned().unwrap_or(Value::undefined());
        let expected = args.get(1).cloned().unwrap_or(Value::undefined());
        if !strict_equal(&actual, &expected) {
            let msg = get_message(args, 2);
            return Err(assertion_error(msg.as_deref(), &actual, &expected, "=="));
        }
        Ok(Value::undefined())
    }

    #[js_static(name = "notEqual", length = 2)]
    pub fn not_equal(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let actual = args.first().cloned().unwrap_or(Value::undefined());
        let expected = args.get(1).cloned().unwrap_or(Value::undefined());
        if strict_equal(&actual, &expected) {
            let msg = get_message(args, 2);
            return Err(assertion_error(msg.as_deref(), &actual, &expected, "!="));
        }
        Ok(Value::undefined())
    }

    #[js_static(name = "strictEqual", length = 2)]
    pub fn strict_equal_fn(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let actual = args.first().cloned().unwrap_or(Value::undefined());
        let expected = args.get(1).cloned().unwrap_or(Value::undefined());
        if !strict_equal(&actual, &expected) {
            let msg = get_message(args, 2);
            return Err(assertion_error(msg.as_deref(), &actual, &expected, "==="));
        }
        Ok(Value::undefined())
    }

    #[js_static(name = "notStrictEqual", length = 2)]
    pub fn not_strict_equal(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let actual = args.first().cloned().unwrap_or(Value::undefined());
        let expected = args.get(1).cloned().unwrap_or(Value::undefined());
        if strict_equal(&actual, &expected) {
            let msg = get_message(args, 2);
            return Err(assertion_error(msg.as_deref(), &actual, &expected, "!=="));
        }
        Ok(Value::undefined())
    }

    #[js_static(name = "deepEqual", length = 2)]
    pub fn deep_equal(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let actual = args.first().cloned().unwrap_or(Value::undefined());
        let expected = args.get(1).cloned().unwrap_or(Value::undefined());
        if !deep_strict_equal(&actual, &expected, 0) {
            let msg = get_message(args, 2);
            return Err(assertion_error(
                msg.as_deref(),
                &actual,
                &expected,
                "deepEqual",
            ));
        }
        Ok(Value::undefined())
    }

    #[js_static(name = "notDeepEqual", length = 2)]
    pub fn not_deep_equal(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let actual = args.first().cloned().unwrap_or(Value::undefined());
        let expected = args.get(1).cloned().unwrap_or(Value::undefined());
        if deep_strict_equal(&actual, &expected, 0) {
            let msg = get_message(args, 2);
            return Err(assertion_error(
                msg.as_deref(),
                &actual,
                &expected,
                "notDeepEqual",
            ));
        }
        Ok(Value::undefined())
    }

    #[js_static(name = "deepStrictEqual", length = 2)]
    pub fn deep_strict_equal_fn(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let actual = args.first().cloned().unwrap_or(Value::undefined());
        let expected = args.get(1).cloned().unwrap_or(Value::undefined());
        if !deep_strict_equal(&actual, &expected, 0) {
            let msg = get_message(args, 2);
            return Err(assertion_error(
                msg.as_deref(),
                &actual,
                &expected,
                "deepStrictEqual",
            ));
        }
        Ok(Value::undefined())
    }

    #[js_static(name = "notDeepStrictEqual", length = 2)]
    pub fn not_deep_strict_equal(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let actual = args.first().cloned().unwrap_or(Value::undefined());
        let expected = args.get(1).cloned().unwrap_or(Value::undefined());
        if deep_strict_equal(&actual, &expected, 0) {
            let msg = get_message(args, 2);
            return Err(assertion_error(
                msg.as_deref(),
                &actual,
                &expected,
                "notDeepStrictEqual",
            ));
        }
        Ok(Value::undefined())
    }

    #[js_static(name = "throws", length = 1)]
    pub fn throws(
        _this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let func = args
            .first()
            .filter(|v| v.is_callable())
            .cloned()
            .ok_or_else(|| {
                VmError::type_error("assert.throws: first argument must be a function")
            })?;

        match ncx.call_function(&func, Value::undefined(), &[]) {
            Ok(_) => {
                let msg = get_message(args, 1)
                    .unwrap_or_else(|| "Missing expected exception".to_string());
                Err(VmError::type_error(&format!("AssertionError: {msg}")))
            }
            Err(_) => Ok(Value::undefined()),
        }
    }

    #[js_static(name = "doesNotThrow", length = 1)]
    pub fn does_not_throw(
        _this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let func = args
            .first()
            .filter(|v| v.is_callable())
            .cloned()
            .ok_or_else(|| {
                VmError::type_error("assert.doesNotThrow: first argument must be a function")
            })?;

        match ncx.call_function(&func, Value::undefined(), &[]) {
            Ok(_) => Ok(Value::undefined()),
            Err(e) => {
                let msg = format!("Got unwanted exception: {e}");
                Err(VmError::type_error(&format!("AssertionError: {msg}")))
            }
        }
    }

    #[js_static(name = "match", length = 2)]
    pub fn match_fn(
        _this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let string_val = args.first().cloned().unwrap_or(Value::undefined());
        let regexp_val = args.get(1).cloned().unwrap_or(Value::undefined());

        if string_val.as_string().is_none() {
            return Err(VmError::type_error(
                "assert.match: first argument must be a string",
            ));
        }
        if regexp_val.as_regex().is_none() {
            return Err(VmError::type_error(
                "assert.match: second argument must be a RegExp",
            ));
        }

        // Call RegExp.prototype.test via ncx
        let test_method = regexp_val
            .as_regex()
            .and_then(|r| r.object.get(&PropertyKey::string("test")))
            .or_else(|| {
                // Fall back to prototype lookup
                regexp_val
                    .as_object()
                    .and_then(|o| o.get(&PropertyKey::string("test")))
            });

        let matches = if let Some(test_fn) = test_method {
            let result = ncx.call_function(&test_fn, regexp_val.clone(), &[string_val.clone()])?;
            result.to_boolean()
        } else {
            // Direct exec fallback
            let regexp = regexp_val.as_regex().unwrap();
            let string = string_val.as_string().unwrap();
            regexp.exec(&string, 0).is_some()
        };

        if matches {
            Ok(Value::undefined())
        } else {
            let pattern = regexp_val
                .as_regex()
                .map(|r| r.pattern.clone())
                .unwrap_or_default();
            let s = string_val
                .as_string()
                .map(|s| s.as_str().to_string())
                .unwrap_or_default();
            let msg = get_message(args, 2).unwrap_or_else(|| {
                format!("The input did not match the regular expression /{pattern}/. Input: '{s}'")
            });
            Err(VmError::type_error(&format!("AssertionError: {msg}")))
        }
    }

    #[js_static(name = "doesNotMatch", length = 2)]
    pub fn does_not_match(
        _this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let string_val = args.first().cloned().unwrap_or(Value::undefined());
        let regexp_val = args.get(1).cloned().unwrap_or(Value::undefined());

        if string_val.as_string().is_none() {
            return Err(VmError::type_error(
                "assert.doesNotMatch: first argument must be a string",
            ));
        }
        if regexp_val.as_regex().is_none() {
            return Err(VmError::type_error(
                "assert.doesNotMatch: second argument must be a RegExp",
            ));
        }

        // Call RegExp.prototype.test via ncx
        let test_method = regexp_val
            .as_regex()
            .and_then(|r| r.object.get(&PropertyKey::string("test")))
            .or_else(|| {
                regexp_val
                    .as_object()
                    .and_then(|o| o.get(&PropertyKey::string("test")))
            });

        let matches = if let Some(test_fn) = test_method {
            let result = ncx.call_function(&test_fn, regexp_val.clone(), &[string_val.clone()])?;
            result.to_boolean()
        } else {
            let regexp = regexp_val.as_regex().unwrap();
            let string = string_val.as_string().unwrap();
            regexp.exec(&string, 0).is_some()
        };

        if !matches {
            Ok(Value::undefined())
        } else {
            let pattern = regexp_val
                .as_regex()
                .map(|r| r.pattern.clone())
                .unwrap_or_default();
            let s = string_val
                .as_string()
                .map(|s| s.as_str().to_string())
                .unwrap_or_default();
            let msg = get_message(args, 2).unwrap_or_else(|| {
                format!(
                    "The input was expected to not match the regular expression /{pattern}/. Input: '{s}'"
                )
            });
            Err(VmError::type_error(&format!("AssertionError: {msg}")))
        }
    }

    #[js_static(name = "ifError", length = 1)]
    pub fn if_error(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let val = args.first().cloned().unwrap_or(Value::undefined());
        if !val.is_null() && !val.is_undefined() {
            return Err(VmError::exception(val));
        }
        Ok(Value::undefined())
    }

    #[js_static(name = "fail", length = 1)]
    pub fn fail(_this: &Value, args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
        let msg = args
            .first()
            .and_then(|v| v.as_string())
            .map(|s| s.as_str().to_string())
            .unwrap_or_else(|| "Failed".to_string());
        Err(VmError::type_error(&format!("AssertionError: {msg}")))
    }
}

// ---------------------------------------------------------------------------
// Assertion error helper
// ---------------------------------------------------------------------------

fn assertion_error(
    message: Option<&str>,
    actual: &Value,
    expected: &Value,
    operator: &str,
) -> VmError {
    let msg = message.map(|s| s.to_string()).unwrap_or_else(|| {
        format!(
            "Expected {} {} {}",
            format_value(actual),
            operator,
            format_value(expected)
        )
    });
    VmError::type_error(&format!("AssertionError: {msg}"))
}

fn get_message(args: &[Value], idx: usize) -> Option<String> {
    args.get(idx)
        .filter(|v| !v.is_undefined())
        .and_then(|v| v.as_string())
        .map(|s| s.as_str().to_string())
}

fn assert_ok_impl(args: &[Value]) -> Result<Value, VmError> {
    let val = args.first().cloned().unwrap_or(Value::undefined());
    if !val.to_boolean() {
        let msg = get_message(args, 1);
        return Err(assertion_error(
            msg.as_deref(),
            &val,
            &Value::boolean(true),
            "==",
        ));
    }
    Ok(Value::undefined())
}

// ---------------------------------------------------------------------------
// Build the assert function object with methods from _decl() functions
// ---------------------------------------------------------------------------

fn build_assert_function(ctx: &RegistrationContext, is_strict: bool) -> Value {
    // Create assert as a callable function (assert(value, message) = assert.ok(value, message))
    let assert_fn_val = Value::native_function_with_proto(
        |_this, args, _ncx| assert_ok_impl(args),
        ctx.mm().clone(),
        ctx.fn_proto(),
    );

    if let Some(fn_obj) = assert_fn_val.as_object() {
        // Set name and length
        fn_obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("assert"))),
        );
        fn_obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::number(1.0)),
        );

        type DeclFn = fn() -> (
            &'static str,
            Arc<
                dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError>
                    + Send
                    + Sync,
            >,
            u32,
        );

        // All assert methods via _decl() from the macro
        let methods: &[DeclFn] = &[
            Assert::ok_decl,
            Assert::equal_decl,
            Assert::not_equal_decl,
            Assert::strict_equal_fn_decl,
            Assert::not_strict_equal_decl,
            Assert::deep_equal_decl,
            Assert::not_deep_equal_decl,
            Assert::deep_strict_equal_fn_decl,
            Assert::not_deep_strict_equal_decl,
            Assert::throws_decl,
            Assert::does_not_throw_decl,
            Assert::match_fn_decl,
            Assert::does_not_match_decl,
            Assert::if_error_decl,
            Assert::fail_decl,
        ];

        for decl in methods {
            let (name, func, length) = decl();
            let method_val = make_fn(ctx, name, func, length);
            let _ = fn_obj.set(PropertyKey::string(name), method_val);
        }

        // assert.rejects — async stub
        let rejects_fn: Arc<
            dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
        > = Arc::new(|_this, _args, _ncx| Ok(Value::undefined()));
        let _ = fn_obj.set(
            PropertyKey::string("rejects"),
            make_fn(ctx, "rejects", rejects_fn.clone(), 1),
        );

        // assert.doesNotReject
        let _ = fn_obj.set(
            PropertyKey::string("doesNotReject"),
            make_fn(ctx, "doesNotReject", rejects_fn, 1),
        );

        // assert.strict
        if is_strict {
            let _ = fn_obj.set(PropertyKey::string("strict"), assert_fn_val.clone());
        } else {
            let strict_fn = build_assert_function(ctx, true);
            let strict_ns = ctx.new_object();
            let _ = strict_ns.set(PropertyKey::string("default"), strict_fn.clone());
            if let Some(strict_obj) = strict_fn.as_object() {
                for key in strict_obj.own_keys() {
                    if let Some(val) = strict_obj.get(&key) {
                        let _ = strict_ns.set(key, val);
                    }
                }
            }
            let _ = fn_obj.set(PropertyKey::string("strict"), Value::object(strict_ns));
        }
    }

    assert_fn_val
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_assert_ok_passes() {
        let _rt = otter_vm_core::runtime::VmRuntime::new();
        assert!(assert_ok_impl(&[Value::boolean(true)]).is_ok());
        assert!(assert_ok_impl(&[Value::number(1.0)]).is_ok());
        assert!(assert_ok_impl(&[Value::string(JsString::intern("hello"))]).is_ok());
    }

    #[test]
    fn test_assert_ok_fails() {
        assert!(assert_ok_impl(&[Value::boolean(false)]).is_err());
        assert!(assert_ok_impl(&[Value::undefined()]).is_err());
        assert!(assert_ok_impl(&[Value::null()]).is_err());
        assert!(assert_ok_impl(&[Value::number(0.0)]).is_err());
    }

    #[test]
    fn test_assert_metadata() {
        assert_eq!(Assert::JS_CLASS_NAME, "Assert");
    }

    #[test]
    fn test_assert_decl_functions() {
        let (name, _func, length) = Assert::ok_decl();
        assert_eq!(name, "ok");
        assert_eq!(length, 1);

        let (name, _func, length) = Assert::strict_equal_fn_decl();
        assert_eq!(name, "strictEqual");
        assert_eq!(length, 2);

        let (name, _func, length) = Assert::throws_decl();
        assert_eq!(name, "throws");
        assert_eq!(length, 1);

        let (name, _func, length) = Assert::deep_strict_equal_fn_decl();
        assert_eq!(name, "deepStrictEqual");
        assert_eq!(length, 2);
    }
}
