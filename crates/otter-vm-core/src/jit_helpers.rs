//! Runtime helper implementations for JIT-compiled code.
//!
//! These `extern "C"` functions are called from Cranelift-generated machine code
//! to handle operations that need VM context (property access, function calls, etc.).
//!
//! # Safety
//!
//! All helpers receive a `*mut u8` context pointer that is actually a `*const JitContext`.
//! The context is constructed by `try_execute_jit` and is valid for the duration of
//! JIT execution. No GC can occur during JIT execution (helpers don't allocate).

#![cfg(feature = "jit")]

use otter_vm_bytecode::Function;
use otter_vm_bytecode::function::InlineCacheState;
use otter_vm_gc::object::{GcAllocation, GcHeader, tags as gc_tags};
use otter_vm_jit::BAILOUT_SENTINEL;
use otter_vm_jit::runtime_helpers::{HelperKind, RuntimeHelpers};

use crate::object::JsObject;

// NaN-boxing constants (must match value.rs)
const TAG_MASK: u64 = 0xFFFF_0000_0000_0000;
const PAYLOAD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
const TAG_POINTER: u64 = 0x7FFC_0000_0000_0000;

/// Opaque context passed as the first argument to every JIT-compiled function.
///
/// Constructed by `try_execute_jit` before calling JIT code. Contains everything
/// runtime helpers need to perform IC lookups, property access, and function calls.
#[repr(C)]
pub struct JitContext {
    /// Pointer to the bytecode Function (for feedback vector access).
    pub function_ptr: *const Function,
    /// Cached prototype epoch (for IC invalidation checks).
    pub proto_epoch: u64,
    /// Pointer to the Interpreter (for re-entrant function calls).
    /// Null when call helpers are not available (e.g. in unit tests).
    pub interpreter: *const crate::interpreter::Interpreter,
    /// Pointer to the VmContext (for re-entrant function calls).
    /// Null when call helpers are not available.
    pub vm_ctx: *mut crate::context::VmContext,
    /// Pointer to module constant pool (for resolving ConstantIndex to names).
    /// Null when constants are not available (e.g. in unit tests).
    pub constants: *const otter_vm_bytecode::ConstantPool,
}

/// Runtime helper: GetPropConst — IC fast-path property read.
///
/// Signature: `(ctx: i64, obj: i64, name_idx: i64, ic_idx: i64) -> i64`
///
/// Checks inline cache for the object's shape. On IC hit, reads the property
/// at the cached offset and returns NaN-boxed bits. On miss or unsupported
/// object type, returns BAILOUT_SENTINEL.
///
/// # Safety
///
/// - `ctx_raw` must point to a valid `JitContext`
/// - `obj_raw` must be a valid NaN-boxed value
/// - No GC must occur during this call (guaranteed by caller)
#[allow(unsafe_code)]
extern "C" fn otter_rt_get_prop_const(
    ctx_raw: i64,
    obj_raw: i64,
    _name_idx: i64,
    ic_idx: i64,
) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    let bits = obj_raw as u64;

    // Only handle heap objects (TAG_POINTER)
    if (bits & TAG_MASK) != TAG_POINTER {
        return BAILOUT_SENTINEL;
    }

    let raw_ptr = (bits & PAYLOAD_MASK) as *const u8;
    if raw_ptr.is_null() {
        return BAILOUT_SENTINEL;
    }

    // Read GC header tag to verify this is a JsObject.
    // GcAllocation<T> layout: [GcHeader (8 bytes)] [T value]
    // raw_ptr points to T (the value), so header is 8 bytes before.
    let header_offset = std::mem::offset_of!(GcAllocation<JsObject>, value);
    let header_ptr = unsafe { raw_ptr.sub(header_offset) as *const GcHeader };
    let tag = unsafe { (*header_ptr).tag() };

    if tag != gc_tags::OBJECT {
        return BAILOUT_SENTINEL;
    }

    // SAFETY: We verified the tag is OBJECT, so raw_ptr is *const JsObject.
    // The object is alive (reachable from interpreter stack). No GC during JIT.
    let obj_ref = unsafe { &*(raw_ptr as *const JsObject) };

    // Check dictionary mode — IC doesn't apply
    if obj_ref.is_dictionary_mode() {
        return BAILOUT_SENTINEL;
    }

    // Get shape pointer for comparison
    let obj_shape_ptr = std::sync::Arc::as_ptr(&obj_ref.shape()) as u64;

    // Read IC from feedback vector
    let function = unsafe { &*ctx.function_ptr };
    let feedback = function.feedback_vector.write();
    let Some(ic) = feedback.get_mut(ic_idx as usize) else {
        return BAILOUT_SENTINEL;
    };

    // Check proto epoch
    if !ic.proto_epoch_matches(ctx.proto_epoch) {
        return BAILOUT_SENTINEL;
    }

    // IC fast path — extract offset from IC state, then read property
    let cached_offset: Option<u32> = match &ic.ic_state {
        InlineCacheState::Monomorphic { shape_id, offset } => {
            if obj_shape_ptr == *shape_id {
                Some(*offset)
            } else {
                None
            }
        }
        InlineCacheState::Polymorphic { count, entries } => {
            let mut found = None;
            for i in 0..(*count as usize) {
                if obj_shape_ptr == entries[i].0 {
                    found = Some(entries[i].1);
                    break;
                }
            }
            found
        }
        _ => None,
    };

    if let Some(offset) = cached_offset {
        if let Some(val) = obj_ref.get_by_offset(offset as usize) {
            ic.record_hit();
            // MRU reordering for polymorphic
            if let InlineCacheState::Polymorphic { count, entries } = &mut ic.ic_state {
                for i in 1..(*count as usize) {
                    if obj_shape_ptr == entries[i].0 {
                        entries.swap(0, i);
                        break;
                    }
                }
            }
            return val.to_jit_bits();
        }
    }

    // IC miss — bail out to interpreter
    BAILOUT_SENTINEL
}

/// Runtime helper: SetPropConst — IC fast-path property write.
///
/// Signature: `(ctx: i64, obj: i64, name_idx: i64, value: i64, ic_idx: i64) -> i64`
///
/// Checks inline cache for the object's shape. On IC hit, writes the value
/// at the cached offset and returns 0. On miss, returns BAILOUT_SENTINEL.
///
/// # Safety
///
/// Same safety requirements as `otter_rt_get_prop_const`.
#[allow(unsafe_code)]
extern "C" fn otter_rt_set_prop_const(
    ctx_raw: i64,
    obj_raw: i64,
    _name_idx: i64,
    value_raw: i64,
    ic_idx: i64,
) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    let bits = obj_raw as u64;

    // Only handle heap objects (TAG_POINTER)
    if (bits & TAG_MASK) != TAG_POINTER {
        return BAILOUT_SENTINEL;
    }

    let raw_ptr = (bits & PAYLOAD_MASK) as *const u8;
    if raw_ptr.is_null() {
        return BAILOUT_SENTINEL;
    }

    // Read GC header tag to verify this is a JsObject.
    let header_offset = std::mem::offset_of!(GcAllocation<JsObject>, value);
    let header_ptr = unsafe { raw_ptr.sub(header_offset) as *const GcHeader };
    let tag = unsafe { (*header_ptr).tag() };

    if tag != gc_tags::OBJECT {
        return BAILOUT_SENTINEL;
    }

    let obj_ref = unsafe { &*(raw_ptr as *const JsObject) };

    if obj_ref.is_dictionary_mode() {
        return BAILOUT_SENTINEL;
    }

    let obj_shape_ptr = std::sync::Arc::as_ptr(&obj_ref.shape()) as u64;

    let function = unsafe { &*ctx.function_ptr };
    let feedback = function.feedback_vector.write();
    let Some(ic) = feedback.get_mut(ic_idx as usize) else {
        return BAILOUT_SENTINEL;
    };

    if !ic.proto_epoch_matches(ctx.proto_epoch) {
        return BAILOUT_SENTINEL;
    }

    // Reconstruct the Value to write.
    // For non-pointer values, we can reconstruct from bits directly.
    // For pointer values, we bail (the object is reachable but we can't
    // reconstruct the HeapRef safely without knowing the type).
    let value_bits = value_raw as u64;
    let write_value = if (value_bits & TAG_MASK) == TAG_POINTER {
        // Heap value — bail out to interpreter for proper GC-safe handling
        return BAILOUT_SENTINEL;
    } else {
        match crate::value::Value::from_jit_bits(value_bits) {
            Some(v) => v,
            None => return BAILOUT_SENTINEL,
        }
    };

    // IC fast path — extract offset, then write
    let cached_offset: Option<u32> = match &ic.ic_state {
        InlineCacheState::Monomorphic { shape_id, offset } => {
            if obj_shape_ptr == *shape_id {
                Some(*offset)
            } else {
                None
            }
        }
        InlineCacheState::Polymorphic { count, entries } => {
            let mut found = None;
            for i in 0..(*count as usize) {
                if obj_shape_ptr == entries[i].0 {
                    found = Some(entries[i].1);
                    break;
                }
            }
            found
        }
        _ => None,
    };

    if let Some(offset) = cached_offset {
        if obj_ref.set_by_offset(offset as usize, write_value).is_ok() {
            ic.record_hit();
            // MRU reordering for polymorphic
            if let InlineCacheState::Polymorphic { count, entries } = &mut ic.ic_state {
                for i in 1..(*count as usize) {
                    if obj_shape_ptr == entries[i].0 {
                        entries.swap(0, i);
                        break;
                    }
                }
            }
            return 0;
        }
    }

    BAILOUT_SENTINEL
}

/// Runtime helper: CallFunction — re-entrant function call.
///
/// Signature: `(ctx: i64, callee: i64, argc: i64, argv_ptr: i64) -> i64`
///
/// Reconstructs the callee Value from NaN-boxed bits, collects arguments from
/// the argv pointer, and uses the interpreter to perform the call. Returns the
/// result as NaN-boxed bits, or BAILOUT_SENTINEL on error or unsupported case.
///
/// # Safety
///
/// - `ctx_raw` must point to a valid `JitContext` with non-null interpreter/vm_ctx
/// - `argv_ptr` must point to `argc` contiguous i64 values
/// - No GC must occur during this call
#[allow(unsafe_code)]
extern "C" fn otter_rt_call_function(
    ctx_raw: i64,
    callee_raw: i64,
    argc_raw: i64,
    argv_ptr_raw: i64,
) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };

    // Need interpreter and vm_ctx for re-entrant calls
    if ctx.interpreter.is_null() || ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }

    // Reconstruct the callee Value from raw bits.
    // SAFETY: We're in a no-GC JIT scope, pointer is valid.
    let callee = match unsafe { crate::value::Value::from_raw_bits_unchecked(callee_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };

    // Check that the callee is actually callable
    if !callee.is_function() && !callee.is_native_function() && callee.as_object().is_none()
        && callee.as_proxy().is_none()
    {
        return BAILOUT_SENTINEL;
    }

    // Collect arguments from the argv pointer
    let argc = argc_raw as usize;
    let mut args = Vec::with_capacity(argc);
    if argc > 0 {
        let argv = argv_ptr_raw as *const i64;
        for i in 0..argc {
            let bits = unsafe { *argv.add(i) } as u64;
            // Reconstruct each argument Value
            let arg = match unsafe { crate::value::Value::from_raw_bits_unchecked(bits) } {
                Some(v) => v,
                None => return BAILOUT_SENTINEL,
            };
            args.push(arg);
        }
    }

    // SAFETY: interpreter and vm_ctx are valid pointers set by try_execute_jit.
    // The interpreter is paused (not executing instructions), and we have
    // exclusive access during this synchronous JIT helper call.
    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    // Perform the call via the interpreter
    match interpreter.call_function(vm_ctx, &callee, crate::value::Value::undefined(), &args) {
        Ok(result) => result.to_jit_bits(),
        Err(_) => BAILOUT_SENTINEL,
    }
}

/// Runtime helper: NewObject — create a new empty object with Object.prototype.
///
/// Signature: `(ctx: i64) -> i64`
///
/// Creates a new JsObject with the correct prototype chain and returns it
/// as NaN-boxed bits. Returns BAILOUT_SENTINEL on failure.
#[allow(unsafe_code)]
extern "C" fn otter_rt_new_object(ctx_raw: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };

    if ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }

    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    // Get Object.prototype for proper prototype chain
    let proto = vm_ctx
        .global()
        .get(&crate::object::PropertyKey::string("Object"))
        .and_then(|obj_ctor| {
            obj_ctor
                .as_object()
                .and_then(|o| o.get(&crate::object::PropertyKey::string("prototype")))
        })
        .and_then(|proto_val| proto_val.as_object());

    let obj = crate::gc::GcRef::new(JsObject::new(
        proto
            .map(crate::value::Value::object)
            .unwrap_or_else(crate::value::Value::null),
        vm_ctx.memory_manager().clone(),
    ));
    crate::value::Value::object(obj).to_jit_bits()
}

/// Runtime helper: NewArray — create a new empty array with Array.prototype.
///
/// Signature: `(ctx: i64, len: i64) -> i64`
///
/// Creates a new JsObject in array mode with the given initial capacity.
/// Returns BAILOUT_SENTINEL on failure.
#[allow(unsafe_code)]
extern "C" fn otter_rt_new_array(ctx_raw: i64, len_raw: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };

    if ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }

    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    let len = len_raw as usize;

    let arr = crate::gc::GcRef::new(JsObject::array(len, vm_ctx.memory_manager().clone()));

    // Attach Array.prototype for iterable support and methods
    if let Some(array_obj) = vm_ctx
        .get_global("Array")
        .and_then(|v| v.as_object())
    {
        if let Some(array_proto) = array_obj
            .get(&crate::object::PropertyKey::string("prototype"))
            .and_then(|v| v.as_object())
        {
            arr.set_prototype(crate::value::Value::object(array_proto));
        }
    }

    crate::value::Value::array(arr).to_jit_bits()
}

/// Helper: resolve constant index to string name from JitContext's constant pool.
///
/// # Safety
/// `ctx` must point to a valid JitContext with non-null `constants` pointer.
#[allow(unsafe_code)]
unsafe fn resolve_constant_string<'a>(ctx: &JitContext, name_idx: i64) -> Option<&'a [u16]> {
    if ctx.constants.is_null() {
        return None;
    }
    let pool = unsafe { &*ctx.constants };
    let constant = pool.get(name_idx as u32)?;
    constant.as_string()
}

/// Runtime helper: GetGlobal — read a global variable by name.
///
/// Signature: `(ctx: i64, name_idx: i64, ic_idx: i64) -> i64`
///
/// IC fast path on the global object's shape, then falls back to
/// `VmContext::get_global_utf16`. Returns BAILOUT_SENTINEL if the variable
/// is not defined (ReferenceError must be handled by the interpreter).
#[allow(unsafe_code)]
extern "C" fn otter_rt_get_global(ctx_raw: i64, name_idx: i64, ic_idx: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };

    if ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }

    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    // IC fast path on global object
    let global_obj = vm_ctx.global();
    if !global_obj.is_dictionary_mode() {
        let function = unsafe { &*ctx.function_ptr };
        let feedback = function.feedback_vector.read();
        if let Some(ic) = feedback.get(ic_idx as usize) {
            if let InlineCacheState::Monomorphic {
                shape_id: shape_addr,
                offset,
            } = &ic.ic_state
            {
                if std::sync::Arc::as_ptr(&global_obj.shape()) as u64 == *shape_addr {
                    if let Some(val) = global_obj.get_by_offset(*offset as usize) {
                        return val.to_jit_bits();
                    }
                }
            }
        }
    }

    // Slow path: name lookup
    let Some(name_str) = (unsafe { resolve_constant_string(ctx, name_idx) }) else {
        return BAILOUT_SENTINEL;
    };

    match vm_ctx.get_global_utf16(name_str) {
        Some(value) => value.to_jit_bits(),
        None => BAILOUT_SENTINEL, // not defined → bail for ReferenceError
    }
}

/// Runtime helper: SetGlobal — write a global variable by name.
///
/// Signature: `(ctx: i64, name_idx: i64, value: i64, ic_idx: i64, is_decl: i64) -> i64`
///
/// Returns 0 on success, BAILOUT_SENTINEL on failure.
#[allow(unsafe_code)]
extern "C" fn otter_rt_set_global(
    ctx_raw: i64,
    name_idx: i64,
    value_raw: i64,
    _ic_idx: i64,
    _is_decl: i64,
) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };

    if ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }

    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    let Some(name_str) = (unsafe { resolve_constant_string(ctx, name_idx) }) else {
        return BAILOUT_SENTINEL;
    };

    // Reconstruct the value from NaN-boxed bits
    let value_bits = value_raw as u64;
    let value = match unsafe { crate::value::Value::from_raw_bits_unchecked(value_bits) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };

    let key = crate::object::PropertyKey::from_js_string(crate::string::JsString::intern_utf16(name_str));
    let global_obj = vm_ctx.global();
    if global_obj.set(key, value).is_ok() {
        0
    } else {
        BAILOUT_SENTINEL
    }
}

/// Build a `RuntimeHelpers` table with all available helper functions.
pub fn build_runtime_helpers() -> RuntimeHelpers {
    let mut helpers = RuntimeHelpers::new();
    // SAFETY: Function signatures match HelperKind conventions.
    unsafe {
        helpers.set(HelperKind::GetPropConst, otter_rt_get_prop_const as *const u8);
        helpers.set(HelperKind::SetPropConst, otter_rt_set_prop_const as *const u8);
        helpers.set(HelperKind::CallFunction, otter_rt_call_function as *const u8);
        helpers.set(HelperKind::NewObject, otter_rt_new_object as *const u8);
        helpers.set(HelperKind::NewArray, otter_rt_new_array as *const u8);
        helpers.set(HelperKind::GetGlobal, otter_rt_get_global as *const u8);
        helpers.set(HelperKind::SetGlobal, otter_rt_set_global as *const u8);
    }
    helpers
}
