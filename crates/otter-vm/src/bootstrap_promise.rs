//! ECMA-262 §27.2 Promise bootstrap installer.
//!
//! Installs the JS-visible `Promise` constructor with all 7 static
//! methods (`resolve` / `reject` / `all` / `race` / `allSettled` /
//! `any` / `withResolvers`) and a prototype carrying `then` /
//! `catch` / `finally`. The constructor delegates to the existing
//! [`crate::promise_dispatch`] dispatcher; the prototype is linked
//! to `%Object.prototype%` and gets `@@toStringTag = "Promise"`
//! installed in
//! [`install_promise_well_knowns_post_bootstrap`].
//!
//! # Contents
//! - [`install_promise`] — bootstrap entry point.
//! - [`install_promise_well_knowns_post_bootstrap`] — `@@toStringTag`
//!   fixup that runs once the per-realm `WellKnownSymbols` table
//!   exists.
//!
//! # Invariants
//! - `new Promise(executor)` runs the executor synchronously with
//!   the realm's `resolve` / `reject` natives. If the executor
//!   throws, the captured rejection reason settles the promise.
//! - `Promise()` (no `new`) throws a `TypeError` per §27.2.3.1.
//! - All statics + prototype methods reuse
//!   [`crate::promise_dispatch::statics_call`] /
//!   [`crate::promise_dispatch::prototype_call`] so the
//!   microtask queue semantics stay identical to the dedicated
//!   `Op::PromiseCall` opcode path.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-promise-constructor>
//! - <https://tc39.es/ecma262/#sec-properties-of-the-promise-prototype-object>

use otter_bytecode::method_id::PromiseMethod;
use smallvec::SmallVec;

use crate::js_surface::{Attr, JsSurfaceError, ObjectBuilder};
use crate::native_function::{NativeCall, NativeFunction};
use crate::object::{self, JsObject, PartialPropertyDescriptor, PropertyDescriptor};
use crate::promise_dispatch::{self, PromiseBuilder};
use crate::{NativeCtx, NativeError, Value, VmError};

/// `BuiltinIntrinsic` adapter for the global `Promise` constructor.
pub struct Intrinsic;

impl crate::intrinsic_install::BuiltinIntrinsic for Intrinsic {
    const NAME: &'static str = "Promise";
    const FEATURE: crate::bootstrap::BootstrapFeatures = crate::bootstrap::BootstrapFeatures::CORE;

    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install(heap, global)
    }
}

/// §27.2 Promise — installer body, called through [`Intrinsic`].
fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
    // Prototype object linked to %Object.prototype%.
    let global_root = Value::Object(global);
    let prototype = crate::bootstrap::alloc_object_with_value_roots(heap, &[&global_root])?;
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(prototype, heap, Some(object_proto));
    }

    // §27.2.5 — `then` / `catch` / `finally` prototype methods.
    {
        let mut builder =
            ObjectBuilder::from_object_with_value_roots(heap, prototype, vec![global_root.clone()]);
        builder.method(
            "then",
            2,
            NativeCall::Static(promise_proto_then),
            Attr::builtin_function(),
        )?;
        builder.method(
            "catch",
            1,
            NativeCall::Static(promise_proto_catch),
            Attr::builtin_function(),
        )?;
        builder.method(
            "finally",
            1,
            NativeCall::Static(promise_proto_finally),
            Attr::builtin_function(),
        )?;
    }

    // §27.2.3 The Promise Constructor.
    let prototype_root = Value::Object(prototype);
    let ctor = crate::bootstrap::native_constructor_static_with_value_roots(
        heap,
        "Promise",
        1,
        promise_ctor_call,
        &[&global_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let string_heap = crate::string::StringHeap::default();

    // §27.2.3.1 — `Promise.prototype` own data property:
    // non-writable, non-enumerable, non-configurable.
    let proto_desc = PropertyDescriptor::data(Value::Object(prototype), false, false, false);
    if !ctor.define_own_property(heap, &string_heap, "prototype", proto_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("prototype"));
    }

    // §27.2.4 — static methods.
    let ctor_roots = vec![global_root.clone(), Value::Object(prototype)];
    define_ctor_method(
        heap,
        ctor,
        "resolve",
        1,
        promise_static_resolve,
        &ctor_roots,
    )?;
    define_ctor_method(heap, ctor, "reject", 1, promise_static_reject, &ctor_roots)?;
    define_ctor_method(heap, ctor, "all", 1, promise_static_all, &ctor_roots)?;
    define_ctor_method(heap, ctor, "race", 1, promise_static_race, &ctor_roots)?;
    define_ctor_method(
        heap,
        ctor,
        "allSettled",
        1,
        promise_static_all_settled,
        &ctor_roots,
    )?;
    define_ctor_method(heap, ctor, "any", 1, promise_static_any, &ctor_roots)?;
    define_ctor_method(
        heap,
        ctor,
        "withResolvers",
        0,
        promise_static_with_resolvers,
        &ctor_roots,
    )?;

    // §27.2.5.2 — `Promise.prototype.constructor` back-pointer.
    object::define_own_property(
        prototype,
        heap,
        "constructor",
        PropertyDescriptor::data(Value::NativeFunction(ctor), true, false, true),
    );

    crate::bootstrap::define_global_value(
        global,
        heap,
        <Intrinsic as crate::intrinsic_install::BuiltinIntrinsic>::NAME,
        Value::NativeFunction(ctor),
    );
    Ok(())
}

/// Install Promise well-known symbol properties.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-promise.prototype-@@tostringtag>
/// - <https://tc39.es/ecma262/#sec-get-promise-@@species>
pub fn install_promise_well_knowns_post_bootstrap(
    heap: &mut otter_gc::GcHeap,
    string_heap: &crate::string::StringHeap,
    global: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    use crate::symbol::WellKnown;

    let Some(Value::NativeFunction(ctor)) = object::get(global, heap, "Promise") else {
        return Ok(());
    };
    let global_root = Value::Object(global);
    let ctor_root = Value::NativeFunction(ctor);
    let species_getter = crate::bootstrap::native_static_with_value_roots(
        heap,
        "get [Symbol.species]",
        0,
        promise_species_get,
        &[&global_root, &ctor_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    if !ctor.define_own_symbol_property(
        heap,
        &well_known.get(WellKnown::Species),
        PartialPropertyDescriptor {
            get: Some(Value::NativeFunction(species_getter)),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    ) {
        return Err(JsSurfaceError::DefinePropertyFailed(
            "Promise[Symbol.species]",
        ));
    }

    let descriptor = ctor
        .own_property_descriptor(heap, string_heap, "prototype")
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let prototype = match descriptor.and_then(|d| match d.kind {
        crate::object::DescriptorKind::Data {
            value: Value::Object(p),
        } => Some(p),
        _ => None,
    }) {
        Some(p) => p,
        None => return Ok(()),
    };
    let tag = crate::string::JsString::from_str("Promise", string_heap)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    object::define_own_symbol_property_partial(
        prototype,
        heap,
        &well_known.get(WellKnown::ToStringTag),
        PartialPropertyDescriptor {
            value: Some(Value::String(tag)),
            writable: Some(false),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    );
    Ok(())
}

/// §27.2.4.10 `get Promise[@@species]`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-get-promise-@@species>
fn promise_species_get(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(ctx.this_value().clone())
}

// ---------------------------------------------------------------
// Constructor body
// ---------------------------------------------------------------

/// §27.2.3.1 `Promise(executor)`.
///
/// 1. If NewTarget is undefined, throw a TypeError.
/// 2. If IsCallable(executor) is false, throw a TypeError.
/// 3. Allocate a fresh pending promise + native resolve/reject.
/// 4. Invoke executor with `[resolve, reject]` synchronously.
///    If the executor throws, settle the promise via the captured
///    `reject` (idempotent — if the executor already resolved
///    before throwing, the reject is a spec-mandated no-op).
fn promise_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    if !ctx.is_construct_call() {
        return Err(NativeError::TypeError {
            name: "Promise",
            reason: "Promise constructor requires 'new'".to_string(),
        });
    }
    let executor = args.first().cloned().unwrap_or(Value::Undefined);
    if !ctx.interp_mut().is_callable_runtime(&executor) {
        return Err(NativeError::TypeError {
            name: "Promise",
            reason: "Promise executor is not callable".to_string(),
        });
    }
    let context = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "Promise",
            reason: "no active execution context".to_string(),
        })?;
    let (handle, resolve, reject) = PromiseBuilder::with_context(context.clone())
        .construct_native_rooted(ctx, &[&executor], &[args])
        .map_err(|_| oom("Promise"))?;
    if let Some(proto) = crate::bootstrap::native_new_target_prototype(ctx, "Promise")? {
        handle.set_prototype_override(ctx.heap_mut(), Some(proto));
    }
    let promise_value = Value::Promise(handle);
    let invoke_args: SmallVec<[Value; 8]> = smallvec::smallvec![resolve, reject.clone()];
    let invoke_result =
        ctx.interp_mut()
            .run_callable_sync(&context, &executor, Value::Undefined, invoke_args);
    if let Err(err) = invoke_result {
        // §27.2.1.4 step 3 — wrap the abrupt completion's value,
        // route it through the captured native `reject`. The
        // resolve / reject natives are idempotent once the
        // promise is settled.
        let reason = vm_err_to_value(&err);
        let _ = ctx.interp_mut().run_callable_sync(
            &context,
            &reject,
            Value::Undefined,
            smallvec::smallvec![reason],
        );
    }
    Ok(promise_value)
}

// ---------------------------------------------------------------
// Static method bodies
// ---------------------------------------------------------------

fn promise_static_resolve(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke_static(ctx, PromiseMethod::Resolve, args)
}

fn promise_static_reject(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke_static(ctx, PromiseMethod::Reject, args)
}

fn promise_static_all(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke_static(ctx, PromiseMethod::All, args)
}

fn promise_static_race(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke_static(ctx, PromiseMethod::Race, args)
}

fn promise_static_all_settled(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    invoke_static(ctx, PromiseMethod::AllSettled, args)
}

fn promise_static_any(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke_static(ctx, PromiseMethod::Any, args)
}

fn promise_static_with_resolvers(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    invoke_static(ctx, PromiseMethod::WithResolvers, args)
}

fn invoke_static(
    ctx: &mut NativeCtx<'_>,
    method: PromiseMethod,
    args: &[Value],
) -> Result<Value, NativeError> {
    let context = ctx.execution_context().cloned();
    let constructor = Some(ctx.this_value().clone());
    let (interp, _ignored_ctx) = ctx.interp_mut_and_context();
    promise_dispatch::statics_call(interp, context, constructor, method, args)
}

// ---------------------------------------------------------------
// Prototype method bodies
// ---------------------------------------------------------------

fn promise_proto_then(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke_prototype(ctx, "then", args)
}

fn promise_proto_catch(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke_prototype(ctx, "catch", args)
}

fn promise_proto_finally(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke_prototype(ctx, "finally", args)
}

fn invoke_prototype(
    ctx: &mut NativeCtx<'_>,
    name: &str,
    args: &[Value],
) -> Result<Value, NativeError> {
    let promise = match ctx.this_value() {
        Value::Promise(p) => *p,
        _ => {
            return Err(NativeError::TypeError {
                name: "Promise.prototype",
                reason: format!("`this` is not a Promise (in {name})"),
            });
        }
    };
    let context = ctx.execution_context().cloned();
    let (interp, _ignored) = ctx.interp_mut_and_context();
    promise_dispatch::prototype_call(interp, context, &promise, name, args)
}

// ---------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------

fn define_ctor_method(
    heap: &mut otter_gc::GcHeap,
    ctor: NativeFunction,
    name: &'static str,
    length: u8,
    call: crate::native_function::NativeFastFn,
    value_roots: &[Value],
) -> Result<(), JsSurfaceError> {
    let ctor_root = Value::NativeFunction(ctor);
    let mut roots = Vec::with_capacity(value_roots.len() + 1);
    roots.push(&ctor_root);
    roots.extend(value_roots.iter());
    let func = crate::bootstrap::native_static_with_value_roots(
        heap,
        name,
        length,
        call,
        roots.as_slice(),
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let string_heap = crate::string::StringHeap::default();
    let attrs = Attr::builtin_function();
    let desc = PropertyDescriptor::data(
        Value::NativeFunction(func),
        attrs.writable,
        attrs.enumerable,
        attrs.configurable,
    );
    if !ctor.define_own_property(heap, &string_heap, name, desc) {
        return Err(JsSurfaceError::DefinePropertyFailed(name));
    }
    Ok(())
}

fn oom(name: &'static str) -> NativeError {
    NativeError::TypeError {
        name,
        reason: "out of memory".to_string(),
    }
}

fn vm_err_to_value(err: &VmError) -> Value {
    Value::String(
        crate::JsString::from_str(&err.to_string(), &crate::StringHeap::default()).unwrap_or_else(
            |_| {
                crate::JsString::from_str("", &crate::StringHeap::default())
                    .expect("empty string allocates")
            },
        ),
    )
}
