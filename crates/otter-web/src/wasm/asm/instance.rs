//! Linking a translated asm.js module (V8 `AsmJs::InstantiateAsmWasm`).
//!
//! The linker checks the standard library and the heap, compiles the
//! translation with Wasmtime, aliases the heap `ArrayBuffer`'s bytes as the
//! module's linear memory, binds foreign imports, and returns the exports as
//! JavaScript functions. Any check that fails returns `None`, and the
//! function body then runs as ordinary JavaScript.
//!
//! # Contents
//! - [`link`] — the whole link step.
//! - [`HeapCreator`] / [`HeapMemory`] — the heap's bytes as Wasmtime memory.
//! - [`AsmInstance`] — the store, re-entry state and exported functions
//!   shared by every export.
//!
//! # Invariants
//! - The heap buffer is keyed against detach before it is aliased, so its
//!   storage never moves, resizes or frees while the buffer lives; every
//!   export captures the buffer, so the buffer outlives every call into the
//!   memory.
//! - Every load and store the translator emits is bounds-checked against the
//!   heap length, so wasm never addresses past the buffer.
//! - One store per linked module. The store is locked by the outermost call;
//!   a call made while an import runs JavaScript re-enters through the
//!   import's `Caller` instead (V8 allows asm.js callbacks into the module).
//! - Foreign callables are export captures, read through the active call's
//!   bridge; nothing about them is held outside the GC.
//!
//! # See also
//! - `super::translate` — the module this links.
//! - `super::super::run_import` — the general WebAssembly import bridge.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use otter_runtime::asm_stdlib::{self, StdlibFunction};
use otter_runtime::marshal::{JsError, JsValue, MarshalCx, to_int32};
use otter_runtime::{
    RuntimeNativeCall as NativeCall, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeNativeFn, RuntimeValue as Value, object,
};
use wasmtime::{
    Caller, Config, Engine, Extern, Func, FuncType, Global, GlobalType, Instance, LinearMemory,
    MemoryCreator, Module, Mutability, OptLevel, Store, Val, ValType,
};

use super::translate::{Exports, ForeignKind, Heap, Import, StdlibUse, Translation, translate};
use otter_runtime::byte_storage::{RESERVED_GUARD, RESERVED_SPAN};

/// Link the `"use asm"` function `source` with its three arguments. `None`
/// when any check fails and the body must run as JavaScript.
pub(super) fn link<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    source: &str,
    stdlib: JsValue<'s>,
    foreign: JsValue<'s>,
    heap: JsValue<'s>,
) -> Result<Option<JsValue<'s>>, JsError> {
    let heap_layout = valid_heap(cx, heap);
    let Ok(translation) = translate(source, heap_layout) else {
        return Ok(None);
    };
    if !stdlib_is_valid(cx, stdlib, &translation.stdlib) {
        return Ok(None);
    }
    // Foreign members are read in source order, as the body would.
    let mut foreign_values = Vec::with_capacity(translation.foreign.len());
    if !translation.foreign.is_empty() {
        if !cx.is_object(foreign) {
            return Ok(None);
        }
        for member in &translation.foreign {
            let value = cx.get(foreign, &member.name)?;
            let value = match member.kind {
                ForeignKind::Int => ForeignValue::Int(to_int32(cx.to_number_spec(value)?)),
                ForeignKind::Double => ForeignValue::Double(cx.to_number_spec(value)?),
                ForeignKind::Function => {
                    if !cx.is_callable(value) {
                        return Ok(None);
                    }
                    ForeignValue::Function(value)
                }
            };
            foreign_values.push(value);
        }
    }
    let memory = if translation.memory {
        let (Some(buffer), Some(layout)) = (cx.escape(heap).as_array_buffer(), heap_layout) else {
            return Ok(None);
        };
        let Some(keyed) = buffer.key_against_detach(cx.heap_mut()) else {
            return Ok(None);
        };
        if keyed.len != layout.len as usize || keyed.reserved != layout.reserved {
            return Ok(None);
        }
        Some(HeapCreator {
            base: keyed.base as usize,
            len: keyed.len,
            reserved: keyed.reserved,
        })
    } else {
        None
    };
    let Some(instance) = instantiate(&translation, memory, &foreign_values) else {
        return Ok(None);
    };
    // Every export captures the heap (keeping the aliased bytes alive) and
    // the foreign callables its imports call.
    let mut captures = Vec::with_capacity(1 + foreign_values.len());
    captures.push(if translation.memory {
        heap
    } else {
        cx.undefined()
    });
    for value in &foreign_values {
        captures.push(match value {
            ForeignValue::Function(function) => *function,
            _ => cx.undefined(),
        });
    }
    let instance = Arc::new(instance);
    let mut functions: Vec<(String, JsValue<'s>)> = Vec::new();
    for exported in &translation.functions {
        let function = export_function(cx, &instance, &exported.export, exported.arity, &captures)?;
        let name = cx.string(&exported.name)?;
        cx.define(
            function,
            "name",
            name,
            object::PropertyFlags::new(false, false, true),
        )?;
        functions.push((exported.export.clone(), function));
    }
    let function_of = |export: &str| {
        functions
            .iter()
            .find(|(name, _)| name == export)
            .map(|(_, function)| *function)
            .expect("every export has a function")
    };
    match &translation.exports {
        Exports::Single(export) => Ok(Some(function_of(export))),
        Exports::Object(entries) => {
            let object = cx.object()?;
            for (property, export) in entries {
                cx.set(object, property, function_of(export))?;
            }
            Ok(Some(object))
        }
    }
}

/// V8 `IsValidAsmjsMemorySize`: a fixed-length, unshared, attached buffer
/// of 2^12..2^24 bytes in powers of two, or a multiple of 2^24 up to 2^31.
fn valid_heap(cx: &mut MarshalCx<'_, '_, '_>, heap: JsValue<'_>) -> Option<Heap> {
    let buffer = cx.escape(heap).as_array_buffer()?;
    let store = cx.heap();
    if buffer.is_shared() || buffer.is_resizable(store) || buffer.is_detached(store) {
        return None;
    }
    let len = buffer.byte_length(store);
    let valid = if len < 1 << 24 {
        len >= 1 << 12 && len.is_power_of_two()
    } else {
        len % (1 << 24) == 0 && len <= 1 << 31
    };
    valid.then(|| Heap {
        len: len as u32,
        reserved: buffer.has_reserved_storage(store),
    })
}

/// V8 `AreStdlibMembersValid`: each captured member, read without running
/// JavaScript, is still the engine's own builtin or constant.
fn stdlib_is_valid(
    cx: &mut MarshalCx<'_, '_, '_>,
    stdlib: JsValue<'_>,
    uses: &[StdlibUse],
) -> bool {
    if uses.is_empty() {
        return true;
    }
    let math = cx.data_property(stdlib, "Math");
    uses.iter().all(|used| match used {
        StdlibUse::Function(function) => {
            let holder = match function {
                StdlibFunction::Int8Array
                | StdlibFunction::Uint8Array
                | StdlibFunction::Int16Array
                | StdlibFunction::Uint16Array
                | StdlibFunction::Int32Array
                | StdlibFunction::Uint32Array
                | StdlibFunction::Float32Array
                | StdlibFunction::Float64Array => Some(stdlib),
                _ => math,
            };
            holder
                .and_then(|holder| cx.data_property(holder, stdlib_name(*function)))
                .is_some_and(|value| {
                    let value = cx.escape(value);
                    asm_stdlib::is_builtin(value, *function, cx.heap())
                })
        }
        StdlibUse::MathConstant(name, expected) => math
            .and_then(|math| cx.data_property(math, name))
            .and_then(|value| cx.as_f64(value))
            .is_some_and(|value| value.to_bits() == expected.to_bits()),
        StdlibUse::Infinity => cx
            .data_property(stdlib, "Infinity")
            .and_then(|value| cx.as_f64(value))
            .is_some_and(|value| value == f64::INFINITY),
        StdlibUse::NaN => cx
            .data_property(stdlib, "NaN")
            .and_then(|value| cx.as_f64(value))
            .is_some_and(f64::is_nan),
    })
}

fn stdlib_name(function: StdlibFunction) -> &'static str {
    use StdlibFunction as F;
    match function {
        F::Acos => "acos",
        F::Asin => "asin",
        F::Atan => "atan",
        F::Cos => "cos",
        F::Sin => "sin",
        F::Tan => "tan",
        F::Exp => "exp",
        F::Log => "log",
        F::Ceil => "ceil",
        F::Floor => "floor",
        F::Sqrt => "sqrt",
        F::Abs => "abs",
        F::Min => "min",
        F::Max => "max",
        F::Atan2 => "atan2",
        F::Pow => "pow",
        F::Imul => "imul",
        F::Clz32 => "clz32",
        F::Fround => "fround",
        F::Int8Array => "Int8Array",
        F::Uint8Array => "Uint8Array",
        F::Int16Array => "Int16Array",
        F::Uint16Array => "Uint16Array",
        F::Int32Array => "Int32Array",
        F::Uint32Array => "Uint32Array",
        F::Float32Array => "Float32Array",
        F::Float64Array => "Float64Array",
    }
}

/// A foreign member's linked value.
enum ForeignValue<'s> {
    Int(i32),
    Double(f64),
    Function(JsValue<'s>),
}

// ---------------------------------------------------------------------------
// Heap memory
// ---------------------------------------------------------------------------

/// Hands Wasmtime the heap buffer's bytes as the module's one memory.
struct HeapCreator {
    base: usize,
    len: usize,
    /// The bytes start a `RESERVED_SPAN` reservation followed by a
    /// `RESERVED_GUARD`, so compiled accesses need no bounds checks.
    reserved: bool,
}

/// The heap buffer's bytes, owned by the buffer.
struct HeapMemory {
    base: usize,
    len: usize,
}

// SAFETY: the bytes belong to an `ArrayBuffer` keyed against detach (they
// never move, resize or free while the buffer lives), and the buffer is
// captured by every export, so it outlives every access the module makes.
// Size and capacity are the fixed byte length; growth beyond it fails.
unsafe impl LinearMemory for HeapMemory {
    fn byte_size(&self) -> usize {
        self.len
    }

    fn byte_capacity(&self) -> usize {
        self.len
    }

    fn grow_to(&mut self, new_size: usize) -> wasmtime::Result<()> {
        if new_size == self.len {
            Ok(())
        } else {
            Err(wasmtime::format_err!("an asm.js heap cannot grow"))
        }
    }

    fn as_ptr(&self) -> *mut u8 {
        self.base as *mut u8
    }
}

// SAFETY: see `HeapMemory`. A reserved heap provides exactly the
// reservation and guard the engine is configured with (it elides bounds
// checks against them); any other heap is configured with no guard and a
// reservation of its exact length, so compiled code relies on nothing beyond
// the buffer.
unsafe impl MemoryCreator for HeapCreator {
    fn new_memory(
        &self,
        _ty: wasmtime::MemoryType,
        minimum: usize,
        maximum: Option<usize>,
        reserved_size_in_bytes: Option<usize>,
        guard_size_in_bytes: usize,
    ) -> Result<Box<dyn LinearMemory>, String> {
        let layout_holds = if self.reserved {
            reserved_size_in_bytes.is_some_and(|reserved| reserved <= RESERVED_SPAN)
                && guard_size_in_bytes <= RESERVED_GUARD
        } else {
            guard_size_in_bytes == 0
        };
        if minimum != self.len || maximum != Some(self.len) || !layout_holds {
            return Err("asm.js heap memory has a fixed shape".to_string());
        }
        Ok(Box::new(HeapMemory {
            base: self.base,
            len: self.len,
        }))
    }
}

// ---------------------------------------------------------------------------
// Instance
// ---------------------------------------------------------------------------

/// Per-store host state: the active call's [`Bridge`] address (`0` idle).
#[derive(Default)]
struct AsmState {
    bridge: usize,
}

/// Live for one export call: the `NativeCtx` driving it and the export's
/// parked captures, which imports read foreign callables from.
struct Bridge<'s> {
    ctx: usize,
    captures: &'s [JsValue<'s>],
}

/// A linked module, shared by its exports.
struct AsmInstance {
    store: Mutex<Store<AsmState>>,
    /// Address of the `Caller` of the import running JavaScript, so a call
    /// back into the module re-enters through it; `0` otherwise.
    reentry: Arc<AtomicUsize>,
    instance: Instance,
}

fn instantiate(
    translation: &Translation,
    memory: Option<HeapCreator>,
    foreign: &[ForeignValue<'_>],
) -> Option<AsmInstance> {
    let mut config = Config::new();
    config.cranelift_opt_level(OptLevel::Speed);
    // Generated JavaScript carries no Spectre masking either; a module's
    // accesses stay within its heap by construction.
    // SAFETY: the flags only drop speculative-access masking.
    unsafe {
        config
            .cranelift_flag_set("enable_heap_access_spectre_mitigation", "false")
            .cranelift_flag_set("enable_table_access_spectre_mitigation", "false");
    }
    if let Some(creator) = memory {
        let len = creator.len as u64;
        let (reservation, guard) = if creator.reserved {
            (RESERVED_SPAN as u64, RESERVED_GUARD as u64)
        } else {
            (len, 0)
        };
        config
            .memory_reservation(reservation)
            .memory_guard_size(guard)
            .memory_reservation_for_growth(0)
            .memory_may_move(false)
            .guard_before_linear_memory(false)
            .memory_init_cow(false)
            .wasm_custom_page_sizes(len % 65536 != 0)
            .with_host_memory(Arc::new(creator));
    }
    let engine = Engine::new(&config).ok()?;
    let module = Module::new(&engine, &translation.wasm).ok()?;
    let mut store = Store::new(&engine, AsmState::default());
    let reentry = Arc::new(AtomicUsize::new(0));
    let mut imports: Vec<Extern> = Vec::with_capacity(translation.imports.len());
    for import in &translation.imports {
        let item: Extern = match import {
            Import::Value {
                foreign: index,
                double,
            } => {
                let (ty, value) = match (&foreign[*index as usize], double) {
                    (ForeignValue::Int(value), false) => (ValType::I32, Val::I32(*value)),
                    (ForeignValue::Double(value), true) => {
                        (ValType::F64, Val::F64(value.to_bits()))
                    }
                    _ => return None,
                };
                Global::new(&mut store, GlobalType::new(ty, Mutability::Const), value)
                    .ok()?
                    .into()
            }
            Import::Function {
                foreign: index,
                params,
                result,
            } => foreign_function(&mut store, &engine, &reentry, *index, params, *result).into(),
            Import::Math(function) => math_function(&mut store, &engine, *function).into(),
            Import::Fmod => {
                let ty = FuncType::new(&engine, [ValType::F64, ValType::F64], [ValType::F64]);
                Func::new(&mut store, ty, |_caller, params, results| {
                    let (Val::F64(a), Val::F64(b)) = (&params[0], &params[1]) else {
                        unreachable!("typed by the import");
                    };
                    results[0] = Val::F64((f64::from_bits(*a) % f64::from_bits(*b)).to_bits());
                    Ok(())
                })
                .into()
            }
        };
        imports.push(item);
    }
    let instance = Instance::new(&mut store, &module, &imports).ok()?;
    Some(AsmInstance {
        store: Mutex::new(store),
        reentry,
        instance,
    })
}

fn math_function(store: &mut Store<AsmState>, engine: &Engine, function: StdlibFunction) -> Func {
    let arity = if matches!(function, StdlibFunction::Atan2 | StdlibFunction::Pow) {
        2
    } else {
        1
    };
    let ty = FuncType::new(engine, vec![ValType::F64; arity], [ValType::F64]);
    Func::new(store, ty, move |_caller, params, results| {
        let mut args = [0.0f64; 2];
        for (arg, param) in args.iter_mut().zip(params) {
            let Val::F64(bits) = param else {
                unreachable!("typed by the import");
            };
            *arg = f64::from_bits(*bits);
        }
        let value = asm_stdlib::math_f64(function, &args[..params.len()]).unwrap_or(f64::NAN);
        results[0] = Val::F64(value.to_bits());
        Ok(())
    })
}

/// A wasm import calling the foreign callable `index` with numbers, and
/// coercing its result as the call site does (`|0` or unary `+`).
fn foreign_function(
    store: &mut Store<AsmState>,
    engine: &Engine,
    reentry: &Arc<AtomicUsize>,
    index: u32,
    params: &[wasm_encoder::ValType],
    result: Option<wasm_encoder::ValType>,
) -> Func {
    let wasm_type = |ty: &wasm_encoder::ValType| match ty {
        wasm_encoder::ValType::F64 => ValType::F64,
        _ => ValType::I32,
    };
    let ty = FuncType::new(
        engine,
        params.iter().map(wasm_type),
        result.iter().map(wasm_type),
    );
    let reentry = reentry.clone();
    Func::new(
        store,
        ty,
        move |mut caller: Caller<'_, AsmState>, params, results| {
            let bridge = caller.data().bridge;
            if bridge == 0 {
                return Err(wasmtime::Error::new(JsError::Native(
                    NativeError::InvalidOperand,
                )));
            }
            let previous =
                reentry.swap(std::ptr::from_mut(&mut caller) as usize, Ordering::Relaxed);
            // SAFETY: the export call that published `bridge` is on the stack
            // below this import for the whole Wasmtime call.
            let bridge = unsafe { &*(bridge as *const Bridge<'_>) };
            let outcome = call_foreign(bridge, index, params);
            reentry.store(previous, Ordering::Relaxed);
            let value = outcome.map_err(wasmtime::Error::new)?;
            if let Some(slot) = results.first_mut() {
                *slot = match slot {
                    Val::F64(_) => Val::F64(value.to_bits()),
                    _ => Val::I32(to_int32(value)),
                };
            }
            Ok(())
        },
    )
}

/// Call the foreign callable with the import's numeric arguments; the
/// result as a number (`ToNumber`, which the call site's coercion applies).
fn call_foreign(bridge: &Bridge<'_>, index: u32, params: &[Val]) -> Result<f64, JsError> {
    // SAFETY: the driving export call owns this `NativeCtx` for the whole
    // Wasmtime call; the import runs synchronously inside it.
    let ctx: &mut NativeCtx<'_> = unsafe { &mut *(bridge.ctx as *mut NativeCtx<'_>) };
    let callee = bridge.captures[1 + index as usize];
    ctx.scope(|scope| {
        let mut cx = MarshalCx::new(scope);
        let callee = cx.escape(callee);
        let callee = cx.park(callee);
        let mut args = Vec::with_capacity(params.len());
        for param in params {
            args.push(match param {
                Val::I32(value) => cx.number(f64::from(*value)),
                Val::F64(bits) => cx.number(f64::from_bits(*bits)),
                _ => return Err(JsError::Native(NativeError::InvalidOperand)),
            });
        }
        let this = cx.undefined();
        let returned = cx.call(callee, this, &args)?;
        cx.to_number_spec(returned)
    })
}

/// The JavaScript function for one export: converts arguments as the asm.js
/// parameter annotations do, runs the wasm function, and returns its result
/// as a number.
fn export_function<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    instance: &Arc<AsmInstance>,
    export: &str,
    arity: u8,
    captures: &[JsValue<'s>],
) -> Result<JsValue<'s>, JsError> {
    let (func, params, results) = {
        let mut store = instance.store.lock().expect("asm.js store poisoned");
        let func = instance
            .instance
            .get_func(&mut *store, export)
            .ok_or(JsError::Native(NativeError::InvalidOperand))?;
        let ty = func.ty(&*store);
        (
            func,
            ty.params().collect::<Vec<_>>(),
            ty.results().collect::<Vec<_>>(),
        )
    };
    let instance = instance.clone();
    let call = move |ctx: &mut NativeCtx<'_>, args: &[Value], captures: &[Value]| {
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let captures: Vec<JsValue<'_>> = captures.iter().map(|value| cx.park(*value)).collect();
            let mut inputs = Vec::with_capacity(params.len());
            for (position, ty) in params.iter().enumerate() {
                let arg = cx.park(args.get(position).copied().unwrap_or_else(Value::undefined));
                let number = cx
                    .to_number_spec(arg)
                    .map_err(|error| error.into_native("asm.js export"))?;
                inputs.push(match ty {
                    ValType::I32 => Val::I32(to_int32(number)),
                    ValType::F32 => Val::F32((number as f32).to_bits()),
                    _ => Val::F64(number.to_bits()),
                });
            }
            let mut outputs: Vec<Val> = results
                .iter()
                .map(|ty| match ty {
                    ValType::I32 => Val::I32(0),
                    ValType::F32 => Val::F32(0),
                    _ => Val::F64(0),
                })
                .collect();
            let bridge = Bridge {
                ctx: std::ptr::from_mut(cx.ctx()) as usize,
                captures: &captures,
            };
            instance
                .call(func, &bridge, &inputs, &mut outputs)
                .map_err(|error| error.into_native("asm.js export"))?;
            let value = match outputs.first() {
                None => cx.undefined(),
                Some(Val::I32(value)) => cx.number(f64::from(*value)),
                Some(Val::F32(bits)) => cx.number(f64::from(f32::from_bits(*bits))),
                Some(Val::F64(bits)) => cx.number(f64::from_bits(*bits)),
                Some(_) => return Err(NativeError::InvalidOperand),
            };
            Ok(cx.escape(value))
        })
    };
    let call: Arc<RuntimeNativeFn> = Arc::new(call);
    cx.native_call_capturing("", arity, NativeCall::Dynamic(call), captures)
}

impl AsmInstance {
    /// Run `func`, from the outermost call by locking the store, or from a
    /// JavaScript import's callback through that import's `Caller`.
    fn call(
        &self,
        func: Func,
        bridge: &Bridge<'_>,
        inputs: &[Val],
        outputs: &mut [Val],
    ) -> Result<(), JsError> {
        let bridge_addr = std::ptr::from_ref(bridge) as usize;
        let result = match self.store.try_lock() {
            Ok(mut store) => {
                store.data_mut().bridge = bridge_addr;
                let result = func.call(&mut *store, inputs, outputs);
                store.data_mut().bridge = 0;
                result
            }
            Err(_) => {
                let caller = self.reentry.load(Ordering::Relaxed);
                if caller == 0 {
                    return Err(JsError::Native(NativeError::InvalidOperand));
                }
                // SAFETY: an import of this store is running JavaScript with
                // this `Caller` published and untouched until it returns.
                let caller = unsafe { &mut *(caller as *mut Caller<'_, AsmState>) };
                let previous = std::mem::replace(&mut caller.data_mut().bridge, bridge_addr);
                let result = func.call(&mut *caller, inputs, outputs);
                caller.data_mut().bridge = previous;
                result
            }
        };
        result.map_err(from_wasmtime)
    }
}

/// A JavaScript throw from an import comes back as its own error; a trap
/// (only stack exhaustion can trap) becomes a `RangeError`.
fn from_wasmtime(error: wasmtime::Error) -> JsError {
    match error.downcast::<JsError>() {
        Ok(error) => error,
        Err(error) => JsError::Range(format!("asm.js: {error}")),
    }
}

/// The bare-function linker hook installed as the realm's `__otterAsmLink`.
pub(crate) fn link_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let arg = |index: usize| args.get(index).copied().unwrap_or_else(Value::undefined);
    ctx.scope(|scope| {
        let mut cx = MarshalCx::new(scope);
        let source = cx.park(arg(0));
        let Some(source) = cx.as_string_lossy(source) else {
            return Ok(Value::undefined());
        };
        let stdlib = cx.park(arg(2));
        let foreign = cx.park(arg(3));
        let heap = cx.park(arg(4));
        match link(&mut cx, &source, stdlib, foreign, heap) {
            Ok(Some(exports)) => Ok(cx.escape(exports)),
            Ok(None) => Ok(Value::undefined()),
            Err(error) => Err(error.into_native("asm.js link")),
        }
    })
}
