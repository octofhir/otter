//! The `WebAssembly` JavaScript API, backed by the `wasmtime` engine.
//!
//! The namespace (`validate` / `compile` / `instantiate` and the two
//! streaming forms) plus the `Module`, `Instance`, `Memory`, `Table`, and
//! `Global` reference types run against wasmtime, so a module is validated,
//! compiled, instantiated (with JS function imports wired as host functions
//! that re-enter JS), and its exported functions, memories, tables, and
//! globals are callable end to end.
//!
//! Exception handling rides the same store: a `WebAssembly.Tag` names an
//! exception's payload signature, a `WebAssembly.Exception` is a thrown
//! exception object (a tag plus its payload values), and `WebAssembly.JSTag`
//! is the realm-wide well-known tag that carries a JS value across wasm
//! frames. A wasm export that `throw`s surfaces to JS as a
//! `WebAssembly.Exception` (with `is` / `getArg`), and a JS import that throws
//! crosses wasm frames as a `JSTag` exception — caught by wasm as an
//! `externref`, or surfaced back to JS as the original value if it escapes.
//!
//! # Contents
//! - [`WebAssembly`] — the namespace: `validate`, `compile`, `instantiate`,
//!   the private `__buildInstance` hook backing `new WebAssembly.Instance`,
//!   and the private `__jsTag` factory backing `WebAssembly.JSTag`.
//! - [`WasmModule`] / [`WasmMemory`] / [`WasmGlobal`] / [`WasmTable`] /
//!   [`WasmTag`] / [`WasmException`] — `#[js_class]` host classes relocated
//!   onto the namespace by `wasm.ns.js`.
//! - [`boundary`] / [`references`] — typed errors and fallible value conversion.
//! - [`realm`] — the sole first-use Engine/Store/JSTag owner.
//! - `wasm.ns.js` — the `Instance` class, `CompileError` / `LinkError` /
//!   `RuntimeError` intrinsic installation, the streaming forms, the `JSTag`
//!   install, and the relocation.
//!
//! # Model
//! One shared `Engine` + `Arc<Mutex<Store<StoreState>>>` lives per realm,
//! initialized only by a real Wasm operation and cached on a hidden global.
//! Bootstrap publishes the ordinary namespace and stable JSTag data property
//! with a symbolic Tag wrapper; reading their descriptors never starts Wasmtime. Every module,
//! instance, memory, table, and global uses that one store, so a standalone
//! `new WebAssembly.Memory/Table/Global(...)` can be linked into any
//! `instantiate(...)` — cross-store imports work. Reference values (`externref`)
//! round-trip through JS: a JS value handed to wasm is parked as a persistent
//! root indexed by an `externref` payload, and read back out by that index.
//! Exported functions are native closures that drive `Func::call`; imported JS
//! functions are wasmtime host functions that reach the active call's
//! [`NativeCtx`] through a per-call bridge and re-enter the VM.
//!
//! # Invariants
//! - JSTag keeps its originating realm owner; first use publishes Engine, Store
//!   and the backing tag atomically after successful native initialization.
//! - The bridge pointer in `StoreState` is live only for the span of a single
//!   synchronous export call or start-function instantiation on this thread;
//!   it is cleared before the driver
//!   returns and read only while set.
//! - Import conversions borrow their existing Caller; no externref conversion
//!   re-locks the Store already held by the driver.
//! - The shared store's `Mutex` is non-reentrant: an export call holds the
//!   guard, so a JS import that re-enters and calls another export cannot
//!   re-lock it — that surfaces as a `RuntimeError` rather than a deadlock.
//!   wasmtime's exclusive `&mut Store` borrow forbids two overlapping calls;
//!   V8/Deno allow nested calls, we surface a catchable error.
//! - `Memory.buffer` snapshots the linear memory into a fresh `ArrayBuffer`;
//!   the VM has no `ArrayBuffer` backed by foreign memory.
//! - `i64` values marshal to JS `BigInt` (spec-faithful), not `Number`.
//! - Wasmtime carries host failures as the existing owned `JsError` through
//!   its typed user-error/backtrace chain. Structural/control/OOM errors return
//!   to the runtime unchanged; only canonical catchable values enter JSTag.
//! - Actual exception payloads use NativeCtx::throw_value and the one pending
//!   synchronous throw root. Error classes come from the traced realm Error
//!   registry and cannot be replaced by changing namespace constructors.
//! - A `WebAssembly.Exception` holds its `ExnRef` rooted for the store's life;
//!   its `externref` payload keeps the underlying JS value alive through the
//!   store's `js_refs` persistent-root side table.
//!
//! # See also
//! - <https://webassembly.github.io/spec/js-api/>
//! - <https://webassembly.github.io/exception-handling/js-api/>
//! - `blob.rs` — the `#[js_class]` host-class exemplar this follows.

mod boundary;
mod realm;
mod references;

use boundary::{catchable_value, from_wasmtime, intrinsic, throw_value};
use realm::{WasmRealm, realm, realm_handle};
use references::{
    extern_ref_from_js, extern_ref_to_js, js_to_val, js_to_val_in, val_to_js, val_to_js_in,
};
use std::sync::{Arc, Mutex};

use otter_macros::{FromJs, HostClass, js_class, js_namespace};
use otter_runtime::marshal::{ArrayBuffer, IntoJs, JsError, JsValue, MarshalCx, ValueIdent};
use otter_runtime::{
    RuntimeErrorKind as ErrorKind, RuntimeNativeCall as NativeCall, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeNativeFn,
    RuntimePersistentRootId as PersistentRootId, RuntimeValue as Value, object,
};
use wasmtime::{
    AsContextMut, Caller, Engine, ExnRef, ExnRefPre, ExnType, Extern, ExternRef, ExternType, Func,
    FuncType, Global as WtGlobal, GlobalType, HeapType, Instance as WtInstance, Linker,
    Memory as WtMemory, MemoryType, Module as WtModule, Mutability, Ref, RefType, Rooted, Store,
    Table as WtTable, TableType, Tag as WtTag, TagType, ThrownException, Val, ValType,
};

/// Per-store host state. Holds the address of the active re-entry [`Bridge`]
/// while a `Func::call` is in flight (`0` when idle), stored as an integer so
/// the state stays `Send`. `js_refs` parks JS values handed to wasm as
/// `externref`s: the `externref` payload is an index into this table.
#[derive(Default)]
struct StoreState {
    bridge: usize,
    js_refs: Vec<PersistentRootId>,
}

/// The `externref` host payload: an index into [`StoreState::js_refs`].
struct ExternIndex(usize);

/// Live-for-one-call handoff from the export driver to imported host
/// functions: the address of the [`NativeCtx`] driving the current
/// `Func::call`, published in [`StoreState::bridge`] for the call's span.
struct Bridge {
    ctx: usize,
}

/// Shared, lockable wasmtime store used by every wasm object in one realm.
type SharedStore = Arc<Mutex<Store<StoreState>>>;

/// Build a `Tag` in the shared store from a list of parameter value types.
fn make_tag(engine: &Engine, store: &SharedStore, params: &[ValType]) -> Result<WtTag, JsError> {
    let func_ty = FuncType::new(engine, params.iter().cloned(), []);
    let tag_ty = TagType::new(func_ty);
    let mut guard = store.lock().expect("wasm store poisoned");
    WtTag::new(&mut *guard, &tag_ty).map_err(|error| from_wasmtime(error, ErrorKind::TypeError))
}

/// `ToUint32`-style reduction to a wasm `i32`.
fn to_wasm_i32(n: f64) -> i32 {
    if !n.is_finite() || n == 0.0 {
        return 0;
    }
    (n.trunc().rem_euclid(4_294_967_296.0) as u32) as i32
}

/// Parse a wasm value-type name from a `Global`/`Table` descriptor.
fn parse_val_type(name: &str) -> Result<ValType, JsError> {
    Ok(match name {
        "i32" => ValType::I32,
        "i64" => ValType::I64,
        "f32" => ValType::F32,
        "f64" => ValType::F64,
        "funcref" | "anyfunc" => ValType::FUNCREF,
        "externref" => ValType::EXTERNREF,
        other => return Err(JsError::Type(format!("unknown value type '{other}'"))),
    })
}

/// Default `Val` for a type, used to zero-fill result slots and table cells.
fn default_val(ty: &ValType) -> Val {
    match ty {
        ValType::I32 => Val::I32(0),
        ValType::I64 => Val::I64(0),
        ValType::F32 => Val::F32(0),
        ValType::F64 => Val::F64(0),
        ValType::V128 => Val::V128(0u128.into()),
        ValType::Ref(r) if r.heap_type().matches(&HeapType::Func) => Val::FuncRef(None),
        ValType::Ref(_) => Val::ExternRef(None),
    }
}

/// Outcome of a failed wasm `Func::call`: either a plain runtime failure
/// (trap, re-entry guard) rendered as a typed error, or a WebAssembly
/// exception whose rooted [`ExnRef`] the caller surfaces to JS.
enum CallFailure {
    Error(wasmtime::Error),
    Exception(Rooted<ExnRef>),
}

/// Drive a single wasm `Func::call`, publishing `ctx` on the store so imported
/// host functions can re-enter the VM for the span of the call. Uses `try_lock`
/// so a re-entrant nested call surfaces as an error, not a deadlock. A wasm
/// `throw` that escapes the call surfaces as [`CallFailure::Exception`].
fn drive_call(
    ctx: &mut NativeCtx<'_>,
    store: &SharedStore,
    func: Func,
    inputs: &[Val],
    outputs: &mut [Val],
) -> Result<(), CallFailure> {
    let ctx_ptr: *mut NativeCtx<'_> = ctx;
    let bridge = Bridge {
        ctx: ctx_ptr as usize,
    };
    let mut guard = store.try_lock().map_err(|_| {
        CallFailure::Error(wasmtime::Error::new(intrinsic(
            ErrorKind::WasmRuntimeError,
            "re-entrant WebAssembly call is not supported",
        )))
    })?;
    guard.data_mut().bridge = &bridge as *const Bridge as usize;
    let result = func.call(&mut *guard, inputs, outputs);
    guard.data_mut().bridge = 0;
    match result {
        Ok(()) => Ok(()),
        Err(err) => {
            if err.is::<ThrownException>() {
                return Err(match guard.take_pending_exception() {
                    Some(exn) => CallFailure::Exception(exn),
                    None => CallFailure::Error(wasmtime::Error::new(JsError::Native(
                        NativeError::InvalidOperand,
                    ))),
                });
            }
            Err(CallFailure::Error(err))
        }
    }
}

/// Failure of an imported host function. A `Fatal` error is a host-side
/// problem (missing bridge, unsupported shape) surfaced as a wasm trap; a
/// `JsThrow` carries the JS value the callback threw, parked as a persistent
/// root, so the caller can wrap it in a `JSTag` exception that crosses wasm
/// frames.
enum ImportFailure {
    Fatal(wasmtime::Error),
    JsThrow(PersistentRootId),
}

/// Body of an imported host function: read the active bridge, resolve the JS
/// callback from the persistent root, marshal `params` into JS, call it, and
/// marshal the result back into `outputs`. When the callback throws, the
/// thrown value is parked and returned as [`ImportFailure::JsThrow`].
fn run_import(
    caller: &mut Caller<'_, StoreState>,
    params: &[Val],
    outputs: &mut [Val],
    root: PersistentRootId,
    results: &[ValType],
) -> Result<(), ImportFailure> {
    let bridge_addr = caller.data().bridge;
    if bridge_addr == 0 {
        return Err(ImportFailure::Fatal(wasmtime::Error::new(JsError::Native(
            NativeError::InvalidOperand,
        ))));
    }
    // SAFETY: the active synchronous driver owns Bridge and NativeCtx for
    // exactly this Wasmtime call. No pointer is saved in a deferred job or
    // retained after drive_call/instantiate clears bridge before returning.
    let bridge = unsafe { &*(bridge_addr as *const Bridge) };
    let ctx: &mut NativeCtx<'_> = unsafe { &mut *(bridge.ctx as *mut NativeCtx<'_>) };
    ctx.scope(|scope| {
        let mut cx = MarshalCx::new(scope);
        let outcome: Result<Vec<Val>, JsError> = (|| {
            let callback = cx
                .ctx()
                .persistent_root_get(root)
                .ok_or(JsError::Native(NativeError::InvalidOperand))?;
            let callback = cx.park(callback);
            let this = cx.undefined();
            let mut argv = Vec::with_capacity(params.len());
            for param in params {
                argv.push(val_to_js_in(&mut cx, &mut *caller, param)?);
            }
            let returned = cx.call(callback, this, &argv)?;
            match results {
                [] => Ok(Vec::new()),
                [single] => Ok(vec![js_to_val_in(&mut cx, &mut *caller, returned, single)?]),
                _ => Err(JsError::Type(
                    "multi-value returns from JS imports are not supported".into(),
                )),
            }
        })();
        match outcome {
            Ok(values) => {
                outputs[..values.len()].clone_from_slice(&values);
                Ok(())
            }
            Err(error) => match catchable_value(&mut cx, error) {
                Ok(value) => {
                    let value = cx.escape(value);
                    let root = cx.ctx().persistent_root_insert(value);
                    Err(ImportFailure::JsThrow(root))
                }
                Err(error) => Err(ImportFailure::Fatal(wasmtime::Error::new(error))),
            },
        }
    })
}

/// Wrap a parked JS value in a fresh `JSTag` exception in the caller's store
/// and throw it so the exception crosses wasm frames: a matching wasm
/// `(catch $jsTag)` receives the value as an `externref`; an uncaught one
/// propagates back to the host as a `ThrownException`.
fn throw_js_via_jstag(
    caller: &mut Caller<'_, StoreState>,
    js_tag: WtTag,
    parked: PersistentRootId,
) -> Result<(), wasmtime::Error> {
    let index = caller.data().js_refs.len();
    caller.data_mut().js_refs.push(parked);
    let handle = ExternRef::new(&mut *caller, ExternIndex(index))?;
    let tag_ty = js_tag.ty(&*caller);
    let exn_ty = ExnType::from_tag_type(&tag_ty)?;
    let pre = ExnRefPre::new(&mut *caller, exn_ty);
    let exn = ExnRef::new(&mut *caller, &pre, &js_tag, &[Val::ExternRef(Some(handle))])?;
    caller.as_context_mut().throw(exn)
}

/// If `exn` carries the realm `JSTag`, unwrap its single `externref` field back
/// to the original JS value it parked; otherwise `None`.
fn js_tag_payload<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    store: &SharedStore,
    js_tag: WtTag,
    exn: Rooted<ExnRef>,
) -> Result<Option<JsValue<'s>>, JsError> {
    let field = {
        let mut guard = store.try_lock().map_err(|_| {
            intrinsic(
                ErrorKind::WasmRuntimeError,
                "re-entrant WebAssembly call is not supported",
            )
        })?;
        let tag = exn
            .tag(&mut *guard)
            .map_err(|error| from_wasmtime(error, ErrorKind::WasmRuntimeError))?;
        if !WtTag::eq(&tag, &js_tag, &*guard) {
            return Ok(None);
        }
        exn.field(&mut *guard, 0)
            .map_err(|error| from_wasmtime(error, ErrorKind::WasmRuntimeError))?
    };
    match field {
        Val::ExternRef(handle) => extern_ref_to_js(cx, store, handle).map(Some),
        _ => Err(JsError::Native(NativeError::InvalidOperand)),
    }
}

/// Surface actual host failures unchanged, and actual Wasm traps through the
/// pinned intrinsic class. Exception payloads use the canonical native throw.
fn surface_call_failure(
    cx: &mut MarshalCx<'_, '_, '_>,
    store: &SharedStore,
    js_tag: WtTag,
    failure: CallFailure,
) -> NativeError {
    const NAME: &str = "WebAssembly.Instance exported function";
    match failure {
        CallFailure::Error(error) => {
            from_wasmtime(error, ErrorKind::WasmRuntimeError).into_native(NAME)
        }
        CallFailure::Exception(exn) => {
            let value = match js_tag_payload(cx, store, js_tag, exn) {
                Ok(Some(value)) => value,
                Ok(None) => match (WasmException {
                    store: store.clone(),
                    exn,
                })
                .into_js(cx)
                {
                    Ok(value) => value,
                    Err(error) => return error.into_native(NAME),
                },
                Err(error) => return error.into_native(NAME),
            };
            throw_value(cx, value, NAME)
        }
    }
}

/// Build a JS callable that marshals its arguments, drives the exported wasm
/// `func`, and marshals the results back.
fn make_export_function<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    store: &SharedStore,
    js_tag: WtTag,
    func: Func,
) -> Result<JsValue<'s>, JsError> {
    let (params, results): (Vec<ValType>, Vec<ValType>) = {
        let mut guard = store.lock().expect("wasm store poisoned");
        let ty = func.ty(&mut *guard);
        (ty.params().collect(), ty.results().collect())
    };
    let arity = u8::try_from(params.len()).unwrap_or(u8::MAX);
    let store = store.clone();
    let call = move |ctx: &mut NativeCtx<'_>, args: &[Value], _captures: &[Value]| {
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let mut inputs: Vec<Val> = Vec::with_capacity(params.len());
            for (index, ty) in params.iter().enumerate() {
                let handle = cx.park(args.get(index).copied().unwrap_or_else(Value::undefined));
                inputs
                    .push(js_to_val(&mut cx, &store, handle, ty).map_err(|err| {
                        err.into_native("WebAssembly.Instance exported function")
                    })?);
            }
            let mut outputs: Vec<Val> = results.iter().map(default_val).collect();
            if let Err(failure) = drive_call(cx.ctx(), &store, func, &inputs, &mut outputs) {
                return Err(surface_call_failure(&mut cx, &store, js_tag, failure));
            }
            let out = match outputs.as_slice() {
                [] => cx.undefined(),
                [single] => val_to_js(&mut cx, &store, single)
                    .map_err(|error| error.into_native("WebAssembly.Instance exported function"))?,
                many => {
                    let array = cx
                        .array(many.len())
                        .map_err(|err| err.into_native("WebAssembly.Instance exported function"))?;
                    for (index, value) in many.iter().enumerate() {
                        let element = val_to_js(&mut cx, &store, value).map_err(|error| {
                            error.into_native("WebAssembly.Instance exported function")
                        })?;
                        cx.set_index(array, index, element).map_err(|err| {
                            err.into_native("WebAssembly.Instance exported function")
                        })?;
                    }
                    array
                }
            };
            Ok(cx.escape(out))
        })
    };
    let call: Arc<RuntimeNativeFn> = Arc::new(call);
    cx.native_call("", arity, NativeCall::Dynamic(call))
}

/// The `[[Prototype]]` a built `Instance` object must carry.
fn instance_prototype<'s>(cx: &mut MarshalCx<'_, '_, 's>) -> Result<Option<JsValue<'s>>, JsError> {
    let Some(namespace) = cx.ctx().global_value("WebAssembly") else {
        return Ok(None);
    };
    let namespace = cx.park(namespace);
    let proto = cx.get(namespace, "__instanceProto")?;
    Ok(cx.is_object(proto).then_some(proto))
}

/// Assemble the exports module-namespace object for an instantiated module.
fn build_exports<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    store: &SharedStore,
    js_tag: WtTag,
    instance: WtInstance,
) -> Result<JsValue<'s>, JsError> {
    let exports: Vec<(String, Extern)> = {
        let mut guard = store.lock().expect("wasm store poisoned");
        instance
            .exports(&mut *guard)
            .map(|export| (export.name().to_string(), export.into_extern()))
            .collect()
    };
    let object = cx.object()?;
    for (name, item) in exports {
        let value = match item {
            Extern::Func(func) => make_export_function(cx, store, js_tag, func)?,
            Extern::Memory(memory) => WasmMemory {
                store: store.clone(),
                memory,
            }
            .into_js(cx)?,
            Extern::Global(global) => {
                let content = global
                    .ty(&mut *store.lock().expect("wasm store poisoned"))
                    .content()
                    .clone();
                WasmGlobal {
                    store: store.clone(),
                    global,
                    content,
                }
                .into_js(cx)?
            }
            Extern::Table(table) => WasmTable {
                store: store.clone(),
                table,
            }
            .into_js(cx)?,
            Extern::Tag(tag) => WasmTag::ready(store.clone(), tag).into_js(cx)?,
            _ => continue,
        };
        cx.set(object, &name, value)?;
    }
    Ok(object)
}

/// Build the `Instance` object with an own `exports` data property,
/// re-parented onto `WebAssembly.Instance.prototype`.
fn make_instance_object<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    exports: JsValue<'s>,
) -> Result<JsValue<'s>, JsError> {
    let instance = cx.object()?;
    cx.set(instance, "exports", exports)?;
    if let Some(proto) = instance_prototype(cx)? {
        cx.set_prototype(instance, Some(proto))?;
    }
    Ok(instance)
}

/// Read `importObject[module][name]`, returning it if present and non-`undefined`.
fn resolve_import<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    import_object: JsValue<'s>,
    module: &str,
    name: &str,
) -> Result<Option<JsValue<'s>>, JsError> {
    if cx.is_nullish(import_object) {
        return Ok(None);
    }
    let submodule = cx.get(import_object, module)?;
    if cx.is_nullish(submodule) {
        return Ok(None);
    }
    let value = cx.get(submodule, name)?;
    Ok((!cx.is_undefined(value)).then_some(value))
}

/// Instantiate `module` with `import_object`, returning the `Instance` object.
fn instantiate_core<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    module: &WasmModule,
    import_object: JsValue<'s>,
) -> Result<JsValue<'s>, JsError> {
    let (engine, store, js_tag) = realm_handle(cx)?.resolve()?;
    let mut linker: Linker<StoreState> = Linker::new(&engine);

    let imports: Vec<(String, String, ExternType)> = module
        .module
        .imports()
        .map(|import| {
            (
                import.module().to_string(),
                import.name().to_string(),
                import.ty(),
            )
        })
        .collect();

    for (module_name, field, ty) in &imports {
        match ty {
            ExternType::Func(func_ty) => {
                let Some(callback) = resolve_import(cx, import_object, module_name, field)? else {
                    return Err(intrinsic(
                        ErrorKind::WasmLinkError,
                        format!("import '{module_name}.{field}' is not provided"),
                    ));
                };
                if !cx.is_callable(callback) {
                    return Err(intrinsic(
                        ErrorKind::WasmLinkError,
                        format!("import '{module_name}.{field}' is not a function"),
                    ));
                }
                let callback_raw = cx.escape(callback);
                let root = cx.ctx().persistent_root_insert(callback_raw);
                let results: Vec<ValType> = func_ty.results().collect();
                linker
                    .func_new(
                        module_name,
                        field,
                        func_ty.clone(),
                        move |mut caller: Caller<'_, StoreState>, params, outputs| match run_import(
                            &mut caller,
                            params,
                            outputs,
                            root,
                            &results,
                        ) {
                            Ok(()) => Ok(()),
                            Err(ImportFailure::Fatal(err)) => Err(err),
                            Err(ImportFailure::JsThrow(parked)) => {
                                throw_js_via_jstag(&mut caller, js_tag, parked)
                            }
                        },
                    )
                    .map_err(|err| from_wasmtime(err, ErrorKind::WasmLinkError))?;
            }
            ExternType::Memory(_)
            | ExternType::Global(_)
            | ExternType::Table(_)
            | ExternType::Tag(_) => {
                let Some(provided) = resolve_import(cx, import_object, module_name, field)? else {
                    return Err(intrinsic(
                        ErrorKind::WasmLinkError,
                        format!("import '{module_name}.{field}' is not provided"),
                    ));
                };
                let ext = import_extern(cx, provided)?.ok_or_else(|| {
                    intrinsic(ErrorKind::WasmLinkError, format!(
                        "import '{module_name}.{field}' is not a WebAssembly Memory/Table/Global/Tag"
                    ))
                })?;
                let mut guard = store.lock().expect("wasm store poisoned");
                linker
                    .define(&mut *guard, module_name, field, ext)
                    .map_err(|err| from_wasmtime(err, ErrorKind::WasmLinkError))?;
            }
        }
    }

    let outcome = {
        let ctx_ptr: *mut NativeCtx<'_> = cx.ctx();
        let bridge = Bridge {
            ctx: ctx_ptr as usize,
        };
        let mut guard = store.try_lock().map_err(|_| {
            intrinsic(
                ErrorKind::WasmRuntimeError,
                "re-entrant WebAssembly call is not supported",
            )
        })?;
        guard.data_mut().bridge = &bridge as *const Bridge as usize;
        let outcome = linker.instantiate(&mut *guard, &module.module);
        guard.data_mut().bridge = 0;
        match outcome {
            Ok(instance) => Ok(instance),
            Err(error) if error.is::<ThrownException>() => match guard.take_pending_exception() {
                Some(exn) => Err(CallFailure::Exception(exn)),
                None => Err(CallFailure::Error(wasmtime::Error::new(JsError::Native(
                    NativeError::InvalidOperand,
                )))),
            },
            Err(error) => Err(CallFailure::Error(error)),
        }
    };
    let instance = match outcome {
        Ok(instance) => instance,
        Err(CallFailure::Exception(exn)) => {
            return Err(JsError::Native(surface_call_failure(
                cx,
                &store,
                js_tag,
                CallFailure::Exception(exn),
            )));
        }
        Err(CallFailure::Error(error)) => {
            let kind = if error.is::<wasmtime::Trap>() {
                ErrorKind::WasmRuntimeError
            } else {
                ErrorKind::WasmLinkError
            };
            return Err(from_wasmtime(error, kind));
        }
    };

    let exports = build_exports(cx, &store, js_tag, instance)?;
    make_instance_object(cx, exports)
}

/// Extract the wasmtime [`Extern`] backing an imported Memory/Table/Global/Tag
/// JS wrapper (all share the realm store, so the handle is valid).
fn import_extern(
    cx: &mut MarshalCx<'_, '_, '_>,
    value: JsValue<'_>,
) -> Result<Option<Extern>, JsError> {
    if let Ok(memory) = cx.with_host_data::<WasmMemory, WtMemory>(value, |m| m.memory) {
        return Ok(Some(Extern::Memory(memory)));
    }
    if let Ok(global) = cx.with_host_data::<WasmGlobal, WtGlobal>(value, |g| g.global) {
        return Ok(Some(Extern::Global(global)));
    }
    if let Ok(table) = cx.with_host_data::<WasmTable, WtTable>(value, |t| t.table) {
        return Ok(Some(Extern::Table(table)));
    }
    if let Ok(tag) = cx.with_host_data::<WasmTag, WasmTag>(value, Clone::clone) {
        return Ok(Some(Extern::Tag(tag.resolve()?.1)));
    }
    Ok(None)
}

/// Compile `bytes` into a `Module` object.
fn compile_module<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    handle: JsValue<'s>,
) -> Result<JsValue<'s>, JsError> {
    let bytes = cx
        .buffer_source_bytes(handle)
        .ok_or_else(|| JsError::type_error("expected a BufferSource of wasm bytes"))?;
    let (engine, _store) = realm(cx)?;
    let module = WtModule::new(&engine, &bytes)
        .map_err(|err| from_wasmtime(err, ErrorKind::WasmCompileError))?;
    WasmModule { module }.into_js(cx)
}

/// Complete a namespace async method: fulfil with `result` or reject with the
/// typed error it describes.
fn settle_promise(
    cx: &mut MarshalCx<'_, '_, '_>,
    result: Result<JsValue<'_>, JsError>,
    operation: &'static str,
) -> Result<Value, NativeError> {
    match result {
        Ok(value) => {
            let promise = cx
                .promise_fulfilled(value)
                .map_err(|err| err.into_native(operation))?;
            Ok(cx.escape(promise))
        }
        Err(error) => {
            let reason =
                catchable_value(cx, error).map_err(|error| error.into_native(operation))?;
            let promise = cx
                .promise_rejected(reason)
                .map_err(|err| err.into_native(operation))?;
            Ok(cx.escape(promise))
        }
    }
}

/// The `WebAssembly` namespace marker.
pub struct WebAssembly;

#[js_namespace(name = "WebAssembly", feature = WEB, tag = "WebAssembly", js = "wasm.ns.js")]
impl WebAssembly {
    /// `WebAssembly.validate(bytes)` — true when `bytes` is a valid module.
    #[method(name = "validate", length = 1, raw)]
    fn validate(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let handle = cx.park(args.first().copied().unwrap_or_else(Value::undefined));
            let Some(bytes) = cx.buffer_source_bytes(handle) else {
                let v = cx.boolean(false);
                return Ok(cx.escape(v));
            };
            let (engine, _) =
                realm(&mut cx).map_err(|error| error.into_native("WebAssembly.validate"))?;
            let ok = match WtModule::validate(&engine, &bytes) {
                Ok(()) => true,
                Err(error) if error.is::<wasmtime::OutOfMemory>() => {
                    return Err(from_wasmtime(error, ErrorKind::WasmCompileError)
                        .into_native("WebAssembly.validate"));
                }
                Err(_) => false,
            };
            let v = cx.boolean(ok);
            Ok(cx.escape(v))
        })
    }

    /// `WebAssembly.compile(bytes)` — a promise of a `Module`.
    #[method(name = "compile", length = 1, raw)]
    fn compile(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let handle = cx.park(args.first().copied().unwrap_or_else(Value::undefined));
            let result = compile_module(&mut cx, handle);
            settle_promise(&mut cx, result, "WebAssembly.compile")
        })
    }

    /// `WebAssembly.instantiate(bytesOrModule, importObject?)`.
    #[method(name = "instantiate", length = 1, raw)]
    fn instantiate(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let source = cx.park(args.first().copied().unwrap_or_else(Value::undefined));
            let imports = cx.park(args.get(1).copied().unwrap_or_else(Value::undefined));
            let result = instantiate_entry(&mut cx, source, imports);
            settle_promise(&mut cx, result, "WebAssembly.instantiate")
        })
    }

    /// Synchronous instantiation backing `new WebAssembly.Instance(...)`.
    #[method(name = "__buildInstance", length = 2, raw)]
    fn build_instance(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let module_handle = cx.park(args.first().copied().unwrap_or_else(Value::undefined));
            let imports = cx.park(args.get(1).copied().unwrap_or_else(Value::undefined));
            let module = cx
                .with_host_data::<WasmModule, WasmModule>(module_handle, Clone::clone)
                .map_err(|_| {
                    JsError::Type("first argument must be a WebAssembly.Module".into())
                        .into_native("WebAssembly.Instance")
                })?;
            let instance = instantiate_core(&mut cx, &module, imports)
                .map_err(|error| error.into_native("WebAssembly.Instance"))?;
            Ok(cx.escape(instance))
        })
    }

    /// Expose original pinned constructors through the static namespace
    /// bootstrap. This hook is moved into the private factory bag by the macro.
    #[method(name = "__errorClasses", length = 0, raw)]
    fn error_classes(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
        ctx.scope(|mut scope| {
            let classes = scope.object()?;
            for kind in [
                ErrorKind::WasmCompileError,
                ErrorKind::WasmLinkError,
                ErrorKind::WasmRuntimeError,
            ] {
                let constructor = scope.error_constructor(kind);
                scope.define(
                    classes,
                    kind.class_name(),
                    constructor,
                    object::PropertyFlags::new(true, false, true),
                )?;
            }
            Ok(scope.finish(classes))
        })
    }

    /// Build the realm-wide well-known `JSTag` as a `WebAssembly.Tag` instance.
    /// The `wasm.ns.js` glue installs the result as the readonly
    /// `WebAssembly.JSTag`.
    #[method(name = "__jsTag", length = 0, raw)]
    fn js_tag(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let realm =
                realm_handle(&mut cx).map_err(|err| err.into_native("WebAssembly.JSTag"))?;
            let value = WasmTag::js_tag(realm)
                .into_js(&mut cx)
                .map_err(|err| err.into_native("WebAssembly.JSTag"))?;
            Ok(cx.escape(value))
        })
    }
}

/// Dispatch `WebAssembly.instantiate` over its two overloads.
fn instantiate_entry<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    source: JsValue<'s>,
    imports: JsValue<'s>,
) -> Result<JsValue<'s>, JsError> {
    if let Ok(module) = cx.with_host_data::<WasmModule, WasmModule>(source, Clone::clone) {
        return instantiate_core(cx, &module, imports);
    }
    let bytes = cx
        .buffer_source_bytes(source)
        .ok_or_else(|| JsError::type_error("expected a BufferSource or a WebAssembly.Module"))?;
    let (engine, _store) = realm(cx)?;
    let module = WtModule::new(&engine, &bytes)
        .map_err(|err| from_wasmtime(err, ErrorKind::WasmCompileError))?;
    let module = WasmModule { module };
    let instance = instantiate_core(cx, &module, imports)?;
    let module_js = module.into_js(cx)?;
    let result = cx.object()?;
    cx.set(result, "module", module_js)?;
    cx.set(result, "instance", instance)?;
    Ok(result)
}

/// Compiled `WebAssembly.Module` (cheap to clone: wasmtime `Module` is an Arc).
#[derive(Clone, HostClass)]
pub struct WasmModule {
    module: WtModule,
}

#[js_class(name = "WebAssembly.Module", feature = WEB)]
impl WasmModule {
    #[constructor(raw)]
    fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let handle = cx.park(args.first().copied().unwrap_or_else(Value::undefined));
            let bytes = cx.buffer_source_bytes(handle).ok_or_else(|| {
                JsError::Type("expected a BufferSource of wasm bytes".into())
                    .into_native("WebAssembly.Module")
            })?;
            let (engine, _store) =
                realm(&mut cx).map_err(|error| error.into_native("WebAssembly.Module"))?;
            let module = WtModule::new(&engine, &bytes).map_err(|error| {
                from_wasmtime(error, ErrorKind::WasmCompileError).into_native("WebAssembly.Module")
            })?;
            let value = WasmModule { module }
                .into_js(&mut cx)
                .map_err(|err| err.into_native("WebAssembly.Module"))?;
            Ok(cx.escape(value))
        })
    }
}

/// A `Memory` constructor descriptor (`{ initial, maximum? }`).
#[derive(FromJs)]
struct MemoryDescriptor {
    initial: f64,
    maximum: Option<f64>,
}

/// `WebAssembly.Memory`: a linear memory in the shared realm store.
#[derive(Clone, HostClass)]
pub struct WasmMemory {
    store: SharedStore,
    memory: WtMemory,
}

#[js_class(name = "WebAssembly.Memory", feature = WEB)]
impl WasmMemory {
    #[constructor(raw)]
    fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let handle = cx.park(args.first().copied().unwrap_or_else(Value::undefined));
            let descriptor: MemoryDescriptor =
                <MemoryDescriptor as otter_runtime::marshal::FromJs>::from_js(
                    &mut cx,
                    handle,
                    ValueIdent::Argument(0),
                )
                .map_err(|err| err.into_native("WebAssembly.Memory"))?;
            let (_engine, store) =
                realm(&mut cx).map_err(|err| err.into_native("WebAssembly.Memory"))?;
            let ty = MemoryType::new(
                descriptor.initial as u32,
                descriptor.maximum.map(|v| v as u32),
            );
            let memory = {
                let mut guard = store.lock().expect("wasm store poisoned");
                WtMemory::new(&mut *guard, ty).map_err(|err| {
                    from_wasmtime(err, ErrorKind::RangeError).into_native("WebAssembly.Memory")
                })?
            };
            let value = WasmMemory {
                store: store.clone(),
                memory,
            }
            .into_js(&mut cx)
            .map_err(|err| err.into_native("WebAssembly.Memory"))?;
            Ok(cx.escape(value))
        })
    }

    /// A fresh `ArrayBuffer` snapshot of the current linear memory.
    #[getter(name = "buffer")]
    fn buffer(&self) -> ArrayBuffer {
        let guard = self.store.lock().expect("wasm store poisoned");
        ArrayBuffer(self.memory.data(&*guard).to_vec())
    }

    #[method(name = "grow", length = 1)]
    fn grow(&self, delta: f64) -> Result<f64, JsError> {
        let mut guard = self.store.lock().expect("wasm store poisoned");
        let previous = self
            .memory
            .grow(&mut *guard, delta as u64)
            .map_err(|err| from_wasmtime(err, ErrorKind::RangeError))?;
        Ok(previous as f64)
    }
}

/// A `Global` constructor descriptor (`{ value, mutable? }`).
#[derive(FromJs)]
struct GlobalDescriptor {
    value: String,
    mutable: Option<bool>,
}

/// `WebAssembly.Global`: a single global cell in the shared realm store.
#[derive(Clone, HostClass)]
pub struct WasmGlobal {
    store: SharedStore,
    global: WtGlobal,
    content: ValType,
}

#[js_class(name = "WebAssembly.Global", feature = WEB)]
impl WasmGlobal {
    #[constructor(raw)]
    fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let desc_handle = cx.park(args.first().copied().unwrap_or_else(Value::undefined));
            let descriptor: GlobalDescriptor =
                <GlobalDescriptor as otter_runtime::marshal::FromJs>::from_js(
                    &mut cx,
                    desc_handle,
                    ValueIdent::Argument(0),
                )
                .map_err(|err| err.into_native("WebAssembly.Global"))?;
            let content = parse_val_type(&descriptor.value)
                .map_err(|err| err.into_native("WebAssembly.Global"))?;
            let (_engine, store) =
                realm(&mut cx).map_err(|err| err.into_native("WebAssembly.Global"))?;
            let initial = cx.park(args.get(1).copied().unwrap_or_else(Value::undefined));
            let value = if cx.is_undefined(initial) {
                default_val(&content)
            } else {
                js_to_val(&mut cx, &store, initial, &content)
                    .map_err(|err| err.into_native("WebAssembly.Global"))?
            };
            let mutability = if descriptor.mutable.unwrap_or(false) {
                Mutability::Var
            } else {
                Mutability::Const
            };
            let global = {
                let mut guard = store.lock().expect("wasm store poisoned");
                WtGlobal::new(
                    &mut *guard,
                    GlobalType::new(content.clone(), mutability),
                    value,
                )
                .map_err(|err| {
                    from_wasmtime(err, ErrorKind::TypeError).into_native("WebAssembly.Global")
                })?
            };
            let out = WasmGlobal {
                store: store.clone(),
                global,
                content,
            }
            .into_js(&mut cx)
            .map_err(|err| err.into_native("WebAssembly.Global"))?;
            Ok(cx.escape(out))
        })
    }

    #[getter(name = "value", raw)]
    fn get_value(&self, ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
        let store = self.store.clone();
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let val = {
                let mut guard = store.lock().expect("wasm store poisoned");
                self.global.get(&mut *guard)
            };
            let out = val_to_js(&mut cx, &store, &val)
                .map_err(|error| error.into_native("WebAssembly.Global"))?;
            Ok(cx.escape(out))
        })
    }

    #[setter(name = "value", raw)]
    fn set_value(&self, ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        let store = self.store.clone();
        let content = self.content.clone();
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let handle = cx.park(args.first().copied().unwrap_or_else(Value::undefined));
            let new_value = js_to_val(&mut cx, &store, handle, &content)
                .map_err(|err| err.into_native("WebAssembly.Global"))?;
            let mut guard = store.lock().expect("wasm store poisoned");
            self.global.set(&mut *guard, new_value).map_err(|err| {
                from_wasmtime(err, ErrorKind::TypeError).into_native("WebAssembly.Global")
            })?;
            Ok(Value::undefined())
        })
    }
}

/// A `Table` constructor descriptor (`{ element, initial, maximum? }`).
#[derive(FromJs)]
struct TableDescriptor {
    element: String,
    initial: f64,
    maximum: Option<f64>,
}

/// `WebAssembly.Table`: a reference-typed table in the shared realm store.
#[derive(Clone, HostClass)]
pub struct WasmTable {
    store: SharedStore,
    table: WtTable,
}

#[js_class(name = "WebAssembly.Table", feature = WEB)]
impl WasmTable {
    #[constructor(raw)]
    fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let handle = cx.park(args.first().copied().unwrap_or_else(Value::undefined));
            let descriptor: TableDescriptor =
                <TableDescriptor as otter_runtime::marshal::FromJs>::from_js(
                    &mut cx,
                    handle,
                    ValueIdent::Argument(0),
                )
                .map_err(|err| err.into_native("WebAssembly.Table"))?;
            let element = parse_val_type(&descriptor.element)
                .map_err(|err| err.into_native("WebAssembly.Table"))?;
            let ValType::Ref(ref_ty) = &element else {
                return Err(
                    JsError::Type("Table element must be 'funcref' or 'externref'".into())
                        .into_native("WebAssembly.Table"),
                );
            };
            let (_engine, store) =
                realm(&mut cx).map_err(|err| err.into_native("WebAssembly.Table"))?;
            let init = cx.park(args.get(1).copied().unwrap_or_else(Value::undefined));
            let init_ref = table_init_ref(&mut cx, &store, ref_ty.clone(), init)
                .map_err(|err| err.into_native("WebAssembly.Table"))?;
            let ty = TableType::new(
                ref_ty.clone(),
                descriptor.initial as u32,
                descriptor.maximum.map(|v| v as u32),
            );
            let table = {
                let mut guard = store.lock().expect("wasm store poisoned");
                WtTable::new(&mut *guard, ty, init_ref).map_err(|err| {
                    from_wasmtime(err, ErrorKind::RangeError).into_native("WebAssembly.Table")
                })?
            };
            let out = WasmTable {
                store: store.clone(),
                table,
            }
            .into_js(&mut cx)
            .map_err(|err| err.into_native("WebAssembly.Table"))?;
            Ok(cx.escape(out))
        })
    }

    #[getter(name = "length")]
    fn length(&self) -> f64 {
        let guard = self.store.lock().expect("wasm store poisoned");
        self.table.size(&*guard) as f64
    }

    #[method(name = "get", length = 1, raw)]
    fn get(&self, ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        let store = self.store.clone();
        let table = self.table;
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let index = cx.park(args.first().copied().unwrap_or_else(Value::undefined));
            let index =
                cx.to_number_spec(index)
                    .map_err(|err| err.into_native("WebAssembly.Table"))? as u64;
            let cell = {
                let mut guard = store.lock().expect("wasm store poisoned");
                table.get(&mut *guard, index)
            };
            let out = match cell {
                Some(Ref::Extern(handle)) => extern_ref_to_js(&mut cx, &store, handle)
                    .map_err(|error| error.into_native("WebAssembly.Table"))?,
                Some(Ref::Func(_)) | Some(_) => cx.null(),
                None => {
                    return Err(JsError::Range("Table.get index out of bounds".into())
                        .into_native("WebAssembly.Table"));
                }
            };
            Ok(cx.escape(out))
        })
    }

    #[method(name = "set", length = 2, raw)]
    fn set(&self, ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        let store = self.store.clone();
        let table = self.table;
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let index_handle = cx.park(args.first().copied().unwrap_or_else(Value::undefined));
            let index =
                cx.to_number_spec(index_handle)
                    .map_err(|err| err.into_native("WebAssembly.Table"))? as u64;
            let value_handle = cx.park(args.get(1).copied().unwrap_or_else(Value::undefined));
            let ref_ty = {
                let mut guard = store.lock().expect("wasm store poisoned");
                table.ty(&mut *guard).element().clone()
            };
            let new_ref = table_init_ref(&mut cx, &store, ref_ty, value_handle)
                .map_err(|err| err.into_native("WebAssembly.Table"))?;
            let mut guard = store.lock().expect("wasm store poisoned");
            table.set(&mut *guard, index, new_ref).map_err(|err| {
                from_wasmtime(err, ErrorKind::RangeError).into_native("WebAssembly.Table")
            })?;
            Ok(Value::undefined())
        })
    }
}

/// Coerce a JS value into a table's element [`Ref`].
fn table_init_ref(
    cx: &mut MarshalCx<'_, '_, '_>,
    store: &SharedStore,
    ref_ty: RefType,
    value: JsValue<'_>,
) -> Result<Ref, JsError> {
    if ref_ty.heap_type().matches(&HeapType::Extern) {
        Ok(Ref::Extern(extern_ref_from_js(cx, store, value)?))
    } else if cx.is_nullish(value) {
        Ok(Ref::Func(None))
    } else {
        Err(JsError::Type(
            "setting a funcref table element from JS is not supported".to_string(),
        ))
    }
}

/// Read the `parameters` sequence of a `Tag`/`Exception` descriptor into wasm
/// value types.
fn read_tag_parameters(
    cx: &mut MarshalCx<'_, '_, '_>,
    descriptor: JsValue<'_>,
) -> Result<Vec<ValType>, JsError> {
    if !cx.is_object(descriptor) {
        return Err(JsError::Type(
            "Tag descriptor must be an object".to_string(),
        ));
    }
    let parameters = cx.get(descriptor, "parameters")?;
    let handles = cx.iterate_to_handles(parameters)?;
    let mut types = Vec::with_capacity(handles.len());
    for handle in handles {
        let name = cx.to_string_spec(handle)?;
        types.push(parse_val_type(&name)?);
    }
    Ok(types)
}

/// Resolve the wasmtime [`Tag`] backing a JS `WebAssembly.Tag` argument.
fn tag_argument(cx: &mut MarshalCx<'_, '_, '_>, value: JsValue<'_>) -> Result<WtTag, JsError> {
    let tag = cx
        .with_host_data::<WasmTag, WasmTag>(value, Clone::clone)
        .map_err(|_| JsError::Type("expected a WebAssembly.Tag".to_string()))?;
    Ok(tag.resolve()?.1)
}

/// `WebAssembly.Tag`: a wasm exception tag in the shared realm store. A `Tag`
/// names an exception's payload signature and gives thrown exceptions their
/// identity (`WebAssembly.Exception.prototype.is`).
#[derive(Clone, HostClass)]
pub struct WasmTag {
    backing: TagBacking,
}

/// The canonical JSTag shell holds the same first-use realm owner. User and
/// exported tags already have an actual Store handle; neither path adds a
/// second tag registry or changes the wrapper's JavaScript identity.
#[derive(Clone)]
enum TagBacking {
    JsTag(WasmRealm),
    Ready { store: SharedStore, tag: WtTag },
}

impl WasmTag {
    fn js_tag(realm: WasmRealm) -> Self {
        Self {
            backing: TagBacking::JsTag(realm),
        }
    }

    fn ready(store: SharedStore, tag: WtTag) -> Self {
        Self {
            backing: TagBacking::Ready { store, tag },
        }
    }

    fn resolve(&self) -> Result<(SharedStore, WtTag), JsError> {
        match &self.backing {
            TagBacking::JsTag(realm) => {
                let (_, store, tag) = realm.resolve()?;
                Ok((store, tag))
            }
            TagBacking::Ready { store, tag } => Ok((store.clone(), *tag)),
        }
    }
}

#[js_class(name = "WebAssembly.Tag", feature = WEB)]
impl WasmTag {
    #[constructor(raw)]
    fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let descriptor = cx.park(args.first().copied().unwrap_or_else(Value::undefined));
            let params = read_tag_parameters(&mut cx, descriptor)
                .map_err(|err| err.into_native("WebAssembly.Tag"))?;
            let (engine, store) =
                realm(&mut cx).map_err(|err| err.into_native("WebAssembly.Tag"))?;
            let tag = make_tag(&engine, &store, &params)
                .map_err(|err| err.into_native("WebAssembly.Tag"))?;
            let value = WasmTag::ready(store, tag)
                .into_js(&mut cx)
                .map_err(|err| err.into_native("WebAssembly.Tag"))?;
            Ok(cx.escape(value))
        })
    }

    /// `tag.type()` — the descriptor `{ parameters: [...] }` this tag was built
    /// from.
    #[method(name = "type", length = 0, raw)]
    fn type_of(&self, ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
        let (store, tag) = self
            .resolve()
            .map_err(|error| error.into_native("WebAssembly.Tag"))?;
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let params: Vec<ValType> = {
                let guard = store.lock().expect("wasm store poisoned");
                tag.ty(&*guard).ty().params().collect()
            };
            let array = cx
                .array(params.len())
                .map_err(|err| err.into_native("WebAssembly.Tag"))?;
            for (index, ty) in params.iter().enumerate() {
                let name = cx
                    .string(val_type_name(ty))
                    .map_err(|err| err.into_native("WebAssembly.Tag"))?;
                cx.set_index(array, index, name)
                    .map_err(|err| err.into_native("WebAssembly.Tag"))?;
            }
            let object = cx
                .object()
                .map_err(|err| err.into_native("WebAssembly.Tag"))?;
            cx.set(object, "parameters", array)
                .map_err(|err| err.into_native("WebAssembly.Tag"))?;
            Ok(cx.escape(object))
        })
    }
}

/// The WebIDL value-type name for a wasm [`ValType`], for `Tag.prototype.type`.
fn val_type_name(ty: &ValType) -> &'static str {
    match ty {
        ValType::I32 => "i32",
        ValType::I64 => "i64",
        ValType::F32 => "f32",
        ValType::F64 => "f64",
        ValType::V128 => "v128",
        ValType::Ref(r) if r.heap_type().matches(&HeapType::Func) => "funcref",
        ValType::Ref(_) => "externref",
    }
}

/// `WebAssembly.Exception`: a thrown wasm exception object — a tag plus the
/// payload values it carries — held as a rooted [`ExnRef`] in the shared store.
#[derive(Clone, HostClass)]
pub struct WasmException {
    store: SharedStore,
    exn: Rooted<ExnRef>,
}

#[js_class(name = "WebAssembly.Exception", feature = WEB)]
impl WasmException {
    #[constructor(raw)]
    fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let tag_handle = cx.park(args.first().copied().unwrap_or_else(Value::undefined));
            let tag = tag_argument(&mut cx, tag_handle)
                .map_err(|err| err.into_native("WebAssembly.Exception"))?;
            let (_engine, store) =
                realm(&mut cx).map_err(|err| err.into_native("WebAssembly.Exception"))?;
            let param_types: Vec<ValType> = {
                let guard = store.lock().expect("wasm store poisoned");
                tag.ty(&*guard).ty().params().collect()
            };
            let payload_handle = cx.park(args.get(1).copied().unwrap_or_else(Value::undefined));
            let payload = cx
                .iterate_to_handles(payload_handle)
                .map_err(|err| err.into_native("WebAssembly.Exception"))?;
            if payload.len() != param_types.len() {
                return Err(JsError::Type(format!(
                    "expected {} payload values, got {}",
                    param_types.len(),
                    payload.len()
                ))
                .into_native("WebAssembly.Exception"));
            }
            let mut fields: Vec<Val> = Vec::with_capacity(param_types.len());
            for (value, ty) in payload.into_iter().zip(&param_types) {
                fields.push(
                    js_to_val(&mut cx, &store, value, ty)
                        .map_err(|err| err.into_native("WebAssembly.Exception"))?,
                );
            }
            let exn = {
                let mut guard = store.lock().expect("wasm store poisoned");
                let exn_ty = ExnType::from_tag_type(&tag.ty(&*guard)).map_err(|err| {
                    from_wasmtime(err, ErrorKind::TypeError).into_native("WebAssembly.Exception")
                })?;
                let pre = ExnRefPre::new(&mut *guard, exn_ty);
                ExnRef::new(&mut *guard, &pre, &tag, &fields).map_err(|err| {
                    from_wasmtime(err, ErrorKind::TypeError).into_native("WebAssembly.Exception")
                })?
            };
            let value = WasmException {
                store: store.clone(),
                exn,
            }
            .into_js(&mut cx)
            .map_err(|err| err.into_native("WebAssembly.Exception"))?;
            Ok(cx.escape(value))
        })
    }

    /// `exception.is(tag)` — whether this exception carries `tag` (identity).
    #[method(name = "is", length = 1, raw)]
    fn is(&self, ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        let store = self.store.clone();
        let exn = self.exn;
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let tag_handle = cx.park(args.first().copied().unwrap_or_else(Value::undefined));
            let tag = tag_argument(&mut cx, tag_handle)
                .map_err(|err| err.into_native("WebAssembly.Exception"))?;
            let matches = {
                let mut guard = store.lock().expect("wasm store poisoned");
                let own = exn.tag(&mut *guard).map_err(|error| {
                    from_wasmtime(error, ErrorKind::TypeError).into_native("WebAssembly.Exception")
                })?;
                WtTag::eq(&own, &tag, &*guard)
            };
            let out = cx.boolean(matches);
            Ok(cx.escape(out))
        })
    }

    /// `exception.getArg(tag, index)` — the `index`th payload value, marshalled
    /// back to JS. Throws `TypeError` on tag mismatch and `RangeError` when
    /// `index` is out of range.
    #[method(name = "getArg", length = 2, raw)]
    fn get_arg(&self, ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        let store = self.store.clone();
        let exn = self.exn;
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let tag_handle = cx.park(args.first().copied().unwrap_or_else(Value::undefined));
            let tag = tag_argument(&mut cx, tag_handle)
                .map_err(|err| err.into_native("WebAssembly.Exception"))?;
            let index_handle = cx.park(args.get(1).copied().unwrap_or_else(Value::undefined));
            let index = cx
                .to_number_spec(index_handle)
                .map_err(|err| err.into_native("WebAssembly.Exception"))?
                as usize;
            let field = {
                let mut guard = store.lock().expect("wasm store poisoned");
                let own = exn.tag(&mut *guard).map_err(|err| {
                    from_wasmtime(err, ErrorKind::TypeError).into_native("WebAssembly.Exception")
                })?;
                if !WtTag::eq(&own, &tag, &*guard) {
                    return Err(JsError::Type("getArg called with the wrong tag".into())
                        .into_native("WebAssembly.Exception"));
                }
                let arity = own.ty(&*guard).ty().params().len();
                if index >= arity {
                    return Err(JsError::Range("getArg index out of range".into())
                        .into_native("WebAssembly.Exception"));
                }
                exn.field(&mut *guard, index).map_err(|err| {
                    from_wasmtime(err, ErrorKind::RangeError).into_native("WebAssembly.Exception")
                })?
            };
            let out = val_to_js(&mut cx, &store, &field)
                .map_err(|error| error.into_native("WebAssembly.Exception"))?;
            Ok(cx.escape(out))
        })
    }
}
