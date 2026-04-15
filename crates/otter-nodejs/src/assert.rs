use otter_macros::lodge;
use otter_runtime::{
    HostedExtensionModule, ObjectHandle, RegisterValue, RuntimeState, VmNativeCallError,
};
use otter_vm::abstract_ops::{is_strictly_equal, same_value};
use otter_vm::descriptors::NativeFunctionDescriptor;

use crate::support::{
    install_method, install_readonly_value, string_value, type_error, value_to_string,
};

const ASSERT_EXPORT_SLOT: &str = "__otter_node_assert_export";
const ASSERT_STRICT_EXPORT_SLOT: &str = "__otter_node_assert_strict_export";

#[derive(Debug, Clone, Copy)]
enum AssertVariant {
    Legacy,
    Strict,
}

lodge!(
    assert_legacy_module,
    module_specifiers = ["node:assert", "assert"],
    kind = commonjs,
    default = value(assert_export_value(runtime, AssertVariant::Legacy)?),
);

lodge!(
    assert_strict_module,
    module_specifiers = ["node:assert/strict", "assert/strict"],
    kind = commonjs,
    default = value(assert_export_value(runtime, AssertVariant::Strict)?),
);

pub(crate) fn assert_module_entries() -> Vec<HostedExtensionModule> {
    let mut entries = assert_legacy_module_entries();
    entries.extend(assert_strict_module_entries());
    entries
}

fn assert_export_value(
    runtime: &mut RuntimeState,
    variant: AssertVariant,
) -> Result<RegisterValue, String> {
    ensure_assert_exports(runtime)?;
    let slot = match variant {
        AssertVariant::Legacy => ASSERT_EXPORT_SLOT,
        AssertVariant::Strict => ASSERT_STRICT_EXPORT_SLOT,
    };
    read_global_slot(runtime, slot)
}

fn ensure_assert_exports(runtime: &mut RuntimeState) -> Result<(), String> {
    if read_global_slot(runtime, ASSERT_EXPORT_SLOT).is_ok()
        && read_global_slot(runtime, ASSERT_STRICT_EXPORT_SLOT).is_ok()
    {
        return Ok(());
    }

    // Build the legacy assert function (assert.equal uses loose ==).
    let legacy = build_assert_object(runtime, false)?;

    // Build the strict assert function (assert.equal uses ===).
    let strict = build_assert_object(runtime, true)?;

    // Cross-link: legacy.strict === strict, strict.strict === strict.
    install_readonly_value(
        runtime,
        legacy,
        "strict",
        RegisterValue::from_object_handle(strict.0),
    )?;
    install_readonly_value(
        runtime,
        strict,
        "strict",
        RegisterValue::from_object_handle(strict.0),
    )?;

    runtime.install_global_value(
        ASSERT_EXPORT_SLOT,
        RegisterValue::from_object_handle(legacy.0),
    );
    runtime.install_global_value(
        ASSERT_STRICT_EXPORT_SLOT,
        RegisterValue::from_object_handle(strict.0),
    );
    Ok(())
}

/// Build a callable assert function with all methods attached.
fn build_assert_object(runtime: &mut RuntimeState, strict: bool) -> Result<ObjectHandle, String> {
    // The assert export itself is a callable function (equivalent to assert.ok).
    let descriptor = NativeFunctionDescriptor::method("assert", 1, assert_ok_impl);
    let function = runtime
        .alloc_host_function_from_descriptor(descriptor)
        .map_err(|error| format!("failed to create assert function: {error}"))?;

    // Install ok (same as the function itself, but also as a named property).
    install_method(runtime, function, "ok", 1, assert_ok_impl, "assert.ok")?;
    install_method(
        runtime,
        function,
        "fail",
        1,
        assert_fail_impl,
        "assert.fail",
    )?;
    install_method(
        runtime,
        function,
        "ifError",
        1,
        assert_if_error_impl,
        "assert.ifError",
    )?;

    // strictEqual / notStrictEqual always use SameValue.
    install_method(
        runtime,
        function,
        "strictEqual",
        2,
        assert_strict_equal_impl,
        "assert.strictEqual",
    )?;
    install_method(
        runtime,
        function,
        "notStrictEqual",
        2,
        assert_not_strict_equal_impl,
        "assert.notStrictEqual",
    )?;

    // deepStrictEqual / notDeepStrictEqual.
    install_method(
        runtime,
        function,
        "deepStrictEqual",
        2,
        assert_deep_strict_equal_impl,
        "assert.deepStrictEqual",
    )?;
    install_method(
        runtime,
        function,
        "notDeepStrictEqual",
        2,
        assert_not_deep_strict_equal_impl,
        "assert.notDeepStrictEqual",
    )?;

    // throws / doesNotThrow.
    install_method(
        runtime,
        function,
        "throws",
        2,
        assert_throws_impl,
        "assert.throws",
    )?;
    install_method(
        runtime,
        function,
        "doesNotThrow",
        1,
        assert_does_not_throw_impl,
        "assert.doesNotThrow",
    )?;

    // match / doesNotMatch.
    install_method(
        runtime,
        function,
        "match",
        2,
        assert_match_impl,
        "assert.match",
    )?;
    install_method(
        runtime,
        function,
        "doesNotMatch",
        2,
        assert_does_not_match_impl,
        "assert.doesNotMatch",
    )?;

    // equal / notEqual / deepEqual / notDeepEqual depend on strict mode.
    if strict {
        install_method(
            runtime,
            function,
            "equal",
            2,
            assert_strict_equal_impl,
            "assert.equal",
        )?;
        install_method(
            runtime,
            function,
            "notEqual",
            2,
            assert_not_strict_equal_impl,
            "assert.notEqual",
        )?;
        install_method(
            runtime,
            function,
            "deepEqual",
            2,
            assert_deep_strict_equal_impl,
            "assert.deepEqual",
        )?;
        install_method(
            runtime,
            function,
            "notDeepEqual",
            2,
            assert_not_deep_strict_equal_impl,
            "assert.notDeepEqual",
        )?;
    } else {
        install_method(
            runtime,
            function,
            "equal",
            2,
            assert_loose_equal_impl,
            "assert.equal",
        )?;
        install_method(
            runtime,
            function,
            "notEqual",
            2,
            assert_loose_not_equal_impl,
            "assert.notEqual",
        )?;
        // Legacy deepEqual is the same as deepStrictEqual in our implementation.
        install_method(
            runtime,
            function,
            "deepEqual",
            2,
            assert_deep_strict_equal_impl,
            "assert.deepEqual",
        )?;
        install_method(
            runtime,
            function,
            "notDeepEqual",
            2,
            assert_not_deep_strict_equal_impl,
            "assert.notDeepEqual",
        )?;
    }

    Ok(function)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn arg(args: &[RegisterValue], index: usize) -> RegisterValue {
    args.get(index)
        .copied()
        .unwrap_or_else(RegisterValue::undefined)
}

fn assertion_error(runtime: &mut RuntimeState, message: &str) -> VmNativeCallError {
    match runtime.alloc_type_error(message) {
        Ok(error) => {
            let name_prop = runtime.intern_property_name("name");
            let name_val = string_value(runtime, "AssertionError [ERR_ASSERTION]");
            let _ = runtime
                .objects_mut()
                .set_property(error, name_prop, name_val);
            let code_prop = runtime.intern_property_name("code");
            let code_val = string_value(runtime, "ERR_ASSERTION");
            let _ = runtime
                .objects_mut()
                .set_property(error, code_prop, code_val);
            VmNativeCallError::Thrown(RegisterValue::from_object_handle(error.0))
        }
        Err(_) => VmNativeCallError::Internal(message.into()),
    }
}

fn is_truthy(value: RegisterValue) -> bool {
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return false;
    }
    if let Some(b) = value.as_bool() {
        return b;
    }
    if let Some(n) = value.as_number() {
        return n != 0.0 && !n.is_nan();
    }
    // Objects, non-empty strings, etc. are truthy.
    true
}

// ---------------------------------------------------------------------------
// assert.ok / assert(value)
// ---------------------------------------------------------------------------

fn assert_ok_impl(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = arg(args, 0);
    if !is_truthy(value) {
        let message = if args.len() > 1 && arg(args, 1) != RegisterValue::undefined() {
            value_to_string(runtime, arg(args, 1))
        } else {
            let rendered = value_to_string(runtime, value);
            format!("The expression evaluated to a falsy value: {rendered}")
        };
        return Err(assertion_error(runtime, &message));
    }
    Ok(RegisterValue::undefined())
}

// ---------------------------------------------------------------------------
// assert.fail
// ---------------------------------------------------------------------------

fn assert_fail_impl(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let message = if args.is_empty() || arg(args, 0) == RegisterValue::undefined() {
        "Failed".to_string()
    } else {
        value_to_string(runtime, arg(args, 0))
    };
    Err(assertion_error(runtime, &message))
}

// ---------------------------------------------------------------------------
// assert.ifError
// ---------------------------------------------------------------------------

fn assert_if_error_impl(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = arg(args, 0);
    if value != RegisterValue::undefined() && value != RegisterValue::null() {
        let rendered = value_to_string(runtime, value);
        return Err(assertion_error(
            runtime,
            &format!("ifError got unwanted exception: {rendered}"),
        ));
    }
    Ok(RegisterValue::undefined())
}

// ---------------------------------------------------------------------------
// assert.strictEqual / assert.notStrictEqual (SameValue)
// ---------------------------------------------------------------------------

fn assert_strict_equal_impl(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let actual = arg(args, 0);
    let expected = arg(args, 1);
    let equal = same_value(runtime.objects(), actual, expected).unwrap_or(false);
    if !equal {
        let message = if args.len() > 2 && arg(args, 2) != RegisterValue::undefined() {
            value_to_string(runtime, arg(args, 2))
        } else {
            let actual_str = value_to_string(runtime, actual);
            let expected_str = value_to_string(runtime, expected);
            format!(
                "Expected values to be strictly equal:\n  actual: {actual_str}\n  expected: {expected_str}"
            )
        };
        return Err(assertion_error(runtime, &message));
    }
    Ok(RegisterValue::undefined())
}

fn assert_not_strict_equal_impl(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let actual = arg(args, 0);
    let expected = arg(args, 1);
    let equal = same_value(runtime.objects(), actual, expected).unwrap_or(false);
    if equal {
        let message = if args.len() > 2 && arg(args, 2) != RegisterValue::undefined() {
            value_to_string(runtime, arg(args, 2))
        } else {
            let actual_str = value_to_string(runtime, actual);
            format!("Expected values to be strictly different: {actual_str}")
        };
        return Err(assertion_error(runtime, &message));
    }
    Ok(RegisterValue::undefined())
}

// ---------------------------------------------------------------------------
// assert.equal / assert.notEqual (loose ==, legacy only)
// ---------------------------------------------------------------------------

/// Legacy assert.equal uses == (Abstract Equality). Since IsLooselyEqual is
/// not publicly exposed on RuntimeState, we approximate with IsStrictlyEqual.
/// Legacy assert.equal is deprecated in Node.js — callers should use
/// assert.strictEqual or the strict module.
fn assert_loose_equal_impl(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let actual = arg(args, 0);
    let expected = arg(args, 1);
    let equal = is_strictly_equal(runtime.objects(), actual, expected).unwrap_or(false);
    if !equal {
        let message = if args.len() > 2 && arg(args, 2) != RegisterValue::undefined() {
            value_to_string(runtime, arg(args, 2))
        } else {
            let actual_str = value_to_string(runtime, actual);
            let expected_str = value_to_string(runtime, expected);
            format!(
                "Expected values to be loosely equal:\n  actual: {actual_str}\n  expected: {expected_str}"
            )
        };
        return Err(assertion_error(runtime, &message));
    }
    Ok(RegisterValue::undefined())
}

fn assert_loose_not_equal_impl(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let actual = arg(args, 0);
    let expected = arg(args, 1);
    let equal = is_strictly_equal(runtime.objects(), actual, expected).unwrap_or(false);
    if equal {
        let message = if args.len() > 2 && arg(args, 2) != RegisterValue::undefined() {
            value_to_string(runtime, arg(args, 2))
        } else {
            let actual_str = value_to_string(runtime, actual);
            format!("Expected values to be loosely different: {actual_str}")
        };
        return Err(assertion_error(runtime, &message));
    }
    Ok(RegisterValue::undefined())
}

// ---------------------------------------------------------------------------
// assert.deepStrictEqual / assert.notDeepStrictEqual
// ---------------------------------------------------------------------------

/// Recursive deep strict equality with depth limit.
fn deep_strict_equal(
    runtime: &mut RuntimeState,
    actual: RegisterValue,
    expected: RegisterValue,
    depth: usize,
) -> bool {
    const MAX_DEPTH: usize = 64;
    if depth > MAX_DEPTH {
        return false;
    }

    // Primitive SameValue check first.
    if same_value(runtime.objects(), actual, expected).unwrap_or(false) {
        return true;
    }

    // Both must be objects to continue.
    let (Some(actual_handle_raw), Some(expected_handle_raw)) =
        (actual.as_object_handle(), expected.as_object_handle())
    else {
        return false;
    };
    let actual_handle = ObjectHandle(actual_handle_raw);
    let expected_handle = ObjectHandle(expected_handle_raw);

    // Check if both are String objects — compare content.
    let actual_str = runtime.objects().string_value(actual_handle).ok().flatten();
    let expected_str = runtime
        .objects()
        .string_value(expected_handle)
        .ok()
        .flatten();
    if let (Some(a), Some(b)) = (actual_str, expected_str) {
        return a == b;
    }

    // Arrays: compare length and elements.
    let actual_len = runtime.objects().array_length(actual_handle).ok().flatten();
    let expected_len = runtime
        .objects()
        .array_length(expected_handle)
        .ok()
        .flatten();
    if let (Some(a_len), Some(e_len)) = (actual_len, expected_len) {
        if a_len != e_len {
            return false;
        }
        for i in 0..a_len {
            let a_elem = runtime
                .objects_mut()
                .get_index(actual_handle, i)
                .ok()
                .flatten()
                .unwrap_or_else(RegisterValue::undefined);
            let e_elem = runtime
                .objects_mut()
                .get_index(expected_handle, i)
                .ok()
                .flatten()
                .unwrap_or_else(RegisterValue::undefined);
            if !deep_strict_equal(runtime, a_elem, e_elem, depth + 1) {
                return false;
            }
        }
        // Also compare named properties.
    }

    // Compare own property keys.
    let actual_keys = runtime
        .objects()
        .own_keys(actual_handle)
        .unwrap_or_default();
    let expected_keys = runtime
        .objects()
        .own_keys(expected_handle)
        .unwrap_or_default();
    if actual_keys.len() != expected_keys.len() {
        return false;
    }
    for key in &actual_keys {
        if !expected_keys.contains(key) {
            return false;
        }
        let a_val = runtime
            .own_property_value(actual_handle, *key)
            .unwrap_or_else(|_| RegisterValue::undefined());
        let e_val = runtime
            .own_property_value(expected_handle, *key)
            .unwrap_or_else(|_| RegisterValue::undefined());
        if !deep_strict_equal(runtime, a_val, e_val, depth + 1) {
            return false;
        }
    }

    true
}

fn assert_deep_strict_equal_impl(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let actual = arg(args, 0);
    let expected = arg(args, 1);
    if !deep_strict_equal(runtime, actual, expected, 0) {
        let message = if args.len() > 2 && arg(args, 2) != RegisterValue::undefined() {
            value_to_string(runtime, arg(args, 2))
        } else {
            let actual_str = value_to_string(runtime, actual);
            let expected_str = value_to_string(runtime, expected);
            format!(
                "Expected values to be deeply strictly equal:\n  actual: {actual_str}\n  expected: {expected_str}"
            )
        };
        return Err(assertion_error(runtime, &message));
    }
    Ok(RegisterValue::undefined())
}

fn assert_not_deep_strict_equal_impl(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let actual = arg(args, 0);
    let expected = arg(args, 1);
    if deep_strict_equal(runtime, actual, expected, 0) {
        let message = if args.len() > 2 && arg(args, 2) != RegisterValue::undefined() {
            value_to_string(runtime, arg(args, 2))
        } else {
            let actual_str = value_to_string(runtime, actual);
            format!("Expected values to NOT be deeply strictly equal: {actual_str}")
        };
        return Err(assertion_error(runtime, &message));
    }
    Ok(RegisterValue::undefined())
}

// ---------------------------------------------------------------------------
// assert.throws / assert.doesNotThrow
// ---------------------------------------------------------------------------

fn assert_throws_impl(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let block = arg(args, 0);
    let Some(block_handle) = block.as_object_handle().map(ObjectHandle) else {
        return Err(type_error(
            runtime,
            "The \"fn\" argument must be of type Function",
        ));
    };
    if !runtime.objects().is_callable(block_handle) {
        return Err(type_error(
            runtime,
            "The \"fn\" argument must be of type Function",
        ));
    }

    let expected = arg(args, 1);
    let result = runtime.call_callable(block_handle, RegisterValue::undefined(), &[]);

    match result {
        Err(VmNativeCallError::Thrown(thrown)) => {
            // If there's an expected error constructor, validate the thrown value.
            if expected != RegisterValue::undefined() && expected.as_object_handle().is_some() {
                let expected_handle = ObjectHandle(expected.as_object_handle().unwrap());
                if runtime.objects().is_callable(expected_handle) {
                    // Check if thrown is an instance of expected (prototype check).
                    if let Some(thrown_handle) = thrown.as_object_handle().map(ObjectHandle) {
                        let is_instance =
                            check_instance_of(runtime, thrown_handle, expected_handle);
                        if !is_instance {
                            let thrown_str = value_to_string(runtime, thrown);
                            return Err(assertion_error(
                                runtime,
                                &format!(
                                    "The error thrown did not match the expected type. Thrown: {thrown_str}"
                                ),
                            ));
                        }
                    }
                }
            }
            Ok(RegisterValue::undefined())
        }
        Err(error) => Err(error),
        Ok(_) => {
            let message = if args.len() > 2 && arg(args, 2) != RegisterValue::undefined() {
                value_to_string(runtime, arg(args, 2))
            } else {
                "Missing expected exception".to_string()
            };
            Err(assertion_error(runtime, &message))
        }
    }
}

fn assert_does_not_throw_impl(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let block = arg(args, 0);
    let Some(block_handle) = block.as_object_handle().map(ObjectHandle) else {
        return Err(type_error(
            runtime,
            "The \"fn\" argument must be of type Function",
        ));
    };
    if !runtime.objects().is_callable(block_handle) {
        return Err(type_error(
            runtime,
            "The \"fn\" argument must be of type Function",
        ));
    }

    match runtime.call_callable(block_handle, RegisterValue::undefined(), &[]) {
        Ok(_) => Ok(RegisterValue::undefined()),
        Err(VmNativeCallError::Thrown(thrown)) => {
            let thrown_str = value_to_string(runtime, thrown);
            let message = if args.len() > 1 && arg(args, 1) != RegisterValue::undefined() {
                value_to_string(runtime, arg(args, 1))
            } else {
                format!("Got unwanted exception: {thrown_str}")
            };
            Err(assertion_error(runtime, &message))
        }
        Err(error) => Err(error),
    }
}

// ---------------------------------------------------------------------------
// assert.match / assert.doesNotMatch
// ---------------------------------------------------------------------------

fn assert_match_impl(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let string_val = arg(args, 0);
    let regexp_val = arg(args, 1);
    let Some(regexp_handle) = regexp_val.as_object_handle().map(ObjectHandle) else {
        return Err(type_error(
            runtime,
            "The \"regexp\" argument must be an instance of RegExp",
        ));
    };

    // Call regexp.test(string).
    let test_prop = runtime.intern_property_name("test");
    let test_fn = runtime
        .own_property_value(regexp_handle, test_prop)
        .map_err(|_| type_error(runtime, "RegExp.prototype.test is not available"))?;
    let Some(test_handle) = test_fn.as_object_handle().map(ObjectHandle) else {
        return Err(type_error(
            runtime,
            "RegExp.prototype.test is not a function",
        ));
    };
    let result = runtime.call_callable(test_handle, regexp_val, &[string_val])?;

    if result.as_bool() != Some(true) {
        let message = if args.len() > 2 && arg(args, 2) != RegisterValue::undefined() {
            value_to_string(runtime, arg(args, 2))
        } else {
            let string_str = value_to_string(runtime, string_val);
            let regexp_str = value_to_string(runtime, regexp_val);
            format!(
                "The input did not match the regular expression {regexp_str}. Input: '{string_str}'"
            )
        };
        return Err(assertion_error(runtime, &message));
    }
    Ok(RegisterValue::undefined())
}

fn assert_does_not_match_impl(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let string_val = arg(args, 0);
    let regexp_val = arg(args, 1);
    let Some(regexp_handle) = regexp_val.as_object_handle().map(ObjectHandle) else {
        return Err(type_error(
            runtime,
            "The \"regexp\" argument must be an instance of RegExp",
        ));
    };

    let test_prop = runtime.intern_property_name("test");
    let test_fn = runtime
        .own_property_value(regexp_handle, test_prop)
        .map_err(|_| type_error(runtime, "RegExp.prototype.test is not available"))?;
    let Some(test_handle) = test_fn.as_object_handle().map(ObjectHandle) else {
        return Err(type_error(
            runtime,
            "RegExp.prototype.test is not a function",
        ));
    };
    let result = runtime.call_callable(test_handle, regexp_val, &[string_val])?;

    if result.as_bool() == Some(true) {
        let message = if args.len() > 2 && arg(args, 2) != RegisterValue::undefined() {
            value_to_string(runtime, arg(args, 2))
        } else {
            let string_str = value_to_string(runtime, string_val);
            let regexp_str = value_to_string(runtime, regexp_val);
            format!(
                "The input was expected to not match the regular expression {regexp_str}. Input: '{string_str}'"
            )
        };
        return Err(assertion_error(runtime, &message));
    }
    Ok(RegisterValue::undefined())
}

// ---------------------------------------------------------------------------
// Instance-of check (prototype chain walk)
// ---------------------------------------------------------------------------

fn check_instance_of(
    runtime: &mut RuntimeState,
    value: ObjectHandle,
    constructor: ObjectHandle,
) -> bool {
    // Get constructor.prototype.
    let proto_prop = runtime.intern_property_name("prototype");
    let Ok(ctor_proto) = runtime.own_property_value(constructor, proto_prop) else {
        return false;
    };
    let Some(ctor_proto_handle) = ctor_proto.as_object_handle().map(ObjectHandle) else {
        return false;
    };

    // Walk the prototype chain of value.
    let mut current = Some(value);
    for _ in 0..256 {
        let Some(handle) = current else {
            return false;
        };
        let Ok(proto) = runtime.objects().get_prototype(handle) else {
            return false;
        };
        let Some(proto_handle) = proto else {
            return false;
        };
        if proto_handle == ctor_proto_handle {
            return true;
        }
        current = Some(proto_handle);
    }
    false
}

fn read_global_slot(runtime: &mut RuntimeState, slot: &str) -> Result<RegisterValue, String> {
    let global = runtime.intrinsics().global_object();
    let property = runtime.intern_property_name(slot);
    let value = runtime
        .own_property_value(global, property)
        .map_err(|error| format!("failed to read global slot '{slot}': {error:?}"))?;
    if value == RegisterValue::undefined() {
        return Err(format!("global slot '{slot}' is undefined"));
    }
    Ok(value)
}
