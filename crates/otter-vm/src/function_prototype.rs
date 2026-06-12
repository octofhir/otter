//! `Function.prototype` VM-owned builtin methods.
//!
//! The methods in this module are installed as JS-visible function
//! values, but their call semantics are owned by the interpreter:
//! `call`, `apply`, and `bind` need to invoke arbitrary JS callables
//! on the current VM stack. They therefore use
//! [`crate::VmIntrinsicFunction`] rather than the host-native
//! [`crate::NativeCtx`] boundary.
//!
//! # Contents
//! - [`FUNCTION_PROTOTYPE_METHODS`] — static specs installed during
//!   bootstrap.
//!
//! # Invariants
//! - The JS-visible function values are static-spec declarations.
//! - Invocation is routed by the VM dispatch loop, not by host
//!   native code re-entering the interpreter through `NativeCtx`.
//! - Methods use standard builtin function attributes.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-function-prototype-object>
//! - [JS surface builders](../../../docs/book/src/extensions/js-surface-builders.md)

use crate::js_surface::{Attr, JsSurfaceError, MethodSpec};
use crate::native_function::NativeFunction;
use crate::object::{self, JsObject, PropertyDescriptor};
use crate::symbol;
use crate::{NativeCall, NativeCtx, NativeError, Value, VmIntrinsicFunction};

/// Static `Function.prototype` method specs.
pub static FUNCTION_PROTOTYPE_METHODS: &[MethodSpec] = &[
    intrinsic_method("call", 1, VmIntrinsicFunction::FunctionPrototypeCall),
    intrinsic_method("apply", 2, VmIntrinsicFunction::FunctionPrototypeApply),
    intrinsic_method("bind", 1, VmIntrinsicFunction::FunctionPrototypeBind),
    intrinsic_method(
        "toString",
        0,
        VmIntrinsicFunction::FunctionPrototypeToString,
    ),
];

/// Install `Function.prototype[@@hasInstance]` per §20.2.3.6.
///
/// The property's attributes are `{ [[Writable]]: false,
/// [[Enumerable]]: false, [[Configurable]]: false }` — the only
/// non-configurable Function.prototype data slot in the spec.
/// The function value carries the canonical native `name` of
/// `"[Symbol.hasInstance]"` and `length` of `1`.
pub(crate) fn install_symbol_has_instance(
    heap: &mut otter_gc::GcHeap,
    prototype: JsObject,
    well_known_has_instance: symbol::JsSymbol,
    value_roots: &[&Value],
) -> Result<(), JsSurfaceError> {
    let prototype_root = Value::object(prototype);
    let mut roots = Vec::with_capacity(value_roots.len() + 1);
    roots.push(&prototype_root);
    roots.extend_from_slice(value_roots);
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        for value in &roots {
            value.trace_value_slots(visitor);
        }
    };
    let value = NativeFunction::from_call_with_roots(
        heap,
        "[Symbol.hasInstance]",
        1,
        NativeCall::VmIntrinsic(VmIntrinsicFunction::FunctionPrototypeSymbolHasInstance),
        &mut external_visit,
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let descriptor = PropertyDescriptor::data(Value::native_function(value), false, false, false);
    if !object::define_own_symbol_property(prototype, heap, well_known_has_instance, descriptor) {
        return Err(JsSurfaceError::DefinePropertyFailed("[Symbol.hasInstance]"));
    }
    Ok(())
}

const fn intrinsic_method(
    name: &'static str,
    length: u8,
    intrinsic: VmIntrinsicFunction,
) -> MethodSpec {
    MethodSpec {
        name,
        length,
        attrs: Attr::builtin_function(),
        call: NativeCall::VmIntrinsic(intrinsic),
    }
}

/// Install `caller` and `arguments` per AddRestrictedFunctionProperties.
///
/// Both properties share the same getter and setter function object:
/// the realm's `%ThrowTypeError%` intrinsic.
pub(crate) fn install_restricted_accessors(
    heap: &mut otter_gc::GcHeap,
    prototype: JsObject,
    value_roots: &[&Value],
) -> Result<(), JsSurfaceError> {
    let prototype_root = Value::object(prototype);
    let mut roots = Vec::with_capacity(value_roots.len() + 1);
    roots.push(&prototype_root);
    roots.extend_from_slice(value_roots);
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        for value in &roots {
            value.trace_value_slots(visitor);
        }
    };
    let thrower = Value::native_function(NativeFunction::throw_type_error_with_roots(
        heap,
        throw_restricted_function_property,
        &mut external_visit,
    )?);
    for name in ["caller", "arguments"] {
        let descriptor = PropertyDescriptor::accessor(Some(thrower), Some(thrower), false, true);
        if !object::define_own_property(prototype, heap, name, descriptor) {
            return Err(JsSurfaceError::DefinePropertyFailed(name));
        }
    }
    Ok(())
}

fn throw_restricted_function_property(
    ctx: &mut NativeCtx<'_>,
    _: &[Value],
) -> Result<Value, NativeError> {
    // ES legacy `caller` is normative-optional. Otter does not expose
    // a stack-sensitive caller object yet; return `undefined` for
    // unsupported non-strict ordinary functions while strict
    // functions, bound functions and natives keep the %ThrowTypeError%
    // poisoning.
    let receiver = *ctx.this_value();
    let fid = receiver.as_function().or_else(|| {
        receiver
            .as_closure(ctx.heap())
            .map(|c| c.cached_function_id)
    });
    if let (Some(fid), Some(exec)) = (fid, ctx.execution_context().cloned())
        && !exec.function_is_strict(fid)
        && !exec.function_is_arrow(fid)
        && exec
            .function(fid)
            .is_some_and(|f| !f.is_generator && !f.is_async)
    {
        return Ok(Value::undefined());
    }
    Err(NativeError::TypeError {
        name: "%ThrowTypeError%",
        reason: "restricted function property access".to_string(),
    })
}
