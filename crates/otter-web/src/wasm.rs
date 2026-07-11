//! The `WebAssembly` JavaScript API, backed by the `wasmi` interpreter.
//!
//! The namespace (`validate` / `compile` / `instantiate` and the two
//! streaming forms) plus the `Module`, `Instance`, `Memory`, `Table`,
//! and `Global` reference types are implemented against a real wasm
//! engine, so a module is validated, compiled, instantiated (with JS
//! function imports wired as host functions that re-enter JS), and its
//! exported functions and memory are callable end to end.
//!
//! # Contents
//! - [`WebAssembly`] — the namespace: `validate`, `compile`,
//!   `instantiate`, and the private `__buildInstance` hook that backs
//!   the synchronous `new WebAssembly.Instance(...)` glue.
//! - [`WasmModule`] / [`WasmMemory`] / [`WasmGlobal`] / [`WasmTable`] —
//!   `#[js_class]` host classes relocated onto the `WebAssembly`
//!   namespace by `wasm.ns.js`.
//! - `wasm.ns.js` — the `Instance` class, the `CompileError` /
//!   `LinkError` / `RuntimeError` subclasses, the streaming forms, and
//!   the class relocation.
//!
//! # Model
//! Each wasmi handle (`Instance`, `Memory`, `Table`, `Global`, `Func`)
//! indexes into an owning `wasmi::Store`, which is not clonable, so a
//! host-class instance holds a shared `Arc<Mutex<Store<StoreState>>>`
//! plus the cheap `Copy` handle. A `Module` holds the `Engine` and the
//! clonable `wasmi::Module`. Exported functions are native closures that
//! own the shared store and drive `Func::call`; imported JS functions
//! are wasmi host functions that reach the active call's [`NativeCtx`]
//! through a per-call bridge and re-enter the VM to invoke the callback.
//!
//! # Invariants
//! - The bridge pointer stored in `StoreState` is live only for the
//!   duration of a single synchronous `Func::call` nested inside a
//!   native method on this thread; it is cleared before the driver
//!   returns and read only while set. The VM is single-threaded, so no
//!   two `Func::call`s on one store overlap.
//! - Value marshalling copies bytes: reading an exported `Memory.buffer`
//!   snapshots the linear memory into a fresh `ArrayBuffer`, because the
//!   engine has no shared-backing `ArrayBuffer` over foreign memory.
//! - Imported/exported `i64` uses JS `Number` rather than `BigInt`, and
//!   an import that reaches back into the same instance's exports would
//!   re-lock its store; both are documented boundaries of this backend.
//!
//! # See also
//! - <https://webassembly.github.io/spec/js-api/>
//! - `blob.rs` — the `#[js_class]` host-class exemplar this follows.

use std::sync::{Arc, Mutex};

use otter_macros::{FromJs, HostClass, js_class, js_namespace};
use otter_runtime::marshal::{ArrayBuffer, BufferSource, IntoJs, JsError, JsValue, MarshalCx};
use otter_runtime::{
    NativeCall, RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError, RuntimeNativeFn,
    RuntimePersistentRootId as PersistentRootId, RuntimeValue as Value, object,
};
use wasmi::{
    Engine, Extern, ExternType, F32, F64, Func, Global as WasmiGlobal, Instance as WasmiInstance,
    Linker, Memory as WasmiMemory, MemoryType, Module as WasmiModule, Mutability, Store,
    Table as WasmiTable, TableType, Val, ValType,
};

/// Per-store host state. Holds the address of the active re-entry
/// [`Bridge`] while a `Func::call` is in flight, or `0` when no call is
/// running. Stored as an integer so the state stays `Send`, which the
/// export-function closures require of the shared store.
#[derive(Default)]
struct StoreState {
    bridge: usize,
}

/// Live-for-one-call handoff from the export driver to the imported host
/// functions: the address of the [`NativeCtx`] driving the current
/// `Func::call`. Lives on the driver's stack; its address is published
/// in [`StoreState::bridge`] only for the span of that call.
struct Bridge {
    ctx: usize,
}

/// Shared, lockable wasmi store used by every handle of one instance (or
/// one standalone `Memory` / `Global` / `Table`).
type SharedStore = Arc<Mutex<Store<StoreState>>>;

/// A thrown/rejected `WebAssembly` error described independently of the
/// VM: `kind` is the JS constructor name, `message` the text.
struct WasmThrow {
    kind: &'static str,
    message: String,
}

impl WasmThrow {
    fn compile(message: impl Into<String>) -> Self {
        Self {
            kind: "CompileError",
            message: message.into(),
        }
    }

    fn link(message: impl Into<String>) -> Self {
        Self {
            kind: "LinkError",
            message: message.into(),
        }
    }

    fn runtime(message: impl Into<String>) -> Self {
        Self {
            kind: "RuntimeError",
            message: message.into(),
        }
    }

    fn type_error(message: impl Into<String>) -> Self {
        Self {
            kind: "TypeError",
            message: message.into(),
        }
    }

    fn from_js(err: JsError) -> Self {
        Self::runtime(err.to_string())
    }

    /// Build the JS error object this describes, parked in the ambient
    /// scope. Falls back to the message string if the constructor cannot
    /// be resolved or invoked.
    fn to_value<'s>(&self, cx: &mut MarshalCx<'_, '_, 's>) -> JsValue<'s> {
        let message = cx.string(&self.message).unwrap_or_else(|_| cx.undefined());
        if let Some(ctor) = error_ctor(cx, self.kind)
            && cx.is_callable(ctor)
        {
            let ctor_raw = cx.escape(ctor);
            let message_raw = cx.escape(message);
            if let Ok(err) = cx.ctx().construct(ctor_raw, &[message_raw]) {
                return cx.park(err);
            }
        }
        message
    }
}

/// Resolve the constructor for a JS error class: the `WebAssembly.*`
/// error subclasses come off the namespace, everything else off the
/// global (`TypeError` / `RangeError`).
fn error_ctor<'s>(cx: &mut MarshalCx<'_, '_, 's>, kind: &str) -> Option<JsValue<'s>> {
    if matches!(kind, "CompileError" | "LinkError" | "RuntimeError") {
        let namespace = cx.ctx().global_value("WebAssembly")?;
        let handle = cx.park(namespace);
        cx.get(handle, kind).ok()
    } else {
        let ctor = cx.ctx().global_value(kind)?;
        Some(cx.park(ctor))
    }
}

/// §7.1.7 `ToUint32`-style reduction to a wasm `i32`.
fn to_wasm_i32(n: f64) -> i32 {
    if !n.is_finite() || n == 0.0 {
        return 0;
    }
    (n.trunc().rem_euclid(4_294_967_296.0) as u32) as i32
}

/// Truncating reduction to a wasm `i64`. Marshalling `i64` through a JS
/// `Number` loses precision beyond 2^53 — a documented boundary of this
/// backend (the spec uses `BigInt`, which the marshal layer does not expose).
fn to_wasm_i64(n: f64) -> i64 {
    if !n.is_finite() {
        return 0;
    }
    n.trunc() as i64
}

/// Convert a wasm value into a JS value parked in the ambient scope.
/// Reference-type values surface as `null`.
fn val_to_js<'s>(cx: &mut MarshalCx<'_, '_, 's>, value: &Val) -> JsValue<'s> {
    match value {
        Val::I32(x) => cx.number(f64::from(*x)),
        Val::I64(x) => cx.number(*x as f64),
        Val::F32(x) => cx.number(f64::from(x.to_float())),
        Val::F64(x) => cx.number(x.to_float()),
        _ => cx.null(),
    }
}

/// Coerce a JS value into a wasm value of the given type.
fn js_to_val<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    handle: JsValue<'s>,
    ty: ValType,
) -> Result<Val, JsError> {
    Ok(match ty {
        ValType::I32 => Val::I32(to_wasm_i32(cx.to_number_spec(handle)?)),
        ValType::I64 => Val::I64(to_wasm_i64(cx.to_number_spec(handle)?)),
        ValType::F32 => Val::F32(F32::from_float(cx.to_number_spec(handle)? as f32)),
        ValType::F64 => Val::F64(F64::from_float(cx.to_number_spec(handle)?)),
        ValType::FuncRef | ValType::ExternRef | ValType::V128 => {
            return Err(JsError::Type(
                "reference-type wasm values are not supported".to_string(),
            ));
        }
    })
}

/// Parse a wasm value-type name from a `Global`/`Table` descriptor.
fn parse_val_type(name: &str) -> Result<ValType, JsError> {
    Ok(match name {
        "i32" => ValType::I32,
        "i64" => ValType::I64,
        "f32" => ValType::F32,
        "f64" => ValType::F64,
        "funcref" | "anyfunc" => ValType::FuncRef,
        "externref" => ValType::ExternRef,
        other => {
            return Err(JsError::Type(format!("unknown value type '{other}'")));
        }
    })
}

/// Drive a single wasm `Func::call`, publishing `ctx` on the store so
/// imported host functions can re-enter the VM for the span of the call.
fn drive_call(
    ctx: &mut NativeCtx<'_>,
    store: &SharedStore,
    func: Func,
    inputs: &[Val],
    outputs: &mut [Val],
) -> Result<(), wasmi::Error> {
    let ctx_ptr: *mut NativeCtx<'_> = ctx;
    let bridge = Bridge {
        ctx: ctx_ptr as usize,
    };
    let mut guard = store.lock().expect("wasm store poisoned");
    guard.data_mut().bridge = &bridge as *const Bridge as usize;
    let result = func.call(&mut *guard, inputs, outputs);
    guard.data_mut().bridge = 0;
    result
}

/// Body of an imported host function: read the active bridge, resolve
/// the JS callback from the persistent root, marshal `params` into JS,
/// call it, and marshal the result back into `outputs`.
fn run_import(
    bridge_addr: usize,
    params: &[Val],
    outputs: &mut [Val],
    root: PersistentRootId,
    results: &[ValType],
) -> Result<(), wasmi::Error> {
    if bridge_addr == 0 {
        return Err(wasmi::Error::new(
            "wasm import invoked without an active JS bridge",
        ));
    }
    // SAFETY: `bridge_addr` is the address of a `Bridge` on the stack of
    // the `drive_call` frame that is currently executing this wasm call
    // on this thread; it stays valid until that call returns, and the VM
    // never runs two `Func::call`s on one store at once.
    let bridge = unsafe { &*(bridge_addr as *const Bridge) };
    let ctx: &mut NativeCtx<'_> = unsafe { &mut *(bridge.ctx as *mut NativeCtx<'_>) };
    let outcome: Result<(), JsError> = ctx.scope(|ctx, scope| {
        let mut cx = MarshalCx::new(ctx, scope);
        let callback = cx
            .ctx()
            .persistent_root_get(root)
            .unwrap_or_else(Value::undefined);
        let callback = cx.park(callback);
        let this = cx.undefined();
        let mut argv: Vec<JsValue<'_>> = Vec::with_capacity(params.len());
        for param in params {
            argv.push(val_to_js(&mut cx, param));
        }
        let returned = cx.call(callback, this, &argv)?;
        match results.len() {
            0 => {}
            1 => outputs[0] = js_to_val(&mut cx, returned, results[0])?,
            _ => {
                return Err(JsError::Type(
                    "multi-value returns from JS imports are not supported".to_string(),
                ));
            }
        }
        Ok(())
    });
    outcome.map_err(|err| wasmi::Error::new(err.to_string()))
}

/// Build a JS callable that marshals its arguments, drives the exported
/// wasm `func`, and marshals the results back.
fn make_export_function<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    store: &SharedStore,
    func: Func,
) -> Result<JsValue<'s>, JsError> {
    let (params, results): (Vec<ValType>, Vec<ValType>) = {
        let guard = store.lock().expect("wasm store poisoned");
        let ty = func.ty(&*guard);
        (ty.params().to_vec(), ty.results().to_vec())
    };
    let arity = u8::try_from(params.len()).unwrap_or(u8::MAX);
    let store = store.clone();
    let call = move |ctx: &mut NativeCtx<'_>, args: &[Value], _captures: &[Value]| {
        ctx.scope(|ctx, scope| {
            let mut cx = MarshalCx::new(ctx, scope);
            let mut inputs: Vec<Val> = Vec::with_capacity(params.len());
            for (index, ty) in params.iter().enumerate() {
                let handle = cx.park(args.get(index).copied().unwrap_or_else(Value::undefined));
                inputs
                    .push(js_to_val(&mut cx, handle, *ty).map_err(|err| {
                        err.into_native("WebAssembly.Instance exported function")
                    })?);
            }
            let mut outputs: Vec<Val> = results.iter().map(|ty| Val::default(*ty)).collect();
            drive_call(cx.ctx(), &store, func, &inputs, &mut outputs).map_err(|err| {
                NativeError::Thrown {
                    name: "WebAssembly.Instance exported function",
                    message: format!("RuntimeError: {err}"),
                }
            })?;
            let out = match outputs.as_slice() {
                [] => cx.undefined(),
                [single] => val_to_js(&mut cx, single),
                many => {
                    let array = cx
                        .array(many.len())
                        .map_err(|err| err.into_native("WebAssembly.Instance exported function"))?;
                    for (index, value) in many.iter().enumerate() {
                        let element = val_to_js(&mut cx, value);
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
    let scope = cx.scope();
    cx.ctx()
        .scoped_native_call(scope, "", arity, NativeCall::Dynamic(call))
        .map_err(|err| JsError::Type(err.to_string()))
}

/// The `[[Prototype]]` a built `Instance` object must carry:
/// `WebAssembly.Instance.prototype`, defined by `wasm.ns.js`.
fn instance_prototype<'s>(cx: &mut MarshalCx<'_, '_, 's>) -> Option<JsValue<'s>> {
    // The `Instance` class is a class-constructor value whose `.prototype`
    // the marshalling `get` cannot read, so `wasm.ns.js` mirrors the
    // prototype onto the namespace as a hidden object property.
    let namespace = cx.ctx().global_value("WebAssembly")?;
    let namespace = cx.park(namespace);
    let proto = cx.get(namespace, "__instanceProto").ok()?;
    cx.is_object(proto).then_some(proto)
}

/// Assemble the exports module-namespace object for an instantiated
/// module: functions become JS callables, memories/tables/globals become
/// their host-class wrappers sharing the instance's store.
fn build_exports<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    store: &SharedStore,
    instance: WasmiInstance,
) -> Result<JsValue<'s>, WasmThrow> {
    let exports: Vec<(String, Extern)> = {
        let guard = store.lock().expect("wasm store poisoned");
        instance
            .exports(&*guard)
            .map(|export| (export.name().to_string(), export.into_extern()))
            .collect()
    };
    let object = cx.object().map_err(WasmThrow::from_js)?;
    for (name, item) in exports {
        let value = match item {
            Extern::Func(func) => {
                make_export_function(cx, store, func).map_err(WasmThrow::from_js)?
            }
            Extern::Memory(memory) => WasmMemory {
                store: store.clone(),
                memory,
            }
            .into_js(cx)
            .map_err(WasmThrow::from_js)?,
            Extern::Global(global) => {
                let content = global
                    .ty(&*store.lock().expect("wasm store poisoned"))
                    .content();
                WasmGlobal {
                    store: store.clone(),
                    global,
                    content,
                }
                .into_js(cx)
                .map_err(WasmThrow::from_js)?
            }
            Extern::Table(table) => WasmTable {
                store: store.clone(),
                table,
            }
            .into_js(cx)
            .map_err(WasmThrow::from_js)?,
        };
        cx.set(object, &name, value).map_err(WasmThrow::from_js)?;
    }
    Ok(object)
}

/// Build the `Instance` object: a plain object carrying an own `exports`
/// data property, re-parented onto `WebAssembly.Instance.prototype`.
fn make_instance_object<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    exports: JsValue<'s>,
) -> Result<JsValue<'s>, WasmThrow> {
    let instance = cx.object().map_err(WasmThrow::from_js)?;
    cx.set(instance, "exports", exports)
        .map_err(WasmThrow::from_js)?;
    if let Some(proto) = instance_prototype(cx) {
        let proto_raw = cx.escape(proto);
        let instance_raw = cx.escape(instance);
        if let Some(object) = instance_raw.as_object() {
            object::set_prototype_value(object, cx.heap_mut(), Some(proto_raw));
        }
    }
    Ok(instance)
}

/// Read `importObject[module][name]`, returning the JS value if present
/// and non-`undefined`.
fn resolve_import<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    import_object: JsValue<'s>,
    module: &str,
    name: &str,
) -> Option<JsValue<'s>> {
    if cx.is_nullish(import_object) {
        return None;
    }
    let submodule = cx.get(import_object, module).ok()?;
    if cx.is_nullish(submodule) {
        return None;
    }
    let value = cx.get(submodule, name).ok()?;
    (!cx.is_undefined(value)).then_some(value)
}

/// Instantiate `module` with `import_object`, returning the built
/// `Instance` object. JS function imports are wired to host functions
/// that re-enter the VM; imported memories/tables/globals are rejected
/// because a fresh store cannot share their backing store.
fn instantiate_core<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    module: &WasmModule,
    import_object: JsValue<'s>,
) -> Result<JsValue<'s>, WasmThrow> {
    let engine = module.engine.clone();
    let store: SharedStore = Arc::new(Mutex::new(Store::new(&engine, StoreState::default())));
    let mut linker: Linker<StoreState> = Linker::new(&engine);

    let imports: Vec<(String, String, ExternType)> = module
        .module
        .imports()
        .map(|import| {
            (
                import.module().to_string(),
                import.name().to_string(),
                import.ty().clone(),
            )
        })
        .collect();

    for (module_name, field, ty) in &imports {
        match ty {
            ExternType::Func(func_ty) => {
                let Some(callback) = resolve_import(cx, import_object, module_name, field) else {
                    return Err(WasmThrow::link(format!(
                        "import '{module_name}.{field}' is not provided"
                    )));
                };
                if !cx.is_callable(callback) {
                    return Err(WasmThrow::link(format!(
                        "import '{module_name}.{field}' is not a function"
                    )));
                }
                let callback_raw = cx.escape(callback);
                let root = cx.ctx().persistent_root_insert(callback_raw);
                let results: Vec<ValType> = func_ty.results().to_vec();
                linker
                    .func_new(
                        module_name,
                        field,
                        func_ty.clone(),
                        move |caller, params, outputs| {
                            let bridge = caller.data().bridge;
                            run_import(bridge, params, outputs, root, &results)
                        },
                    )
                    .map_err(|err| WasmThrow::link(err.to_string()))?;
            }
            ExternType::Memory(_) | ExternType::Global(_) | ExternType::Table(_) => {
                return Err(WasmThrow::link(format!(
                    "importing '{module_name}.{field}': imported Memory/Table/Global \
                     across stores is not supported"
                )));
            }
        }
    }

    let instance = {
        let ctx_ptr: *mut NativeCtx<'_> = cx.ctx();
        let bridge = Bridge {
            ctx: ctx_ptr as usize,
        };
        let mut guard = store.lock().expect("wasm store poisoned");
        guard.data_mut().bridge = &bridge as *const Bridge as usize;
        let outcome = linker.instantiate_and_start(&mut *guard, &module.module);
        guard.data_mut().bridge = 0;
        outcome.map_err(|err| {
            if err.as_trap_code().is_some() {
                WasmThrow::runtime(err.to_string())
            } else {
                WasmThrow::link(err.to_string())
            }
        })?
    };

    let exports = build_exports(cx, &store, instance)?;
    make_instance_object(cx, exports)
}

/// Compile `bytes` into a `Module` object.
fn compile_module<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    handle: JsValue<'s>,
) -> Result<JsValue<'s>, WasmThrow> {
    let bytes = cx
        .buffer_source_bytes(handle)
        .ok_or_else(|| WasmThrow::type_error("expected a BufferSource of wasm bytes"))?;
    let engine = Engine::default();
    let module =
        WasmiModule::new(&engine, &bytes).map_err(|err| WasmThrow::compile(err.to_string()))?;
    WasmModule { engine, module }
        .into_js(cx)
        .map_err(WasmThrow::from_js)
}

/// Complete a namespace async method: fulfil with `result` or reject
/// with the typed error it describes.
fn settle_promise(
    cx: &mut MarshalCx<'_, '_, '_>,
    result: Result<JsValue<'_>, WasmThrow>,
    operation: &'static str,
) -> Result<Value, NativeError> {
    match result {
        Ok(value) => {
            let promise = cx
                .promise_fulfilled(value)
                .map_err(|err| err.into_native(operation))?;
            Ok(cx.escape(promise))
        }
        Err(throw) => {
            let reason = throw.to_value(cx);
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
    /// `WebAssembly.validate(bytes)` — true when `bytes` is a valid
    /// module.
    #[method(name = "validate", length = 1)]
    fn validate(bytes: BufferSource) -> bool {
        let engine = Engine::default();
        WasmiModule::validate(&engine, bytes.as_ref()).is_ok()
    }

    /// `WebAssembly.compile(bytes)` — a promise of a `Module`.
    #[method(name = "compile", length = 1, raw)]
    fn compile(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        ctx.scope(|ctx, scope| {
            let mut cx = MarshalCx::new(ctx, scope);
            let handle = cx.park(args.first().copied().unwrap_or_else(Value::undefined));
            let result = compile_module(&mut cx, handle);
            settle_promise(&mut cx, result, "WebAssembly.compile")
        })
    }

    /// `WebAssembly.instantiate(bytesOrModule, importObject?)` — a
    /// promise of `{ module, instance }` (bytes form) or of an
    /// `Instance` (module form).
    #[method(name = "instantiate", length = 1, raw)]
    fn instantiate(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        ctx.scope(|ctx, scope| {
            let mut cx = MarshalCx::new(ctx, scope);
            let source = cx.park(args.first().copied().unwrap_or_else(Value::undefined));
            let imports = cx.park(args.get(1).copied().unwrap_or_else(Value::undefined));
            let result = instantiate_entry(&mut cx, source, imports);
            settle_promise(&mut cx, result, "WebAssembly.instantiate")
        })
    }

    /// Synchronous instantiation backing `new WebAssembly.Instance(...)`.
    /// Consumed by `wasm.ns.js`, which re-types the thrown error.
    #[method(name = "__buildInstance", length = 2, raw)]
    fn build_instance(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        ctx.scope(|ctx, scope| {
            let mut cx = MarshalCx::new(ctx, scope);
            let module_handle = cx.park(args.first().copied().unwrap_or_else(Value::undefined));
            let imports = cx.park(args.get(1).copied().unwrap_or_else(Value::undefined));
            let module = cx
                .with_host_data::<WasmModule, WasmModule>(module_handle, Clone::clone)
                .map_err(|_| NativeError::Thrown {
                    name: "WebAssembly.Instance",
                    message: "LinkError: first argument must be a WebAssembly.Module".to_string(),
                })?;
            match instantiate_core(&mut cx, &module, imports) {
                Ok(instance) => Ok(cx.escape(instance)),
                Err(throw) => Err(NativeError::Thrown {
                    name: "WebAssembly.Instance",
                    message: format!("{}: {}", throw.kind, throw.message),
                }),
            }
        })
    }
}

/// Dispatch `WebAssembly.instantiate` over its two overloads.
fn instantiate_entry<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    source: JsValue<'s>,
    imports: JsValue<'s>,
) -> Result<JsValue<'s>, WasmThrow> {
    if let Ok(module) = cx.with_host_data::<WasmModule, WasmModule>(source, Clone::clone) {
        return instantiate_core(cx, &module, imports);
    }
    let bytes = cx
        .buffer_source_bytes(source)
        .ok_or_else(|| WasmThrow::type_error("expected a BufferSource or a WebAssembly.Module"))?;
    let engine = Engine::default();
    let module =
        WasmiModule::new(&engine, &bytes).map_err(|err| WasmThrow::compile(err.to_string()))?;
    let module = WasmModule { engine, module };
    let instance = instantiate_core(cx, &module, imports)?;
    let module_js = module.into_js(cx).map_err(WasmThrow::from_js)?;
    let result = cx.object().map_err(WasmThrow::from_js)?;
    cx.set(result, "module", module_js)
        .map_err(WasmThrow::from_js)?;
    cx.set(result, "instance", instance)
        .map_err(WasmThrow::from_js)?;
    Ok(result)
}

/// Compiled `WebAssembly.Module`: the owning engine plus the compiled
/// module (both cheap to clone).
#[derive(Clone, HostClass)]
pub struct WasmModule {
    engine: Engine,
    module: WasmiModule,
}

#[js_class(name = "WebAssembly.Module", feature = WEB)]
impl WasmModule {
    #[constructor]
    fn new(bytes: BufferSource) -> Result<WasmModule, JsError> {
        let engine = Engine::default();
        let module = WasmiModule::new(&engine, bytes.as_ref())
            .map_err(|err| JsError::Type(format!("CompileError: {err}")))?;
        Ok(WasmModule { engine, module })
    }
}

/// A `Memory` constructor descriptor (`{ initial, maximum? }`).
#[derive(FromJs)]
struct MemoryDescriptor {
    initial: f64,
    maximum: Option<f64>,
}

/// `WebAssembly.Memory`: a linear memory in its own store.
#[derive(Clone, HostClass)]
pub struct WasmMemory {
    store: SharedStore,
    memory: WasmiMemory,
}

#[js_class(name = "WebAssembly.Memory", feature = WEB)]
impl WasmMemory {
    #[constructor]
    fn new(descriptor: MemoryDescriptor) -> Result<WasmMemory, JsError> {
        let engine = Engine::default();
        let mut store = Store::new(&engine, StoreState::default());
        let ty = MemoryType::new(
            descriptor.initial as u32,
            descriptor.maximum.map(|value| value as u32),
        );
        let memory = WasmiMemory::new(&mut store, ty)
            .map_err(|err| JsError::Range(format!("Memory: {err}")))?;
        Ok(WasmMemory {
            store: Arc::new(Mutex::new(store)),
            memory,
        })
    }

    /// A fresh `ArrayBuffer` snapshot of the current linear memory. The
    /// engine has no shared-backing buffer, so this copies on every read.
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
            .map_err(|err| JsError::Range(format!("Memory.grow: {err}")))?;
        Ok(previous as f64)
    }
}

/// A `Global` constructor descriptor (`{ value, mutable? }`).
#[derive(FromJs)]
struct GlobalDescriptor {
    value: String,
    mutable: Option<bool>,
}

/// `WebAssembly.Global`: a single global cell in its own store.
#[derive(Clone, HostClass)]
pub struct WasmGlobal {
    store: SharedStore,
    global: WasmiGlobal,
    content: ValType,
}

#[js_class(name = "WebAssembly.Global", feature = WEB)]
impl WasmGlobal {
    #[constructor]
    fn new(descriptor: GlobalDescriptor, initial: Option<f64>) -> Result<WasmGlobal, JsError> {
        let content = parse_val_type(&descriptor.value)?;
        let engine = Engine::default();
        let mut store = Store::new(&engine, StoreState::default());
        let initial = initial.unwrap_or(0.0);
        let value = match content {
            ValType::I32 => Val::I32(to_wasm_i32(initial)),
            ValType::I64 => Val::I64(to_wasm_i64(initial)),
            ValType::F32 => Val::F32(F32::from_float(initial as f32)),
            ValType::F64 => Val::F64(F64::from_float(initial)),
            ValType::FuncRef | ValType::ExternRef | ValType::V128 => {
                return Err(JsError::Type(
                    "reference-type globals are not supported".to_string(),
                ));
            }
        };
        let mutability = if descriptor.mutable.unwrap_or(false) {
            Mutability::Var
        } else {
            Mutability::Const
        };
        let global = WasmiGlobal::new(&mut store, value, mutability);
        Ok(WasmGlobal {
            store: Arc::new(Mutex::new(store)),
            global,
            content,
        })
    }

    #[getter(name = "value")]
    fn get_value(&self) -> f64 {
        let guard = self.store.lock().expect("wasm store poisoned");
        match self.global.get(&*guard) {
            Val::I32(x) => f64::from(x),
            Val::I64(x) => x as f64,
            Val::F32(x) => f64::from(x.to_float()),
            Val::F64(x) => x.to_float(),
            _ => f64::NAN,
        }
    }

    #[setter(name = "value")]
    fn set_value(&self, value: f64) -> Result<(), JsError> {
        let new_value = match self.content {
            ValType::I32 => Val::I32(to_wasm_i32(value)),
            ValType::I64 => Val::I64(to_wasm_i64(value)),
            ValType::F32 => Val::F32(F32::from_float(value as f32)),
            ValType::F64 => Val::F64(F64::from_float(value)),
            ValType::FuncRef | ValType::ExternRef | ValType::V128 => {
                return Err(JsError::Type(
                    "reference-type globals are not supported".to_string(),
                ));
            }
        };
        let mut guard = self.store.lock().expect("wasm store poisoned");
        self.global
            .set(&mut *guard, new_value)
            .map_err(|err| JsError::Type(format!("Global.value: {err}")))
    }
}

/// A `Table` constructor descriptor (`{ element, initial, maximum? }`).
#[derive(FromJs)]
struct TableDescriptor {
    element: String,
    initial: f64,
    maximum: Option<f64>,
}

/// `WebAssembly.Table`: a reference-typed table in its own store. Element
/// reads/writes are limited to `null`: the engine's reference values are
/// not reflected back into JS.
#[derive(Clone, HostClass)]
pub struct WasmTable {
    store: SharedStore,
    table: WasmiTable,
}

#[js_class(name = "WebAssembly.Table", feature = WEB)]
impl WasmTable {
    #[constructor]
    fn new(descriptor: TableDescriptor) -> Result<WasmTable, JsError> {
        let element = parse_val_type(&descriptor.element)?;
        if !matches!(element, ValType::FuncRef | ValType::ExternRef) {
            return Err(JsError::Type(
                "Table element must be 'funcref' or 'externref'".to_string(),
            ));
        }
        let engine = Engine::default();
        let mut store = Store::new(&engine, StoreState::default());
        let ty = TableType::new(
            element,
            descriptor.initial as u32,
            descriptor.maximum.map(|value| value as u32),
        );
        let table = WasmiTable::new(&mut store, ty, Val::default(element))
            .map_err(|err| JsError::Range(format!("Table: {err}")))?;
        Ok(WasmTable {
            store: Arc::new(Mutex::new(store)),
            table,
        })
    }

    #[getter(name = "length")]
    fn length(&self) -> f64 {
        let guard = self.store.lock().expect("wasm store poisoned");
        self.table.size(&*guard) as f64
    }

    #[method(name = "grow", length = 1)]
    fn grow(&self, delta: f64) -> Result<f64, JsError> {
        let mut guard = self.store.lock().expect("wasm store poisoned");
        let element = self.table.ty(&*guard).element();
        let previous = self
            .table
            .grow(&mut *guard, delta as u64, Val::default(element))
            .map_err(|err| JsError::Range(format!("Table.grow: {err}")))?;
        Ok(previous as f64)
    }

    #[method(name = "get", length = 1)]
    fn get(&self, index: f64) -> Result<Option<bool>, JsError> {
        let guard = self.store.lock().expect("wasm store poisoned");
        match self.table.get(&*guard, index as u64) {
            // A present reference-typed slot surfaces as `null`; this
            // backend does not reflect stored funcref/externref values
            // back into JS.
            Some(_) => Ok(None),
            None => Err(JsError::Range("Table.get: index out of bounds".to_string())),
        }
    }

    #[method(name = "set", length = 2)]
    fn set(&self, index: f64, value: Option<bool>) -> Result<(), JsError> {
        if value.is_some() {
            return Err(JsError::Type(
                "Table.set only supports null elements in this build".to_string(),
            ));
        }
        let mut guard = self.store.lock().expect("wasm store poisoned");
        let element = self.table.ty(&*guard).element();
        self.table
            .set(&mut *guard, index as u64, Val::default(element))
            .map_err(|err| JsError::Range(format!("Table.set: {err}")))
    }
}
