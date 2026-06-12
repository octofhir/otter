//! Minimal `node:assert` / `assert` hosted module.
//!
//! The CommonJS export is a *callable* (`assert(value[, message])`) that also
//! carries the assertion methods (`strictEqual`, `deepStrictEqual`, `throws`,
//! ...). Because the hosted-module object-namespace path cannot represent a
//! callable, assert registers a `cjs_value` installer that builds a callable
//! host object. The ESM namespace ([`install_assert_module`]) exposes the same
//! methods as plain properties.
//!
//! # Invariants
//! - Comparison uses the VM abstract operations (`is_strictly_equal`,
//!   `is_loosely_equal`) so semantics match the engine's `===` / `==`.
//! - Failures throw so the caller's `try`/`catch` (and the test harness) observe
//!   them. AssertionError fidelity (error subclass, `actual`/`expected` props) is
//!   a follow-up; the message carries the detail today.

use otter_gc::GcHeap;
use otter_runtime::CapabilitySet;
use otter_runtime::module_scope::ModuleScope;
use otter_vm::{NativeCtx, NativeError, Value, abstract_ops};

/// Build the ESM namespace object for `node:assert` (methods as properties; the
/// namespace itself is not callable). CommonJS `require` uses [`assert_cjs_value`].
pub fn install_assert_module(ctx: &mut otter_runtime::HostedModuleCtx<'_>) -> Result<(), String> {
    for (name, len, f) in ASSERT_METHODS {
        ctx.builtin_method(name, *len, *f)?;
    }
    Ok(())
}

/// Build the callable CommonJS export: a host object whose `[[Call]]` runs
/// `assert(value, message)` and which carries the assertion methods. Built via
/// [`ModuleScope`], which handles moving-GC rooting (no manual root juggling).
pub fn assert_cjs_value(
    ctx: &mut NativeCtx<'_>,
    _capabilities: &CapabilitySet,
) -> Result<Value, String> {
    let mut scope = ModuleScope::new(ctx);
    let export = scope.callable("assert", 2, assert_ok, ASSERT_METHODS)?;
    // `assert.strict` aliases assert itself (close enough until a separate
    // strict variant lands).
    scope.set(export, "strict", export);
    Ok(scope.finish(export))
}

type Method = (
    &'static str,
    u8,
    fn(&mut NativeCtx<'_>, &[Value]) -> Result<Value, NativeError>,
);

const ASSERT_METHODS: &[Method] = &[
    ("ok", 1, assert_ok),
    ("strictEqual", 2, assert_strict_equal),
    ("notStrictEqual", 2, assert_not_strict_equal),
    ("equal", 2, assert_equal),
    ("notEqual", 2, assert_not_equal),
    ("deepStrictEqual", 2, assert_deep_strict_equal),
    ("notDeepStrictEqual", 2, assert_not_deep_strict_equal),
    ("deepEqual", 2, assert_deep_strict_equal),
    ("notDeepEqual", 2, assert_not_deep_strict_equal),
    ("throws", 2, assert_throws),
    ("doesNotThrow", 2, assert_does_not_throw),
    ("ifError", 1, assert_if_error),
    ("fail", 1, assert_fail),
];

fn fail(message: impl Into<String>) -> NativeError {
    NativeError::Thrown {
        name: "assert",
        message: message.into(),
    }
}

fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).copied().unwrap_or_else(Value::undefined)
}

/// JS truthiness (`ToBoolean`).
fn is_truthy(v: &Value, heap: &GcHeap) -> bool {
    if v.is_nullish() {
        return false;
    }
    if let Some(b) = v.as_boolean() {
        return b;
    }
    if let Some(n) = v.as_number() {
        let f = n.as_f64();
        return f != 0.0 && !f.is_nan();
    }
    if let Some(s) = v.as_string(heap) {
        return !s.to_lossy_string(heap).is_empty();
    }
    true
}

fn assert_ok(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let value = arg(args, 0);
    if is_truthy(&value, ctx.heap()) {
        Ok(Value::undefined())
    } else {
        Err(fail("assertion failed: value is not truthy"))
    }
}

/// Build a comparison failure. If the caller passed an explicit message
/// (3rd argument), use it (Node behaviour); otherwise render actual/expected.
fn cmp_fail(ctx: &NativeCtx<'_>, op: &str, args: &[Value], a: &Value, b: &Value) -> NativeError {
    let heap = ctx.heap();
    if let Some(msg) = args.get(2).filter(|v| !v.is_undefined()) {
        return fail(msg.display_string(heap));
    }
    fail(format!(
        "Expected values to be {op}:\n  actual:   {}\n  expected: {}",
        a.display_string(heap),
        b.display_string(heap)
    ))
}

fn assert_strict_equal(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let (a, b) = (arg(args, 0), arg(args, 1));
    if abstract_ops::is_strictly_equal(&a, &b, ctx.heap()) {
        Ok(Value::undefined())
    } else {
        Err(cmp_fail(ctx, "strictly equal", args, &a, &b))
    }
}

fn assert_not_strict_equal(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let (a, b) = (arg(args, 0), arg(args, 1));
    if abstract_ops::is_strictly_equal(&a, &b, ctx.heap()) {
        Err(fail("assert.notStrictEqual: values are strictly equal"))
    } else {
        Ok(Value::undefined())
    }
}

fn assert_equal(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let (a, b) = (arg(args, 0), arg(args, 1));
    if abstract_ops::is_loosely_equal(&a, &b, ctx.heap()) {
        Ok(Value::undefined())
    } else {
        Err(fail("assert.equal: values are not loosely equal"))
    }
}

fn assert_not_equal(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let (a, b) = (arg(args, 0), arg(args, 1));
    if abstract_ops::is_loosely_equal(&a, &b, ctx.heap()) {
        Err(fail("assert.notEqual: values are loosely equal"))
    } else {
        Ok(Value::undefined())
    }
}

/// Structural deep equality over primitives and arrays. Plain-object key
/// comparison is a follow-up (no public key-enumeration helper wired yet);
/// objects currently compare by reference.
fn deep_equal(a: &Value, b: &Value, heap: &GcHeap) -> bool {
    if abstract_ops::is_strictly_equal(a, b, heap) {
        return true;
    }
    if let (Some(xa), Some(xb)) = (a.as_array(), b.as_array()) {
        let (la, lb) = (
            otter_vm::array::len(xa, heap),
            otter_vm::array::len(xb, heap),
        );
        if la != lb {
            return false;
        }
        for i in 0..la {
            let ea = otter_vm::array::get(xa, heap, i);
            let eb = otter_vm::array::get(xb, heap, i);
            if !deep_equal(&ea, &eb, heap) {
                return false;
            }
        }
        return true;
    }
    false
}

fn assert_deep_strict_equal(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let (a, b) = (arg(args, 0), arg(args, 1));
    if deep_equal(&a, &b, ctx.heap()) {
        Ok(Value::undefined())
    } else {
        Err(fail("assert.deepStrictEqual: values are not deeply equal"))
    }
}

fn assert_not_deep_strict_equal(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let (a, b) = (arg(args, 0), arg(args, 1));
    if deep_equal(&a, &b, ctx.heap()) {
        Err(fail("assert.notDeepStrictEqual: values are deeply equal"))
    } else {
        Ok(Value::undefined())
    }
}

fn assert_throws(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let callee = arg(args, 0);
    if !otter_vm::is_callable_value(&callee) {
        return Err(fail("assert.throws: first argument must be a function"));
    }
    let expected = arg(args, 1);
    let (interp, context) = ctx.interp_mut_and_context();
    let Some(context) = context else {
        return Err(fail("assert.throws: no execution context"));
    };
    if interp
        .run_callable_sync(&context, &callee, Value::undefined(), Default::default())
        .is_ok()
    {
        return Err(fail("Missing expected exception."));
    }
    // Recover the real thrown Error value and validate it against the matcher.
    let thrown = interp
        .take_pending_uncaught_throw()
        .unwrap_or_else(Value::undefined);
    let depth = interp.push_module_root(thrown);
    let thrown = interp.module_root(depth - 1);
    let result = validate_thrown(interp, &context, thrown, expected);
    interp.pop_module_roots_to(depth - 1);
    result.map(|()| Value::undefined())
}

/// Validate a thrown value against `assert.throws`'s expected matcher.
/// Supports: object matchers (`{ code, name, message }`), error constructors
/// (matched by `name`), and ignores a string second argument (it is the
/// assertion message, not a matcher).
fn validate_thrown(
    interp: &mut otter_vm::Interpreter,
    context: &otter_vm::ExecutionContext,
    thrown: Value,
    expected: Value,
) -> Result<(), NativeError> {
    // No matcher, or a plain message string: any throw satisfies it.
    if expected.is_undefined() || expected.is_string() {
        return Ok(());
    }
    let get = |interp: &mut otter_vm::Interpreter, recv: Value, key: &str| -> Value {
        interp
            .get_property(context, recv, key)
            .unwrap_or_else(|_| Value::undefined())
    };
    if expected.is_object() && otter_vm::is_callable_value(&expected) {
        // Error constructor: match by class name.
        let want = get(interp, expected, "name");
        let got = get(interp, thrown, "name");
        if abstract_ops::is_strictly_equal(&want, &got, interp.gc_heap()) {
            return Ok(());
        }
        return Err(fail(format!(
            "The error is expected to be an instance of \"{}\". Received \"{}\"",
            want.display_string(interp.gc_heap()),
            got.display_string(interp.gc_heap()),
        )));
    }
    if expected.is_object() {
        // Object matcher: every checked key must strictly match.
        for key in ["name", "code", "message"] {
            let want = get(interp, expected, key);
            if want.is_undefined() {
                continue;
            }
            let got = get(interp, thrown, key);
            if !abstract_ops::is_strictly_equal(&want, &got, interp.gc_heap()) {
                return Err(fail(format!(
                    "error.{key} mismatch: expected {}, got {}",
                    want.display_string(interp.gc_heap()),
                    got.display_string(interp.gc_heap()),
                )));
            }
        }
        return Ok(());
    }
    Ok(())
}

fn assert_does_not_throw(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let callee = arg(args, 0);
    if !otter_vm::is_callable_value(&callee) {
        return Err(fail(
            "assert.doesNotThrow: first argument must be a function",
        ));
    }
    let (interp, context) = ctx.interp_mut_and_context();
    let Some(context) = context else {
        return Err(fail("assert.doesNotThrow: no execution context"));
    };
    match interp.run_callable_sync(&context, &callee, Value::undefined(), Default::default()) {
        Ok(_) => Ok(Value::undefined()),
        Err(_) => Err(fail("assert.doesNotThrow: got unwanted exception")),
    }
}

fn assert_if_error(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let value = arg(args, 0);
    let _ = ctx;
    if value.is_nullish() {
        Ok(Value::undefined())
    } else {
        Err(fail("assert.ifError: got an error value"))
    }
}

fn assert_fail(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Err(fail("assert.fail: failed"))
}
