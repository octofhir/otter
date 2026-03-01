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

use otter_vm_bytecode::Function;
use otter_vm_bytecode::function::InlineCacheState;
use otter_vm_gc::object::{GcAllocation, GcHeader, tags as gc_tags};
use otter_vm_jit::BAILOUT_SENTINEL;
use otter_vm_jit::runtime_helpers::{HelperKind, RuntimeHelpers};

use crate::gc::GcRef;
use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::UpvalueCell;

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
    /// Pointer to the upvalue cells array for closure support.
    /// Points to the first element of the `Vec<UpvalueCell>` slice.
    /// Null when the function has no upvalues.
    pub upvalues_ptr: *const UpvalueCell,
    /// Number of upvalue cells in the array.
    pub upvalue_count: u32,
    /// NaN-boxed `this` value for the JIT-compiled function.
    /// Snapshotted from `VmContext::pending_this` before JIT entry,
    /// with non-strict undefined→globalThis substitution already applied.
    pub this_raw: i64,
    /// NaN-boxed callee value (the function being called).
    /// Used by class opcodes (CallSuper, GetSuper) and arguments.callee.
    pub callee_raw: i64,
    /// NaN-boxed home_object (GcRef<JsObject>) for class methods.
    /// Used by GetSuper, CallSuper, GetSuperProp.
    pub home_object_raw: i64,
    /// Secondary return value for multi-result opcodes (e.g. IteratorNext done flag).
    /// Written by the helper, read by JIT code after the call returns.
    pub secondary_result: i64,
}

// Compile-time check: JIT_CTX_SECONDARY_RESULT_OFFSET in runtime_helpers.rs must match
// the actual byte offset of `secondary_result` in JitContext.
const _: () = {
    assert!(
        std::mem::offset_of!(JitContext, secondary_result) == 80,
        "JitContext::secondary_result offset changed — update JIT_CTX_SECONDARY_RESULT_OFFSET in runtime_helpers.rs"
    );
};

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
    if !callee.is_function()
        && !callee.is_native_function()
        && callee.as_object().is_none()
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
    if let Some(array_obj) = vm_ctx.get_global("Array").and_then(|v| v.as_object()) {
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

    let key =
        crate::object::PropertyKey::from_js_string(crate::string::JsString::intern_utf16(name_str));
    let global_obj = vm_ctx.global();
    if global_obj.set(key, value).is_ok() {
        0
    } else {
        BAILOUT_SENTINEL
    }
}

/// Runtime helper: GetUpvalue — read a captured variable from upvalue cell.
///
/// Signature: `(ctx: i64, idx: i64) -> i64`
///
/// Indexes into the upvalue cells array from JitContext and returns the
/// current value as NaN-boxed bits. Returns BAILOUT_SENTINEL if the index
/// is out of bounds or the upvalue pointer is null.
///
/// # Safety
///
/// - `ctx_raw` must point to a valid `JitContext`
/// - The upvalue cells must be alive for the duration of JIT execution
#[allow(unsafe_code)]
extern "C" fn otter_rt_get_upvalue(ctx_raw: i64, idx_raw: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    let idx = idx_raw as u32;

    if ctx.upvalues_ptr.is_null() || idx >= ctx.upvalue_count {
        return BAILOUT_SENTINEL;
    }

    // SAFETY: We verified the index is in bounds and the pointer is non-null.
    // The upvalue cells are alive because they're held by the caller's stack
    // (passed as a slice to try_execute_jit).
    let cell = unsafe { &*ctx.upvalues_ptr.add(idx as usize) };
    cell.get().to_jit_bits()
}

/// Runtime helper: SetUpvalue — write a value to a captured variable's upvalue cell.
///
/// Signature: `(ctx: i64, idx: i64, value: i64) -> i64`
///
/// Indexes into the upvalue cells array and sets the new value.
/// Returns 0 on success, BAILOUT_SENTINEL on failure.
///
/// # Safety
///
/// Same safety requirements as `otter_rt_get_upvalue`.
#[allow(unsafe_code)]
extern "C" fn otter_rt_set_upvalue(ctx_raw: i64, idx_raw: i64, value_raw: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    let idx = idx_raw as u32;

    if ctx.upvalues_ptr.is_null() || idx >= ctx.upvalue_count {
        return BAILOUT_SENTINEL;
    }

    // Reconstruct the Value from NaN-boxed bits.
    // SAFETY: We're in a no-GC JIT scope; pointer values are still alive.
    let value = match unsafe { crate::value::Value::from_raw_bits_unchecked(value_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };

    let cell = unsafe { &*ctx.upvalues_ptr.add(idx as usize) };
    cell.set(value);
    0
}

// ---------------------------------------------------------------------------
// Helper: extract a JsObject reference from NaN-boxed bits
// ---------------------------------------------------------------------------
#[allow(unsafe_code)]
unsafe fn extract_js_object(bits: u64) -> Option<&'static JsObject> {
    if (bits & TAG_MASK) != TAG_POINTER {
        return None;
    }
    let raw_ptr = (bits & PAYLOAD_MASK) as *const u8;
    if raw_ptr.is_null() {
        return None;
    }
    let header_offset = std::mem::offset_of!(GcAllocation<JsObject>, value);
    let header_ptr = raw_ptr.sub(header_offset) as *const GcHeader;
    if (*header_ptr).tag() != gc_tags::OBJECT {
        return None;
    }
    Some(&*(raw_ptr as *const JsObject))
}

// ---------------------------------------------------------------------------
// Helper: reconstruct arguments from argv pointer
// ---------------------------------------------------------------------------
#[allow(unsafe_code)]
unsafe fn collect_args(argc: usize, argv_ptr: i64) -> Option<Vec<crate::value::Value>> {
    let mut args = Vec::with_capacity(argc);
    if argc > 0 {
        let argv = argv_ptr as *const i64;
        for i in 0..argc {
            let bits = *argv.add(i) as u64;
            let arg = crate::value::Value::from_raw_bits_unchecked(bits)?;
            args.push(arg);
        }
    }
    Some(args)
}

// ---------------------------------------------------------------------------
// Helper: simplified value_to_property_key (no ToPrimitive for objects)
// ---------------------------------------------------------------------------
fn value_to_property_key_simple(value: &crate::value::Value) -> Option<PropertyKey> {
    if let Some(sym) = value.as_symbol() {
        return Some(PropertyKey::Symbol(sym));
    }
    if let Some(n) = value.as_int32() {
        if n >= 0 {
            return Some(PropertyKey::Index(n as u32));
        }
    }
    if let Some(s) = value.as_string() {
        // Check if it's an array index
        let str_val = s.as_str();
        if let Ok(idx) = str_val.parse::<u32>() {
            if idx.to_string() == str_val {
                return Some(PropertyKey::Index(idx));
            }
        }
        return Some(PropertyKey::from_js_string(s));
    }
    if let Some(n) = value.as_number() {
        // Numeric keys like 1.5 become string "1.5"
        let s = crate::globals::js_number_to_string(n);
        if let Ok(idx) = s.parse::<u32>() {
            if idx.to_string() == s {
                return Some(PropertyKey::Index(idx));
            }
        }
        return Some(PropertyKey::string(&s));
    }
    None // bail for objects/complex types that need ToPrimitive
}

// ---------------------------------------------------------------------------
// LoadThis
// ---------------------------------------------------------------------------

/// Runtime helper: LoadThis — read the `this` value.
///
/// Signature: `(ctx: i64) -> i64`
///
/// Returns the pre-computed `this` value from JitContext (snapshotted at JIT entry).
#[allow(unsafe_code)]
extern "C" fn otter_rt_load_this(ctx_raw: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    ctx.this_raw
}

// ---------------------------------------------------------------------------
// TypeOf / TypeOfName
// ---------------------------------------------------------------------------

/// Runtime helper: TypeOf — `typeof val`.
///
/// Signature: `(ctx: i64, val: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_typeof(_ctx_raw: i64, val_raw: i64) -> i64 {
    let value = match unsafe { crate::value::Value::from_raw_bits_unchecked(val_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };
    let type_name = value.type_of();
    crate::value::Value::string(JsString::intern(type_name)).to_jit_bits()
}

/// Runtime helper: TypeOfName — `typeof globalName` (no ReferenceError).
///
/// Signature: `(ctx: i64, name_idx: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_typeof_name(ctx_raw: i64, name_idx: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.vm_ctx.is_null() || ctx.constants.is_null() {
        return BAILOUT_SENTINEL;
    }
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    let Some(name_str) = (unsafe { resolve_constant_string(ctx, name_idx) }) else {
        return BAILOUT_SENTINEL;
    };
    let type_name = match vm_ctx.get_global_utf16(name_str) {
        Some(value) => value.type_of(),
        None => "undefined", // avoids ReferenceError
    };
    crate::value::Value::string(JsString::intern(type_name)).to_jit_bits()
}

// ---------------------------------------------------------------------------
// Pow
// ---------------------------------------------------------------------------

/// Runtime helper: Pow — `lhs ** rhs` (numeric only, bail on BigInt).
///
/// Signature: `(ctx: i64, lhs: i64, rhs: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_pow(_ctx_raw: i64, lhs_raw: i64, rhs_raw: i64) -> i64 {
    let lhs = match unsafe { crate::value::Value::from_raw_bits_unchecked(lhs_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };
    let rhs = match unsafe { crate::value::Value::from_raw_bits_unchecked(rhs_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };
    let left = if let Some(n) = lhs.as_number() {
        n
    } else if let Some(i) = lhs.as_int32() {
        i as f64
    } else {
        return BAILOUT_SENTINEL;
    };
    let right = if let Some(n) = rhs.as_number() {
        n
    } else if let Some(i) = rhs.as_int32() {
        i as f64
    } else {
        return BAILOUT_SENTINEL;
    };
    crate::value::Value::number(left.powf(right)).to_jit_bits()
}

// ---------------------------------------------------------------------------
// CloseUpvalue
// ---------------------------------------------------------------------------

/// Runtime helper: CloseUpvalue — close an upvalue cell for a local variable.
///
/// Signature: `(ctx: i64, local_idx: i64) -> i64`
///
/// Note: This only works when the JIT function has an interpreter frame context
/// (e.g., when called from re-entrant execution). For top-level JIT, this bails.
#[allow(unsafe_code)]
extern "C" fn otter_rt_close_upvalue(ctx_raw: i64, local_idx: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    match vm_ctx.close_upvalue(local_idx as u16) {
        Ok(()) => 0,
        Err(_) => BAILOUT_SENTINEL,
    }
}

// ---------------------------------------------------------------------------
// GetElem / SetElem
// ---------------------------------------------------------------------------

/// Runtime helper: GetElem — element access with IC fast path.
///
/// Signature: `(ctx: i64, obj: i64, idx: i64, ic_idx: i64) -> i64`
///
/// Fast paths: array integer index, IC shape match.
/// Bails on: proxy, string indexing, prototype chain walk.
#[allow(unsafe_code)]
extern "C" fn otter_rt_get_elem(ctx_raw: i64, obj_raw: i64, idx_raw: i64, ic_idx: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    let obj_bits = obj_raw as u64;
    let idx_val = match unsafe { crate::value::Value::from_raw_bits_unchecked(idx_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };

    // Only handle heap objects
    let Some(obj_ref) = (unsafe { extract_js_object(obj_bits) }) else {
        return BAILOUT_SENTINEL;
    };

    // Fast path: array with integer index
    if obj_ref.is_array() {
        if let Some(n) = idx_val.as_int32() {
            if n >= 0 {
                let elements = obj_ref.get_elements_storage().borrow();
                if (n as usize) < elements.len() {
                    return elements[n as usize].to_jit_bits();
                }
            }
        }
    }

    // IC fast path for string keys
    let key = match value_to_property_key_simple(&idx_val) {
        Some(k) => k,
        None => return BAILOUT_SENTINEL,
    };

    if obj_ref.is_dictionary_mode() {
        return BAILOUT_SENTINEL;
    }

    if matches!(&key, PropertyKey::String(_)) {
        let obj_shape_ptr = std::sync::Arc::as_ptr(&obj_ref.shape()) as u64;
        let function = unsafe { &*ctx.function_ptr };
        let feedback = function.feedback_vector.write();
        if let Some(ic) = feedback.get_mut(ic_idx as usize) {
            if ic.proto_epoch_matches(ctx.proto_epoch) {
                let cached_offset = match &ic.ic_state {
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
                        return val.to_jit_bits();
                    }
                }
            }
        }
    }

    // For integer indices on non-array objects (e.g., arguments, typed arrays),
    // try direct property lookup
    if let PropertyKey::Index(idx) = &key {
        if let Some(val) = obj_ref.get(&PropertyKey::Index(*idx)) {
            return val.to_jit_bits();
        }
    }

    BAILOUT_SENTINEL
}

/// Runtime helper: SetElem — element write with IC fast path.
///
/// Signature: `(ctx: i64, obj: i64, idx: i64, value: i64, ic_idx: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_set_elem(
    ctx_raw: i64,
    obj_raw: i64,
    idx_raw: i64,
    value_raw: i64,
    ic_idx: i64,
) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    let obj_bits = obj_raw as u64;
    let idx_val = match unsafe { crate::value::Value::from_raw_bits_unchecked(idx_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };
    let write_val = match unsafe { crate::value::Value::from_raw_bits_unchecked(value_raw as u64) }
    {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };

    // Only handle heap objects
    let Some(obj_ref) = (unsafe { extract_js_object(obj_bits) }) else {
        return BAILOUT_SENTINEL;
    };

    // Fast path: array with integer index
    if obj_ref.is_array() {
        if let Some(n) = idx_val.as_int32() {
            if n >= 0 {
                let mut elements = obj_ref.get_elements_storage().borrow_mut();
                let idx = n as usize;
                if idx < elements.len() {
                    elements[idx] = write_val;
                    return 0;
                } else if idx == elements.len() {
                    elements.push(write_val);
                    // Update length property
                    let length_key = PropertyKey::string("length");
                    if let Some(len_offset) = obj_ref.shape().get_offset(&length_key) {
                        let _ = obj_ref.set_by_offset(
                            len_offset,
                            crate::value::Value::number((idx + 1) as f64),
                        );
                    }
                    return 0;
                }
            }
        }
    }

    // IC fast path for string keys (same as GetPropConst but for write)
    let key = match value_to_property_key_simple(&idx_val) {
        Some(k) => k,
        None => return BAILOUT_SENTINEL,
    };

    if obj_ref.is_dictionary_mode() {
        return BAILOUT_SENTINEL;
    }

    if matches!(&key, PropertyKey::String(_)) {
        let obj_shape_ptr = std::sync::Arc::as_ptr(&obj_ref.shape()) as u64;
        let function = unsafe { &*ctx.function_ptr };
        let feedback = function.feedback_vector.write();
        if let Some(ic) = feedback.get_mut(ic_idx as usize) {
            if ic.proto_epoch_matches(ctx.proto_epoch) {
                let cached_offset = match &ic.ic_state {
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
                    if obj_ref
                        .set_by_offset(offset as usize, write_val.clone())
                        .is_ok()
                    {
                        ic.record_hit();
                        return 0;
                    }
                }
            }
        }
    }

    // For integer indices, try direct set on the object
    if let PropertyKey::Index(idx) = &key {
        if obj_ref
            .set(PropertyKey::Index(*idx), write_val.clone())
            .is_ok()
        {
            return 0;
        }
    }

    BAILOUT_SENTINEL
}

// ---------------------------------------------------------------------------
// GetProp / SetProp (dynamic key)
// ---------------------------------------------------------------------------

/// Runtime helper: GetProp — dynamic property read.
///
/// Signature: `(ctx: i64, obj: i64, key: i64, ic_idx: i64) -> i64`
///
/// Like GetPropConst but the key is a runtime value (not a constant index).
#[allow(unsafe_code)]
extern "C" fn otter_rt_get_prop(ctx_raw: i64, obj_raw: i64, key_raw: i64, ic_idx: i64) -> i64 {
    // Delegate to GetElem — same operation, different opcode name
    otter_rt_get_elem(ctx_raw, obj_raw, key_raw, ic_idx)
}

/// Runtime helper: SetProp — dynamic property write.
///
/// Signature: `(ctx: i64, obj: i64, key: i64, value: i64, ic_idx: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_set_prop(
    ctx_raw: i64,
    obj_raw: i64,
    key_raw: i64,
    value_raw: i64,
    ic_idx: i64,
) -> i64 {
    // Delegate to SetElem — same operation
    otter_rt_set_elem(ctx_raw, obj_raw, key_raw, value_raw, ic_idx)
}

// ---------------------------------------------------------------------------
// DeleteProp
// ---------------------------------------------------------------------------

/// Runtime helper: DeleteProp — property deletion.
///
/// Signature: `(ctx: i64, obj: i64, key: i64) -> i64`
///
/// Returns boolean as NaN-boxed bits. Bails on proxy/strict mode errors.
#[allow(unsafe_code)]
extern "C" fn otter_rt_delete_prop(_ctx_raw: i64, obj_raw: i64, key_raw: i64) -> i64 {
    let obj_val = match unsafe { crate::value::Value::from_raw_bits_unchecked(obj_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };
    let key_val = match unsafe { crate::value::Value::from_raw_bits_unchecked(key_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };

    // Bail on null/undefined/proxy
    if obj_val.is_null() || obj_val.is_undefined() || obj_val.as_proxy().is_some() {
        return BAILOUT_SENTINEL;
    }

    let Some(obj_ref) = obj_val.as_object() else {
        // Non-object: delete always succeeds
        return crate::value::Value::boolean(true).to_jit_bits();
    };

    let key = match value_to_property_key_simple(&key_val) {
        Some(k) => k,
        None => return BAILOUT_SENTINEL,
    };

    if !obj_ref.has_own(&key) {
        return crate::value::Value::boolean(true).to_jit_bits();
    }

    let result = obj_ref.delete(&key);
    // Bail on strict mode delete failure (need frame context for strict check)
    if !result {
        return BAILOUT_SENTINEL; // let interpreter handle strict mode error
    }
    crate::value::Value::boolean(result).to_jit_bits()
}

// ---------------------------------------------------------------------------
// DefineProperty
// ---------------------------------------------------------------------------

/// Runtime helper: DefineProperty — define a property on an object.
///
/// Signature: `(ctx: i64, obj: i64, key: i64, val: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_define_property(
    _ctx_raw: i64,
    obj_raw: i64,
    key_raw: i64,
    val_raw: i64,
) -> i64 {
    let obj_val = match unsafe { crate::value::Value::from_raw_bits_unchecked(obj_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };
    let key_val = match unsafe { crate::value::Value::from_raw_bits_unchecked(key_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };
    let value = match unsafe { crate::value::Value::from_raw_bits_unchecked(val_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };

    let Some(obj_ref) = obj_val.as_object() else {
        return BAILOUT_SENTINEL;
    };

    let key = match value_to_property_key_simple(&key_val) {
        Some(k) => k,
        None => return BAILOUT_SENTINEL,
    };

    obj_ref.define_property(key, PropertyDescriptor::data(value));
    0
}

// ---------------------------------------------------------------------------
// Throw
// ---------------------------------------------------------------------------

/// Runtime helper: ThrowValue — bail out to let interpreter handle the throw.
///
/// Signature: `(ctx: i64, val: i64) -> i64`
///
/// JIT can't handle exceptions directly (no try/catch support yet).
/// Returns BAILOUT_SENTINEL so the interpreter re-executes and throws properly.
#[allow(unsafe_code)]
extern "C" fn otter_rt_throw_value(_ctx_raw: i64, _val_raw: i64) -> i64 {
    BAILOUT_SENTINEL
}

// ---------------------------------------------------------------------------
// Construct
// ---------------------------------------------------------------------------

/// Runtime helper: Construct — `new Ctor(args)`.
///
/// Signature: `(ctx: i64, callee: i64, argc: i64, argv_ptr: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_construct(
    ctx_raw: i64,
    callee_raw: i64,
    argc_raw: i64,
    argv_ptr_raw: i64,
) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.interpreter.is_null() || ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }

    let callee = match unsafe { crate::value::Value::from_raw_bits_unchecked(callee_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };

    // Bail on proxy constructors
    if callee.as_proxy().is_some() {
        return BAILOUT_SENTINEL;
    }

    let argc = argc_raw as usize;
    let args = match unsafe { collect_args(argc, argv_ptr_raw) } {
        Some(a) => a,
        None => return BAILOUT_SENTINEL,
    };

    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    match interpreter.call_function_construct(
        vm_ctx,
        &callee,
        crate::value::Value::undefined(),
        &args,
    ) {
        Ok(result) => result.to_jit_bits(),
        Err(_) => BAILOUT_SENTINEL,
    }
}

// ---------------------------------------------------------------------------
// CallMethod
// ---------------------------------------------------------------------------

/// Runtime helper: CallMethod — `obj.method(args)`.
///
/// Signature: `(ctx: i64, obj: i64, method_name_idx: i64, argc: i64, argv_ptr: i64, ic_idx: i64) -> i64`
///
/// Resolves the method via IC fast path (like GetPropConst), then calls via
/// interpreter.call_function with obj as receiver.
#[allow(unsafe_code)]
extern "C" fn otter_rt_call_method(
    ctx_raw: i64,
    obj_raw: i64,
    method_name_idx: i64,
    argc_raw: i64,
    argv_ptr_raw: i64,
    ic_idx: i64,
) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.interpreter.is_null() || ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }

    let receiver = match unsafe { crate::value::Value::from_raw_bits_unchecked(obj_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };

    // Try IC fast path to get method
    let obj_ref = match receiver.as_object() {
        Some(o) => o,
        None => return BAILOUT_SENTINEL,
    };

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

    let cached_offset = match &mut ic.ic_state {
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
                    // MRU reordering
                    if i > 0 {
                        entries.swap(0, i);
                    }
                    break;
                }
            }
            found
        }
        _ => None,
    };

    let method = match cached_offset {
        Some(offset) => match obj_ref.get_by_offset(offset as usize) {
            Some(val) => {
                ic.record_hit();
                val
            }
            None => return BAILOUT_SENTINEL,
        },
        None => return BAILOUT_SENTINEL,
    };
    // Must drop feedback borrow before calling interpreter
    drop(feedback);

    let argc = argc_raw as usize;
    let args = match unsafe { collect_args(argc, argv_ptr_raw) } {
        Some(a) => a,
        None => return BAILOUT_SENTINEL,
    };

    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    match interpreter.call_function(vm_ctx, &method, receiver, &args) {
        Ok(result) => result.to_jit_bits(),
        Err(_) => BAILOUT_SENTINEL,
    }
}

// ---------------------------------------------------------------------------
// CallWithReceiver
// ---------------------------------------------------------------------------

/// Runtime helper: CallWithReceiver — `func.call(thisVal, args)`.
///
/// Signature: `(ctx: i64, callee: i64, this_val: i64, argc: i64, argv_ptr: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_call_with_receiver(
    ctx_raw: i64,
    callee_raw: i64,
    this_raw: i64,
    argc_raw: i64,
    argv_ptr_raw: i64,
) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.interpreter.is_null() || ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }

    let callee = match unsafe { crate::value::Value::from_raw_bits_unchecked(callee_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };
    let this_val = match unsafe { crate::value::Value::from_raw_bits_unchecked(this_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };

    let argc = argc_raw as usize;
    let args = match unsafe { collect_args(argc, argv_ptr_raw) } {
        Some(a) => a,
        None => return BAILOUT_SENTINEL,
    };

    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    match interpreter.call_function(vm_ctx, &callee, this_val, &args) {
        Ok(result) => result.to_jit_bits(),
        Err(_) => BAILOUT_SENTINEL,
    }
}

// ---------------------------------------------------------------------------
// CallMethodComputed
// ---------------------------------------------------------------------------

/// Runtime helper: CallMethodComputed — `obj[key](args)`.
///
/// Signature: `(ctx: i64, obj: i64, key: i64, argc: i64, argv_ptr: i64, ic_idx: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_call_method_computed(
    ctx_raw: i64,
    obj_raw: i64,
    key_raw: i64,
    argc_raw: i64,
    argv_ptr_raw: i64,
    _ic_idx: i64,
) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.interpreter.is_null() || ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }

    let receiver = match unsafe { crate::value::Value::from_raw_bits_unchecked(obj_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };
    let key_val = match unsafe { crate::value::Value::from_raw_bits_unchecked(key_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };

    // Bail on proxy
    if receiver.as_proxy().is_some() {
        return BAILOUT_SENTINEL;
    }

    let Some(obj_ref) = receiver.as_object() else {
        return BAILOUT_SENTINEL;
    };

    // Resolve property key
    let key = match value_to_property_key_simple(&key_val) {
        Some(k) => k,
        None => return BAILOUT_SENTINEL,
    };

    // Get the method
    let method = match obj_ref.get(&key) {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };

    let argc = argc_raw as usize;
    let args = match unsafe { collect_args(argc, argv_ptr_raw) } {
        Some(a) => a,
        None => return BAILOUT_SENTINEL,
    };

    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    match interpreter.call_function(vm_ctx, &method, receiver, &args) {
        Ok(result) => result.to_jit_bits(),
        Err(_) => BAILOUT_SENTINEL,
    }
}

// ---------------------------------------------------------------------------
// ToNumber / ToString / RequireCoercible
// ---------------------------------------------------------------------------

/// Runtime helper: ToNumber — convert value to number.
///
/// Signature: `(ctx: i64, val: i64) -> i64`
///
/// Handles numeric types directly, bails on objects (need ToPrimitive).
#[allow(unsafe_code)]
extern "C" fn otter_rt_to_number(ctx_raw: i64, val_raw: i64) -> i64 {
    let value = match unsafe { crate::value::Value::from_raw_bits_unchecked(val_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };

    // Fast paths for common types
    if value.is_int32() || value.is_number() {
        return value.to_jit_bits(); // already a number
    }
    if value.is_undefined() {
        return crate::value::Value::number(f64::NAN).to_jit_bits();
    }
    if value.is_null() {
        return crate::value::Value::number(0.0).to_jit_bits();
    }
    if let Some(b) = value.as_boolean() {
        return crate::value::Value::number(if b { 1.0 } else { 0.0 }).to_jit_bits();
    }

    // For objects and strings, use interpreter's to_number_value
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.interpreter.is_null() || ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }
    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    match interpreter.to_number_value(vm_ctx, &value) {
        Ok(n) => crate::value::Value::number(n).to_jit_bits(),
        Err(_) => BAILOUT_SENTINEL,
    }
}

/// Runtime helper: ToString — convert value to string.
///
/// Signature: `(ctx: i64, val: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_to_string(ctx_raw: i64, val_raw: i64) -> i64 {
    let value = match unsafe { crate::value::Value::from_raw_bits_unchecked(val_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };

    // Fast path: already a string
    if value.as_string().is_some() {
        return value.to_jit_bits();
    }

    // For all other types, use interpreter's to_string_value
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.interpreter.is_null() || ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }
    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    match interpreter.to_string_value(vm_ctx, &value) {
        Ok(s) => crate::value::Value::string(JsString::intern(&s)).to_jit_bits(),
        Err(_) => BAILOUT_SENTINEL,
    }
}

/// Runtime helper: RequireCoercible — throws if null or undefined.
///
/// Signature: `(ctx: i64, val: i64) -> i64`
///
/// Returns 0 on success, BAILOUT_SENTINEL if null/undefined.
#[allow(unsafe_code)]
extern "C" fn otter_rt_require_coercible(_ctx_raw: i64, val_raw: i64) -> i64 {
    let value = match unsafe { crate::value::Value::from_raw_bits_unchecked(val_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };
    if value.is_null() || value.is_undefined() {
        return BAILOUT_SENTINEL; // interpreter will throw TypeError
    }
    0
}

// ---------------------------------------------------------------------------
// InstanceOf
// ---------------------------------------------------------------------------

/// Runtime helper: InstanceOf — `lhs instanceof rhs`.
///
/// Signature: `(ctx: i64, lhs: i64, rhs: i64, ic_idx: i64) -> i64`
///
/// Implements OrdinaryHasInstance: walks left's prototype chain looking for
/// right.prototype. Bails on Symbol.hasInstance (rare).
#[allow(unsafe_code)]
extern "C" fn otter_rt_instanceof(_ctx_raw: i64, lhs_raw: i64, rhs_raw: i64, _ic_idx: i64) -> i64 {
    let lhs = match unsafe { crate::value::Value::from_raw_bits_unchecked(lhs_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };
    let rhs = match unsafe { crate::value::Value::from_raw_bits_unchecked(rhs_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };

    // Bail on proxy
    if rhs.as_proxy().is_some() {
        return BAILOUT_SENTINEL;
    }

    let Some(right_obj) = rhs.as_object() else {
        return BAILOUT_SENTINEL; // interpreter will throw TypeError
    };

    // Get right.prototype
    let proto_val = match right_obj.get(&PropertyKey::string("prototype")) {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };
    let Some(proto) = proto_val.as_object() else {
        return BAILOUT_SENTINEL; // prototype is not an object
    };

    // lhs must be an object for prototype chain walk
    let Some(left_obj) = lhs.as_object() else {
        return crate::value::Value::boolean(false).to_jit_bits();
    };

    // Walk the prototype chain — compare by raw pointer identity
    let proto_ptr = proto.as_ptr() as u64;
    let mut current = left_obj;
    for _ in 0..1000 {
        // depth limit
        let current_proto = current.prototype();
        if current_proto.is_null() || current_proto.is_undefined() {
            return crate::value::Value::boolean(false).to_jit_bits();
        }
        if let Some(proto_obj) = current_proto.as_object() {
            if proto_obj.as_ptr() as u64 == proto_ptr {
                return crate::value::Value::boolean(true).to_jit_bits();
            }
            current = proto_obj;
        } else {
            return crate::value::Value::boolean(false).to_jit_bits();
        }
    }
    BAILOUT_SENTINEL // prototype chain too deep
}

// ---------------------------------------------------------------------------
// In
// ---------------------------------------------------------------------------

/// Runtime helper: In — `key in obj`.
///
/// Signature: `(ctx: i64, lhs: i64, rhs: i64, ic_idx: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_in(_ctx_raw: i64, lhs_raw: i64, rhs_raw: i64, _ic_idx: i64) -> i64 {
    let lhs = match unsafe { crate::value::Value::from_raw_bits_unchecked(lhs_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };
    let rhs = match unsafe { crate::value::Value::from_raw_bits_unchecked(rhs_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };

    // Bail on proxy
    if rhs.as_proxy().is_some() {
        return BAILOUT_SENTINEL;
    }

    let Some(right_obj) = rhs.as_object() else {
        return BAILOUT_SENTINEL; // interpreter will throw TypeError
    };

    let key = match value_to_property_key_simple(&lhs) {
        Some(k) => k,
        None => return BAILOUT_SENTINEL,
    };

    // Check own property first
    if right_obj.has_own(&key) {
        return crate::value::Value::boolean(true).to_jit_bits();
    }

    // Walk prototype chain
    let mut current = right_obj;
    for _ in 0..1000 {
        let proto = current.prototype();
        if proto.is_null() || proto.is_undefined() {
            return crate::value::Value::boolean(false).to_jit_bits();
        }
        if let Some(proto_obj) = proto.as_object() {
            if proto_obj.has_own(&key) {
                return crate::value::Value::boolean(true).to_jit_bits();
            }
            current = proto_obj;
        } else {
            return crate::value::Value::boolean(false).to_jit_bits();
        }
    }
    BAILOUT_SENTINEL // prototype chain too deep
}

// ---------------------------------------------------------------------------
// DeclareGlobalVar
// ---------------------------------------------------------------------------

/// Runtime helper: DeclareGlobalVar — declare a global variable binding.
///
/// Signature: `(ctx: i64, name_idx: i64, configurable: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_declare_global_var(ctx_raw: i64, name_idx: i64, configurable: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.vm_ctx.is_null() || ctx.constants.is_null() {
        return BAILOUT_SENTINEL;
    }
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    let Some(name_str) = (unsafe { resolve_constant_string(ctx, name_idx) }) else {
        return BAILOUT_SENTINEL;
    };

    let key = PropertyKey::from_js_string(JsString::intern_utf16(name_str));
    let name_owned = String::from_utf16_lossy(name_str);
    vm_ctx.add_global_var_name(name_owned);

    let global = vm_ctx.global();
    if !global.has_own(&key) {
        use crate::object::PropertyAttributes;
        global.define_property(
            key,
            PropertyDescriptor::data_with_attrs(
                crate::value::Value::undefined(),
                PropertyAttributes {
                    writable: true,
                    enumerable: true,
                    configurable: configurable != 0,
                },
            ),
        );
    }
    0
}

// ---------------------------------------------------------------------------
// DefineGetter
// ---------------------------------------------------------------------------

/// Runtime helper: DefineGetter — define a getter accessor on an object.
///
/// Signature: `(ctx: i64, obj: i64, key: i64, func: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_define_getter(
    _ctx_raw: i64,
    obj_raw: i64,
    key_raw: i64,
    func_raw: i64,
) -> i64 {
    let obj = unsafe { extract_js_object(obj_raw as u64) };
    let Some(obj_ref) = obj else {
        return BAILOUT_SENTINEL;
    };

    let key_val = unsafe { crate::value::Value::from_raw_bits_unchecked(key_raw as u64) };
    let Some(key_val) = key_val else {
        return BAILOUT_SENTINEL;
    };
    let Some(prop_key) = value_to_property_key_simple(&key_val) else {
        return BAILOUT_SENTINEL;
    };

    let getter_val = unsafe { crate::value::Value::from_raw_bits_unchecked(func_raw as u64) };
    let Some(getter_val) = getter_val else {
        return BAILOUT_SENTINEL;
    };

    // Check for existing accessor with setter
    let existing_setter =
        obj_ref
            .get_own_property_descriptor(&prop_key)
            .and_then(|desc| match desc {
                PropertyDescriptor::Accessor { set, .. } => set,
                _ => None,
            });

    let desc = PropertyDescriptor::Accessor {
        get: Some(getter_val),
        set: existing_setter,
        attributes: crate::object::PropertyAttributes::accessor(),
    };
    obj_ref.define_property(prop_key, desc);
    0
}

// ---------------------------------------------------------------------------
// DefineSetter
// ---------------------------------------------------------------------------

/// Runtime helper: DefineSetter — define a setter accessor on an object.
///
/// Signature: `(ctx: i64, obj: i64, key: i64, func: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_define_setter(
    _ctx_raw: i64,
    obj_raw: i64,
    key_raw: i64,
    func_raw: i64,
) -> i64 {
    let obj = unsafe { extract_js_object(obj_raw as u64) };
    let Some(obj_ref) = obj else {
        return BAILOUT_SENTINEL;
    };

    let key_val = unsafe { crate::value::Value::from_raw_bits_unchecked(key_raw as u64) };
    let Some(key_val) = key_val else {
        return BAILOUT_SENTINEL;
    };
    let Some(prop_key) = value_to_property_key_simple(&key_val) else {
        return BAILOUT_SENTINEL;
    };

    let setter_val = unsafe { crate::value::Value::from_raw_bits_unchecked(func_raw as u64) };
    let Some(setter_val) = setter_val else {
        return BAILOUT_SENTINEL;
    };

    // Check for existing accessor with getter
    let existing_getter =
        obj_ref
            .get_own_property_descriptor(&prop_key)
            .and_then(|desc| match desc {
                PropertyDescriptor::Accessor { get, .. } => get,
                _ => None,
            });

    let desc = PropertyDescriptor::Accessor {
        get: existing_getter,
        set: Some(setter_val),
        attributes: crate::object::PropertyAttributes::accessor(),
    };
    obj_ref.define_property(prop_key, desc);
    0
}

// ---------------------------------------------------------------------------
// DefineMethod
// ---------------------------------------------------------------------------

/// Runtime helper: DefineMethod — define a method (non-enumerable) on an object.
///
/// Signature: `(ctx: i64, obj: i64, key: i64, val: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_define_method(
    _ctx_raw: i64,
    obj_raw: i64,
    key_raw: i64,
    val_raw: i64,
) -> i64 {
    let obj = unsafe { extract_js_object(obj_raw as u64) };
    let Some(obj_ref) = obj else {
        return BAILOUT_SENTINEL;
    };

    let key_val = unsafe { crate::value::Value::from_raw_bits_unchecked(key_raw as u64) };
    let Some(key_val) = key_val else {
        return BAILOUT_SENTINEL;
    };
    let Some(prop_key) = value_to_property_key_simple(&key_val) else {
        return BAILOUT_SENTINEL;
    };

    let value = unsafe { crate::value::Value::from_raw_bits_unchecked(val_raw as u64) };
    let Some(value) = value else {
        return BAILOUT_SENTINEL;
    };

    obj_ref.define_property(prop_key, PropertyDescriptor::builtin_method(value));
    0
}

// ---------------------------------------------------------------------------
// SpreadArray
// ---------------------------------------------------------------------------

/// Runtime helper: Spread — copy elements from src array into dst array.
///
/// Signature: `(ctx: i64, dst_arr: i64, src_arr: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_spread_array(_ctx_raw: i64, dst_raw: i64, src_raw: i64) -> i64 {
    let dst_obj = unsafe { extract_js_object(dst_raw as u64) };
    let src_obj = unsafe { extract_js_object(src_raw as u64) };
    let (Some(dst_ref), Some(src_ref)) = (dst_obj, src_obj) else {
        return BAILOUT_SENTINEL;
    };

    let dst_len = dst_ref
        .get(&PropertyKey::string("length"))
        .and_then(|v| v.as_int32())
        .unwrap_or(0) as u32;

    let src_len = src_ref
        .get(&PropertyKey::string("length"))
        .and_then(|v| v.as_int32())
        .unwrap_or(0) as u32;

    for i in 0..src_len {
        let elem = src_ref
            .get(&PropertyKey::Index(i))
            .unwrap_or_else(crate::value::Value::undefined);
        let _ = dst_ref.set(PropertyKey::Index(dst_len + i), elem);
    }
    // Update length
    let _ = dst_ref.set(
        PropertyKey::string("length"),
        crate::value::Value::int32((dst_len + src_len) as i32),
    );
    0
}

// ---------------------------------------------------------------------------
// ClosureCreate
// ---------------------------------------------------------------------------

/// Runtime helper: ClosureCreate — create a function closure from a function index.
///
/// Signature: `(ctx: i64, func_idx: i64) -> i64`
///
/// Needs interpreter frame locals for upvalue capture. Always bails out.
#[allow(unsafe_code)]
extern "C" fn otter_rt_closure_create(_ctx_raw: i64, _func_idx: i64) -> i64 {
    BAILOUT_SENTINEL
}

// ---------------------------------------------------------------------------
// CreateArguments
// ---------------------------------------------------------------------------

/// Runtime helper: CreateArguments — create the arguments object.
///
/// Signature: `(ctx: i64) -> i64`
///
/// Needs interpreter frame info (argc, parameter mapping). Always bails out.
#[allow(unsafe_code)]
extern "C" fn otter_rt_create_arguments(_ctx_raw: i64) -> i64 {
    BAILOUT_SENTINEL
}

// ---------------------------------------------------------------------------
// GetIterator
// ---------------------------------------------------------------------------

/// Runtime helper: GetIterator — call obj[Symbol.iterator]().
///
/// Signature: `(ctx: i64, src: i64) -> i64`
///
/// Gets the Symbol.iterator method from the object and calls it.
/// Returns the iterator result object as NaN-boxed bits.
#[allow(unsafe_code)]
extern "C" fn otter_rt_get_iterator(ctx_raw: i64, src_raw: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.vm_ctx.is_null() || ctx.interpreter.is_null() {
        return BAILOUT_SENTINEL;
    }

    let src_val = unsafe { crate::value::Value::from_raw_bits_unchecked(src_raw as u64) };
    let Some(src_val) = src_val else {
        return BAILOUT_SENTINEL;
    };

    // Get Symbol.iterator
    let iterator_sym = crate::intrinsics::well_known::iterator_symbol();
    let iterator_method = if let Some(obj) = src_val.as_object() {
        obj.get(&PropertyKey::Symbol(iterator_sym))
    } else {
        // For strings, need String.prototype[Symbol.iterator]
        // Bail out for non-object/non-string types
        return BAILOUT_SENTINEL;
    };

    let Some(iter_fn) = iterator_method else {
        return BAILOUT_SENTINEL;
    };

    // Call the iterator method with obj as `this`
    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    match interpreter.call_function(vm_ctx, &iter_fn, src_val, &[]) {
        Ok(result) => result.to_jit_bits(),
        Err(_) => BAILOUT_SENTINEL,
    }
}

// ---------------------------------------------------------------------------
// IteratorNext
// ---------------------------------------------------------------------------

/// Runtime helper: IteratorNext — call iterator.next() and extract value/done.
///
/// Signature: `(ctx: i64, iter: i64) -> i64`
///
/// Calls iterator.next(), extracts `value` and `done` from the result.
/// Returns `value` as NaN-boxed bits. Writes `done` (as boolean NaN-boxed)
/// to `ctx.secondary_result` for the translator to read.
#[allow(unsafe_code)]
extern "C" fn otter_rt_iterator_next(ctx_raw: i64, iter_raw: i64) -> i64 {
    let ctx = unsafe { &mut *(ctx_raw as *mut JitContext) };
    if ctx.interpreter.is_null() || ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }

    let iter_val = unsafe { crate::value::Value::from_raw_bits_unchecked(iter_raw as u64) };
    let Some(iter_val) = iter_val else {
        return BAILOUT_SENTINEL;
    };

    // Get the iterator object
    let Some(iter_obj) = iter_val.as_object() else {
        return BAILOUT_SENTINEL;
    };

    // Get the "next" method
    let Some(next_fn) = iter_obj.get(&PropertyKey::string("next")) else {
        return BAILOUT_SENTINEL;
    };

    if !next_fn.is_callable() {
        return BAILOUT_SENTINEL;
    }

    // Call next()
    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    let result = match interpreter.call_function(vm_ctx, &next_fn, iter_val, &[]) {
        Ok(r) => r,
        Err(_) => return BAILOUT_SENTINEL,
    };

    // Extract .done and .value from the result object
    let result_obj = match result.as_object() {
        Some(o) => o,
        None => {
            // Non-object result: value=result, done=false
            ctx.secondary_result = crate::value::Value::boolean(false).to_jit_bits();
            return result.to_jit_bits();
        }
    };

    let done = result_obj
        .get(&PropertyKey::string("done"))
        .map(|v| v.to_boolean())
        .unwrap_or(false);
    let value = result_obj
        .get(&PropertyKey::string("value"))
        .unwrap_or_else(crate::value::Value::undefined);

    // Write done to secondary_result
    ctx.secondary_result = crate::value::Value::boolean(done).to_jit_bits();
    value.to_jit_bits()
}

// ---------------------------------------------------------------------------
// IteratorClose
// ---------------------------------------------------------------------------

/// Runtime helper: IteratorClose — call iterator.return() if it exists.
///
/// Signature: `(ctx: i64, iter: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_iterator_close(ctx_raw: i64, iter_raw: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.vm_ctx.is_null() || ctx.interpreter.is_null() {
        return BAILOUT_SENTINEL;
    }

    let iter_val = unsafe { crate::value::Value::from_raw_bits_unchecked(iter_raw as u64) };
    let Some(iter_val) = iter_val else {
        return BAILOUT_SENTINEL;
    };

    // Get return method
    let return_method = if let Some(obj) = iter_val.as_object() {
        obj.get(&PropertyKey::string("return"))
            .unwrap_or(crate::value::Value::undefined())
    } else {
        return BAILOUT_SENTINEL;
    };

    // If return is undefined or null, normal completion
    if return_method.is_undefined() || return_method.is_null() {
        return 0;
    }

    // Must be callable
    if !return_method.is_callable() {
        return BAILOUT_SENTINEL;
    }

    // Call return method
    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    match interpreter.call_function(vm_ctx, &return_method, iter_val, &[]) {
        Ok(_) => 0,
        Err(_) => BAILOUT_SENTINEL,
    }
}

// ---------------------------------------------------------------------------
// CallSpread
// ---------------------------------------------------------------------------

/// Runtime helper: CallSpread — call function with spread arguments.
///
/// Signature: `(ctx: i64, callee: i64, argc: i64, argv_ptr: i64, spread: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_call_spread(
    ctx_raw: i64,
    callee_raw: i64,
    argc: i64,
    argv_ptr: i64,
    spread_raw: i64,
) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.vm_ctx.is_null() || ctx.interpreter.is_null() {
        return BAILOUT_SENTINEL;
    }

    let callee = unsafe { crate::value::Value::from_raw_bits_unchecked(callee_raw as u64) };
    let Some(callee) = callee else {
        return BAILOUT_SENTINEL;
    };

    // Collect regular arguments
    let Some(mut args) = (unsafe { collect_args(argc as usize, argv_ptr) }) else {
        return BAILOUT_SENTINEL;
    };

    // Spread the array into args
    let spread_val = unsafe { crate::value::Value::from_raw_bits_unchecked(spread_raw as u64) };
    let Some(spread_val) = spread_val else {
        return BAILOUT_SENTINEL;
    };

    if let Some(arr_obj) = spread_val.as_object() {
        let len = arr_obj
            .get(&PropertyKey::string("length"))
            .and_then(|v| v.as_int32())
            .unwrap_or(0) as u32;
        for i in 0..len {
            if let Some(elem) = arr_obj.get(&PropertyKey::Index(i)) {
                args.push(elem);
            } else {
                args.push(crate::value::Value::undefined());
            }
        }
    }

    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    match interpreter.call_function(vm_ctx, &callee, crate::value::Value::undefined(), &args) {
        Ok(result) => result.to_jit_bits(),
        Err(_) => BAILOUT_SENTINEL,
    }
}

// ---------------------------------------------------------------------------
// ConstructSpread
// ---------------------------------------------------------------------------

/// Runtime helper: ConstructSpread — construct with spread arguments.
///
/// Signature: `(ctx: i64, callee: i64, argc: i64, argv_ptr: i64, spread: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_construct_spread(
    ctx_raw: i64,
    callee_raw: i64,
    argc: i64,
    argv_ptr: i64,
    spread_raw: i64,
) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.vm_ctx.is_null() || ctx.interpreter.is_null() {
        return BAILOUT_SENTINEL;
    }

    let callee = unsafe { crate::value::Value::from_raw_bits_unchecked(callee_raw as u64) };
    let Some(callee) = callee else {
        return BAILOUT_SENTINEL;
    };

    // Collect regular arguments
    let Some(mut args) = (unsafe { collect_args(argc as usize, argv_ptr) }) else {
        return BAILOUT_SENTINEL;
    };

    // Spread the array
    let spread_val = unsafe { crate::value::Value::from_raw_bits_unchecked(spread_raw as u64) };
    let Some(spread_val) = spread_val else {
        return BAILOUT_SENTINEL;
    };

    if let Some(arr_obj) = spread_val.as_object() {
        let len = arr_obj
            .get(&PropertyKey::string("length"))
            .and_then(|v| v.as_int32())
            .unwrap_or(0) as u32;
        for i in 0..len {
            if let Some(elem) = arr_obj.get(&PropertyKey::Index(i)) {
                args.push(elem);
            } else {
                args.push(crate::value::Value::undefined());
            }
        }
    }

    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    match interpreter.call_function_construct(
        vm_ctx,
        &callee,
        crate::value::Value::undefined(),
        &args,
    ) {
        Ok(result) => result.to_jit_bits(),
        Err(_) => BAILOUT_SENTINEL,
    }
}

// ---------------------------------------------------------------------------
// CallMethodComputedSpread
// ---------------------------------------------------------------------------

/// Runtime helper: CallMethodComputedSpread — call obj[key](...spread).
///
/// Signature: `(ctx: i64, obj: i64, key: i64, spread: i64, ic_idx: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_call_method_computed_spread(
    ctx_raw: i64,
    obj_raw: i64,
    key_raw: i64,
    spread_raw: i64,
    _ic_idx: i64,
) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.vm_ctx.is_null() || ctx.interpreter.is_null() {
        return BAILOUT_SENTINEL;
    }

    let receiver = unsafe { crate::value::Value::from_raw_bits_unchecked(obj_raw as u64) };
    let Some(receiver) = receiver else {
        return BAILOUT_SENTINEL;
    };

    let key_val = unsafe { crate::value::Value::from_raw_bits_unchecked(key_raw as u64) };
    let Some(key_val) = key_val else {
        return BAILOUT_SENTINEL;
    };

    // Resolve method
    let prop_key = value_to_property_key_simple(&key_val);
    let Some(prop_key) = prop_key else {
        return BAILOUT_SENTINEL;
    };

    let method = if let Some(obj) = receiver.as_object() {
        obj.get(&prop_key)
    } else {
        return BAILOUT_SENTINEL;
    };

    let Some(method) = method else {
        return BAILOUT_SENTINEL;
    };

    // Spread the array into args
    let spread_val = unsafe { crate::value::Value::from_raw_bits_unchecked(spread_raw as u64) };
    let Some(spread_val) = spread_val else {
        return BAILOUT_SENTINEL;
    };

    let mut args = Vec::new();
    if let Some(arr_obj) = spread_val.as_object() {
        let len = arr_obj
            .get(&PropertyKey::string("length"))
            .and_then(|v| v.as_int32())
            .unwrap_or(0) as u32;
        for i in 0..len {
            if let Some(elem) = arr_obj.get(&PropertyKey::Index(i)) {
                args.push(elem);
            } else {
                args.push(crate::value::Value::undefined());
            }
        }
    }

    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    match interpreter.call_function(vm_ctx, &method, receiver, &args) {
        Ok(result) => result.to_jit_bits(),
        Err(_) => BAILOUT_SENTINEL,
    }
}

// ---------------------------------------------------------------------------
// TailCall
// ---------------------------------------------------------------------------

/// Runtime helper: TailCall — perform a tail call to a function.
///
/// Signature: `(ctx: i64, callee: i64, argc: i64, argv_ptr: i64) -> i64`
///
/// Tail calls in JIT just delegate to regular call_function since the JIT
/// doesn't maintain its own call frames that could be reused.
#[allow(unsafe_code)]
extern "C" fn otter_rt_tail_call(ctx_raw: i64, callee_raw: i64, argc: i64, argv_ptr: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.vm_ctx.is_null() || ctx.interpreter.is_null() {
        return BAILOUT_SENTINEL;
    }

    let callee = unsafe { crate::value::Value::from_raw_bits_unchecked(callee_raw as u64) };
    let Some(callee) = callee else {
        return BAILOUT_SENTINEL;
    };

    let Some(args) = (unsafe { collect_args(argc as usize, argv_ptr) }) else {
        return BAILOUT_SENTINEL;
    };

    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    match interpreter.call_function(vm_ctx, &callee, crate::value::Value::undefined(), &args) {
        Ok(result) => result.to_jit_bits(),
        Err(_) => BAILOUT_SENTINEL,
    }
}

// ===========================================================================
// Real implementations for class, iterator, and eval opcodes
// ===========================================================================

// ---------------------------------------------------------------------------
// GetSuper — return home_object.__proto__
// ---------------------------------------------------------------------------

/// Runtime helper: GetSuper — get the super prototype.
///
/// Signature: `(ctx: i64) -> i64`
///
/// Reads `home_object` from JitContext and returns its prototype.
#[allow(unsafe_code)]
extern "C" fn otter_rt_get_super(ctx_raw: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };

    let ho_val =
        unsafe { crate::value::Value::from_raw_bits_unchecked(ctx.home_object_raw as u64) };
    let Some(ho_val) = ho_val else {
        return BAILOUT_SENTINEL;
    };
    let Some(ho_obj) = ho_val.as_object() else {
        return BAILOUT_SENTINEL;
    };

    let proto = ho_obj.prototype();
    if proto.is_null() || proto.is_undefined() {
        return crate::value::Value::null().to_jit_bits();
    }
    proto.to_jit_bits()
}

// ---------------------------------------------------------------------------
// SetHomeObject — set [[HomeObject]] on a closure
// ---------------------------------------------------------------------------

/// Runtime helper: SetHomeObject — clone a closure with home_object set.
///
/// Signature: `(ctx: i64, func: i64, obj: i64) -> i64`
///
/// Creates a new Closure identical to `func` but with `home_object = obj`.
/// Returns the new function value.
#[allow(unsafe_code)]
extern "C" fn otter_rt_set_home_object(_ctx_raw: i64, func_raw: i64, obj_raw: i64) -> i64 {
    let func_val = unsafe { crate::value::Value::from_raw_bits_unchecked(func_raw as u64) };
    let Some(func_val) = func_val else {
        return BAILOUT_SENTINEL;
    };
    let obj_val = unsafe { crate::value::Value::from_raw_bits_unchecked(obj_raw as u64) };
    let Some(obj_val) = obj_val else {
        return BAILOUT_SENTINEL;
    };

    let Some(closure) = func_val.as_function() else {
        return BAILOUT_SENTINEL;
    };
    let Some(obj_ref) = obj_val.as_object() else {
        return BAILOUT_SENTINEL;
    };

    // Create new closure with home_object set
    let new_closure = GcRef::new(crate::value::Closure {
        function_index: closure.function_index,
        module: closure.module.clone(),
        upvalues: closure.upvalues.clone(),
        is_async: closure.is_async,
        is_generator: closure.is_generator,
        object: closure.object,
        home_object: Some(obj_ref),
    });

    crate::value::Value::function(new_closure).to_jit_bits()
}

// ---------------------------------------------------------------------------
// GetSuperProp — read property from home_object.__proto__
// ---------------------------------------------------------------------------

/// Runtime helper: GetSuperProp — read a named property from super prototype.
///
/// Signature: `(ctx: i64, name_idx: i64) -> i64`
///
/// Reads home_object from JitContext, gets its prototype, and looks up
/// the named property. Returns the value or BAILOUT_SENTINEL.
#[allow(unsafe_code)]
extern "C" fn otter_rt_get_super_prop(ctx_raw: i64, name_idx: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };

    if ctx.constants.is_null() {
        return BAILOUT_SENTINEL;
    }

    let ho_val =
        unsafe { crate::value::Value::from_raw_bits_unchecked(ctx.home_object_raw as u64) };
    let Some(ho_val) = ho_val else {
        return BAILOUT_SENTINEL;
    };
    let Some(ho_obj) = ho_val.as_object() else {
        return BAILOUT_SENTINEL;
    };

    // Get the prototype
    let proto_val = ho_obj.prototype();
    let Some(proto_obj) = proto_val.as_object() else {
        return BAILOUT_SENTINEL;
    };

    // Resolve property name
    let Some(name_str) = (unsafe { resolve_constant_string(ctx, name_idx) }) else {
        return BAILOUT_SENTINEL;
    };
    let key = PropertyKey::from_js_string(JsString::intern_utf16(name_str));

    // Look up property on the prototype (including prototype chain)
    match proto_obj.get(&key) {
        Some(val) => {
            // Check if it's an accessor with getter — need to call with correct this
            if let Some(desc) = proto_obj.lookup_property_descriptor(&key) {
                if let PropertyDescriptor::Accessor {
                    get: Some(getter), ..
                } = desc
                {
                    // Call getter with `this` from JitContext
                    if ctx.interpreter.is_null() || ctx.vm_ctx.is_null() {
                        return BAILOUT_SENTINEL;
                    }
                    let this_val = unsafe {
                        crate::value::Value::from_raw_bits_unchecked(ctx.this_raw as u64)
                    };
                    let Some(this_val) = this_val else {
                        return BAILOUT_SENTINEL;
                    };
                    let interpreter = unsafe { &*ctx.interpreter };
                    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
                    match interpreter.call_function(vm_ctx, &getter, this_val, &[]) {
                        Ok(result) => return result.to_jit_bits(),
                        Err(_) => return BAILOUT_SENTINEL,
                    }
                }
            }
            val.to_jit_bits()
        }
        None => crate::value::Value::undefined().to_jit_bits(),
    }
}

// ---------------------------------------------------------------------------
// DefineClass — set up prototype chain for a class
// ---------------------------------------------------------------------------

/// Runtime helper: DefineClass — create class prototype chain.
///
/// Signature: `(ctx: i64, ctor: i64, super_class: i64, name_idx: i64) -> i64`
///
/// For derived classes: validates superclass, creates derived prototype,
/// sets up prototype chain and static inheritance.
/// For base classes (super_class == undefined): ensures ctor.prototype.constructor = ctor.
/// Returns the constructor.
#[allow(unsafe_code)]
extern "C" fn otter_rt_define_class(
    ctx_raw: i64,
    ctor_raw: i64,
    super_raw: i64,
    _name_idx: i64,
) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }

    let ctor_val = unsafe { crate::value::Value::from_raw_bits_unchecked(ctor_raw as u64) };
    let Some(ctor_val) = ctor_val else {
        return BAILOUT_SENTINEL;
    };
    let super_val = unsafe { crate::value::Value::from_raw_bits_unchecked(super_raw as u64) };
    let Some(super_val) = super_val else {
        return BAILOUT_SENTINEL;
    };

    let Some(ctor_closure) = ctor_val.as_function() else {
        return BAILOUT_SENTINEL;
    };
    let ctor_obj = &ctor_closure.object;

    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    if super_val.is_undefined() || super_val.is_null() {
        // Base class: ensure ctor.prototype.constructor = ctor
        if let Some(proto_val) = ctor_obj.get(&PropertyKey::string("prototype")) {
            if let Some(proto_obj) = proto_val.as_object() {
                let _ = proto_obj.set(PropertyKey::string("constructor"), ctor_val.clone());
            }
        }
        return ctor_val.to_jit_bits();
    }

    // Derived class
    let Some(super_obj) = super_val
        .as_object()
        .or_else(|| super_val.as_function().map(|c| c.object))
    else {
        return BAILOUT_SENTINEL;
    };

    // Get super.prototype
    let super_proto = super_obj
        .get(&PropertyKey::string("prototype"))
        .unwrap_or_else(crate::value::Value::null);

    let super_proto_obj = super_proto.as_object();

    // Create derived prototype with super.prototype as __proto__
    let derived_proto = GcRef::new(JsObject::new(
        super_proto_obj
            .map(crate::value::Value::object)
            .unwrap_or_else(crate::value::Value::null),
        vm_ctx.memory_manager().clone(),
    ));

    // Set ctor.prototype = derived_proto
    let _ = ctor_obj.set(
        PropertyKey::string("prototype"),
        crate::value::Value::object(derived_proto.clone()),
    );

    // Set derived_proto.constructor = ctor
    let _ = derived_proto.set(PropertyKey::string("constructor"), ctor_val.clone());

    // Set ctor.__proto__ = super for static method inheritance
    ctor_obj.set_prototype(super_val.clone());

    ctor_val.to_jit_bits()
}

// ---------------------------------------------------------------------------
// CallSuper — call super constructor
// ---------------------------------------------------------------------------

/// Runtime helper: CallSuper — call the super constructor.
///
/// Signature: `(ctx: i64, argc: i64, argv_ptr: i64) -> i64`
///
/// Finds the super constructor from callee.__proto__, creates a new instance,
/// and calls the super constructor. Returns the constructed `this`.
#[allow(unsafe_code)]
extern "C" fn otter_rt_call_super(ctx_raw: i64, argc_raw: i64, argv_ptr_raw: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.interpreter.is_null() || ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }

    // Get callee (the constructor being called)
    let callee = unsafe { crate::value::Value::from_raw_bits_unchecked(ctx.callee_raw as u64) };
    let Some(callee) = callee else {
        return BAILOUT_SENTINEL;
    };

    // Find super constructor: Object.getPrototypeOf(callee)
    let super_ctor = if let Some(closure) = callee.as_function() {
        let proto = closure.object.prototype();
        if proto.is_null() || proto.is_undefined() {
            return BAILOUT_SENTINEL;
        }
        proto
    } else if let Some(obj) = callee.as_object() {
        let proto = obj.prototype();
        if proto.is_null() || proto.is_undefined() {
            return BAILOUT_SENTINEL;
        }
        proto
    } else {
        return BAILOUT_SENTINEL;
    };

    let argc = argc_raw as usize;
    let args = match unsafe { collect_args(argc, argv_ptr_raw) } {
        Some(a) => a,
        None => return BAILOUT_SENTINEL,
    };

    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    // Call super constructor
    match interpreter.call_function_construct(
        vm_ctx,
        &super_ctor,
        crate::value::Value::undefined(),
        &args,
    ) {
        Ok(result) => result.to_jit_bits(),
        Err(_) => BAILOUT_SENTINEL,
    }
}

// ---------------------------------------------------------------------------
// CallSuperSpread — call super with spread arguments
// ---------------------------------------------------------------------------

/// Runtime helper: CallSuperSpread — call super constructor with a spread array.
///
/// Signature: `(ctx: i64, args_array: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_call_super_spread(ctx_raw: i64, args_array_raw: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.interpreter.is_null() || ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }

    // Get callee
    let callee = unsafe { crate::value::Value::from_raw_bits_unchecked(ctx.callee_raw as u64) };
    let Some(callee) = callee else {
        return BAILOUT_SENTINEL;
    };

    // Find super constructor
    let super_ctor = if let Some(closure) = callee.as_function() {
        let proto = closure.object.prototype();
        if proto.is_null() || proto.is_undefined() {
            return BAILOUT_SENTINEL;
        }
        proto
    } else {
        return BAILOUT_SENTINEL;
    };

    // Spread the array into args
    let args_val = unsafe { crate::value::Value::from_raw_bits_unchecked(args_array_raw as u64) };
    let Some(args_val) = args_val else {
        return BAILOUT_SENTINEL;
    };

    let mut args = Vec::new();
    if let Some(arr_obj) = args_val.as_object() {
        let len = arr_obj
            .get(&PropertyKey::string("length"))
            .and_then(|v| v.as_int32())
            .unwrap_or(0) as u32;
        for i in 0..len {
            args.push(
                arr_obj
                    .get(&PropertyKey::Index(i))
                    .unwrap_or_else(crate::value::Value::undefined),
            );
        }
    }

    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    match interpreter.call_function_construct(
        vm_ctx,
        &super_ctor,
        crate::value::Value::undefined(),
        &args,
    ) {
        Ok(result) => result.to_jit_bits(),
        Err(_) => BAILOUT_SENTINEL,
    }
}

// ---------------------------------------------------------------------------
// GetAsyncIterator — call obj[Symbol.asyncIterator]() or fallback to Symbol.iterator
// ---------------------------------------------------------------------------

/// Runtime helper: GetAsyncIterator — get async iterator.
///
/// Signature: `(ctx: i64, src: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_get_async_iterator(ctx_raw: i64, src_raw: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.vm_ctx.is_null() || ctx.interpreter.is_null() {
        return BAILOUT_SENTINEL;
    }

    let src_val = unsafe { crate::value::Value::from_raw_bits_unchecked(src_raw as u64) };
    let Some(src_val) = src_val else {
        return BAILOUT_SENTINEL;
    };

    let Some(obj) = src_val.as_object() else {
        return BAILOUT_SENTINEL;
    };

    // Try Symbol.asyncIterator first
    let async_iter_sym = crate::intrinsics::well_known::async_iterator_symbol();
    let iter_method = obj
        .get(&PropertyKey::Symbol(async_iter_sym))
        .filter(|v| !v.is_undefined() && !v.is_null());

    // Fall back to Symbol.iterator
    let iter_method = match iter_method {
        Some(m) => m,
        None => {
            let iter_sym = crate::intrinsics::well_known::iterator_symbol();
            match obj.get(&PropertyKey::Symbol(iter_sym)) {
                Some(m) if !m.is_undefined() && !m.is_null() => m,
                _ => return BAILOUT_SENTINEL,
            }
        }
    };

    if !iter_method.is_callable() {
        return BAILOUT_SENTINEL;
    }

    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    match interpreter.call_function(vm_ctx, &iter_method, src_val, &[]) {
        Ok(result) => result.to_jit_bits(),
        Err(_) => BAILOUT_SENTINEL,
    }
}

// ---------------------------------------------------------------------------
// CallEval — evaluate code string
// ---------------------------------------------------------------------------

/// Runtime helper: CallEval — direct eval().
///
/// Signature: `(ctx: i64, code_val: i64) -> i64`
///
/// If the argument is not a string, returns it unchanged.
/// Otherwise, compiles and executes the string as eval code.
#[allow(unsafe_code)]
extern "C" fn otter_rt_call_eval(ctx_raw: i64, code_raw: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.interpreter.is_null() || ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }

    let code_val = unsafe { crate::value::Value::from_raw_bits_unchecked(code_raw as u64) };
    let Some(code_val) = code_val else {
        return BAILOUT_SENTINEL;
    };

    // If argument is not a string, return it unchanged (spec §19.2.1.1)
    let Some(code_str) = code_val.as_string() else {
        return code_val.to_jit_bits();
    };

    let source = code_str.as_str().to_string();

    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    // Determine strict mode from the function flags
    let is_strict = unsafe { &*ctx.function_ptr }.flags.is_strict;

    // Compile eval code
    let eval_module = match vm_ctx.compile_eval(&source, is_strict) {
        Ok(m) => m,
        Err(_) => return BAILOUT_SENTINEL,
    };

    // Execute eval module
    match interpreter.execute_eval_module(vm_ctx, &eval_module) {
        Ok(result) => result.to_jit_bits(),
        Err(_) => BAILOUT_SENTINEL,
    }
}

// ---------------------------------------------------------------------------
// Runtime host hooks opcodes (Import / Export / ForInNext)
// ---------------------------------------------------------------------------

/// Runtime helper: Import — delegate to runtime host hooks.
///
/// Signature: `(ctx: i64, module_name_idx: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_import(ctx_raw: i64, module_name_idx: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }
    let Ok(module_idx) = u32::try_from(module_name_idx) else {
        return BAILOUT_SENTINEL;
    };
    if ctx.constants.is_null() {
        return BAILOUT_SENTINEL;
    }
    let constants = unsafe { &*ctx.constants };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    match vm_ctx.host_import_from_constant_pool(constants, module_idx) {
        Ok(value) => value.to_jit_bits(),
        Err(_) => BAILOUT_SENTINEL,
    }
}

/// Runtime helper: Export — delegate to runtime host hooks.
///
/// Signature: `(ctx: i64, export_name_idx: i64, value: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_export(ctx_raw: i64, export_name_idx: i64, value_raw: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }
    let Ok(export_idx) = u32::try_from(export_name_idx) else {
        return BAILOUT_SENTINEL;
    };
    if ctx.constants.is_null() {
        return BAILOUT_SENTINEL;
    }
    let constants = unsafe { &*ctx.constants };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    let Some(value) = (unsafe { crate::value::Value::from_raw_bits_unchecked(value_raw as u64) })
    else {
        return BAILOUT_SENTINEL;
    };
    match vm_ctx.host_export_from_constant_pool(constants, export_idx, value) {
        Ok(()) => 0,
        Err(_) => BAILOUT_SENTINEL,
    }
}

/// Runtime helper: ForInNext — delegate to runtime host hooks.
///
/// Signature: `(ctx: i64, target: i64) -> i64`
///
/// Returns the next key, or `undefined` when iteration is complete.
#[allow(unsafe_code)]
extern "C" fn otter_rt_for_in_next(ctx_raw: i64, target_raw: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    let Some(target) = (unsafe { crate::value::Value::from_raw_bits_unchecked(target_raw as u64) })
    else {
        return BAILOUT_SENTINEL;
    };
    match vm_ctx.host_for_in_next(target) {
        Ok(Some(value)) => value.to_jit_bits(),
        Ok(None) => crate::value::Value::undefined().to_jit_bits(),
        Err(_) => BAILOUT_SENTINEL,
    }
}

// ---------------------------------------------------------------------------
// Bail-out stubs for truly unsupported opcodes
// ---------------------------------------------------------------------------
// TryStart — push try handler onto try_stack
// ---------------------------------------------------------------------------

/// Runtime helper: TryStart — push a try handler for exception handling.
///
/// Signature: `(ctx: i64, catch_pc: i64) -> i64`
///
/// Pushes a TryHandler with the given catch_pc onto the VmContext try_stack.
/// The catch_pc is the absolute instruction index computed by the translator.
/// Returns 0 (no meaningful return value).
#[allow(unsafe_code)]
extern "C" fn otter_rt_try_start(ctx_raw: i64, catch_pc: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    vm_ctx.push_try(catch_pc as usize);
    0
}

// ---------------------------------------------------------------------------
// TryEnd — pop try handler from try_stack
// ---------------------------------------------------------------------------

/// Runtime helper: TryEnd — pop the current frame's try handler.
///
/// Signature: `(ctx: i64) -> i64`
///
/// Pops the most recent try handler if it belongs to the current frame.
/// Returns 0 (no meaningful return value).
#[allow(unsafe_code)]
extern "C" fn otter_rt_try_end(ctx_raw: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    vm_ctx.pop_try_for_current_frame();
    0
}

// ---------------------------------------------------------------------------
// CatchOp — take pending exception value
// ---------------------------------------------------------------------------

/// Runtime helper: CatchOp — take the pending exception from VmContext.
///
/// Signature: `(ctx: i64) -> i64`
///
/// Takes the pending exception value (if any) and returns it as NaN-boxed bits.
/// Returns undefined if no exception is pending.
#[allow(unsafe_code)]
extern "C" fn otter_rt_catch(ctx_raw: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    let value = vm_ctx
        .take_exception()
        .unwrap_or_else(crate::value::Value::undefined);
    value.to_jit_bits()
}

// ---------------------------------------------------------------------------
// CallSuperForward — forward all arguments to super constructor
// ---------------------------------------------------------------------------

/// Runtime helper: CallSuperForward — default derived constructor arg forwarding.
///
/// Signature: `(ctx: i64) -> i64`
///
/// Reads the current frame's arguments (stored in locals by push_frame),
/// finds the super constructor, and calls it. Returns the constructed `this`.
#[allow(unsafe_code)]
extern "C" fn otter_rt_call_super_forward(ctx_raw: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.interpreter.is_null() || ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }

    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    // Read frame info
    let (home_object, new_target_proto, argc, callee_value) = {
        let frame = match vm_ctx.current_frame() {
            Some(f) => f,
            None => return BAILOUT_SENTINEL,
        };
        let home_object = match frame.home_object.clone() {
            Some(ho) => ho,
            None => return BAILOUT_SENTINEL,
        };
        let new_target_proto = frame
            .new_target_proto
            .clone()
            .unwrap_or_else(|| home_object.clone());
        let argc = frame.argc;
        let callee_value = frame.callee_value.clone();
        (home_object, new_target_proto, argc, callee_value)
    };

    // Collect args from locals
    let mut args = Vec::with_capacity(argc);
    for i in 0..argc {
        match vm_ctx.get_local(i as u16) {
            Ok(v) => args.push(v),
            Err(_) => return BAILOUT_SENTINEL,
        }
    }

    // Get super constructor
    let super_ctor_val = if let Some(callee) = callee_value {
        if let Some(callee_obj) = callee.as_object() {
            callee_obj.prototype()
        } else {
            crate::value::Value::undefined()
        }
    } else {
        let super_proto = match home_object.prototype().as_object() {
            Some(sp) => sp,
            None => return BAILOUT_SENTINEL,
        };
        let ctor_key = PropertyKey::string("constructor");
        super_proto
            .get(&ctor_key)
            .unwrap_or_else(crate::value::Value::undefined)
    };

    // Check if super constructor is also a derived class
    let super_is_derived = super_ctor_val
        .as_function()
        .and_then(|c| {
            c.module
                .function(c.function_index)
                .map(|f| f.flags.is_derived)
        })
        .unwrap_or(false);

    let this_value = if super_is_derived {
        if let Some(super_closure) = super_ctor_val.as_function() {
            vm_ctx.set_pending_is_derived(true);
            vm_ctx.set_pending_new_target_proto(new_target_proto);
            let proto_key = PropertyKey::string("prototype");
            if let Some(proto_val) = super_closure.object.get(&proto_key) {
                if let Some(proto_obj) = proto_val.as_object() {
                    vm_ctx.set_pending_home_object(proto_obj);
                }
            }
        }
        match interpreter.call_function(
            vm_ctx,
            &super_ctor_val,
            crate::value::Value::undefined(),
            &args,
        ) {
            Ok(result) => {
                if result.is_object() {
                    result
                } else {
                    crate::value::Value::undefined()
                }
            }
            Err(_) => return BAILOUT_SENTINEL,
        }
    } else if super_ctor_val.as_native_function().is_some() {
        // Native built-in constructor
        let mm = vm_ctx.memory_manager().clone();
        let new_obj = GcRef::new(JsObject::new(
            crate::value::Value::object(new_target_proto.clone()),
            mm,
        ));
        let new_obj_value = crate::value::Value::object(new_obj);
        match interpreter.call_function_construct(
            vm_ctx,
            &super_ctor_val,
            new_obj_value.clone(),
            &args,
        ) {
            Ok(result) => {
                let this_obj = if result.is_object() {
                    if let Some(obj) = result.as_object() {
                        obj.set_prototype(crate::value::Value::object(new_target_proto));
                    }
                    result
                } else {
                    new_obj_value
                };
                this_obj
            }
            Err(_) => return BAILOUT_SENTINEL,
        }
    } else {
        let mm = vm_ctx.memory_manager().clone();
        let new_obj = GcRef::new(JsObject::new(
            crate::value::Value::object(new_target_proto),
            mm,
        ));
        let new_obj_value = crate::value::Value::object(new_obj);
        match interpreter.call_function(vm_ctx, &super_ctor_val, new_obj_value.clone(), &args) {
            Ok(result) => {
                if result.is_object() {
                    result
                } else {
                    new_obj_value
                }
            }
            Err(_) => return BAILOUT_SENTINEL,
        }
    };

    // Update frame's this_value
    if let Some(frame) = vm_ctx.current_frame_mut() {
        frame.this_value = this_value.clone();
        frame.this_initialized = true;
    }

    // Run field initializers
    if interpreter
        .run_field_initializers(vm_ctx, &this_value)
        .is_err()
    {
        return BAILOUT_SENTINEL;
    }

    this_value.to_jit_bits()
}

// ---------------------------------------------------------------------------
// AsyncClosure — create async function closure
// ---------------------------------------------------------------------------

/// Runtime helper: AsyncClosure — create an async function closure object.
///
/// Signature: `(ctx: i64, func_idx: i64) -> i64`
///
/// Creates a Closure with is_async=true, is_generator=false, capturing upvalues
/// from the current frame. Returns the function value as NaN-boxed bits.
#[allow(unsafe_code)]
extern "C" fn otter_rt_async_closure(ctx_raw: i64, func_idx: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    // Get the module from the current frame
    let module = match vm_ctx.current_frame() {
        Some(frame) => frame.module.clone(),
        None => return BAILOUT_SENTINEL,
    };

    let func_def = match module.function(func_idx as u32) {
        Some(f) => f,
        None => return BAILOUT_SENTINEL,
    };

    // Capture upvalues
    let captured = match capture_upvalues_for_jit(vm_ctx, &func_def.upvalues) {
        Ok(c) => c,
        Err(_) => return BAILOUT_SENTINEL,
    };

    let func_obj = GcRef::new(JsObject::new(
        crate::value::Value::null(),
        vm_ctx.memory_manager().clone(),
    ));

    // Set [[Prototype]] to Function.prototype
    if let Some(fn_proto) = vm_ctx.function_prototype() {
        func_obj.set_prototype(crate::value::Value::object(fn_proto));
    }

    // Set function length and name properties
    let fn_attrs = crate::object::PropertyAttributes {
        writable: false,
        enumerable: false,
        configurable: true,
    };
    func_obj.define_property(
        PropertyKey::string("length"),
        crate::object::PropertyDescriptor::Data {
            value: crate::value::Value::int32(func_def.param_count as i32),
            attributes: fn_attrs,
        },
    );
    let fn_name = func_def.name.as_deref().unwrap_or("");
    func_obj.define_property(
        PropertyKey::string("name"),
        crate::object::PropertyDescriptor::Data {
            value: crate::value::Value::string(JsString::intern(fn_name)),
            attributes: fn_attrs,
        },
    );

    let closure = GcRef::new(crate::value::Closure {
        function_index: func_idx as u32,
        module: std::sync::Arc::clone(&module),
        upvalues: captured,
        is_async: true,
        is_generator: false,
        object: func_obj.clone(),
        home_object: None,
    });
    let func_value = crate::value::Value::function(closure);

    // Async functions are not constructors
    func_obj.define_property(
        PropertyKey::string("__non_constructor"),
        PropertyDescriptor::Data {
            value: crate::value::Value::boolean(true),
            attributes: crate::object::PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: false,
            },
        },
    );

    func_value.to_jit_bits()
}

// ---------------------------------------------------------------------------
// GeneratorClosure — create generator function closure
// ---------------------------------------------------------------------------

/// Runtime helper: GeneratorClosure — create a generator function closure object.
///
/// Signature: `(ctx: i64, func_idx: i64) -> i64`
///
/// Creates a Closure with is_async=false, is_generator=true, with generator
/// prototype chain. Returns the function value as NaN-boxed bits.
#[allow(unsafe_code)]
extern "C" fn otter_rt_generator_closure(ctx_raw: i64, func_idx: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    // Get the module from the current frame
    let module = match vm_ctx.current_frame() {
        Some(frame) => frame.module.clone(),
        None => return BAILOUT_SENTINEL,
    };

    let func_def = match module.function(func_idx as u32) {
        Some(f) => f,
        None => return BAILOUT_SENTINEL,
    };

    // Capture upvalues
    let captured = match capture_upvalues_for_jit(vm_ctx, &func_def.upvalues) {
        Ok(c) => c,
        Err(_) => return BAILOUT_SENTINEL,
    };

    // Get GeneratorFunctionPrototype as the function's [[Prototype]]
    let gen_func_proto = vm_ctx
        .get_global("GeneratorFunctionPrototype")
        .and_then(|v| v.as_object());

    let func_obj = GcRef::new(JsObject::new(
        gen_func_proto
            .map(crate::value::Value::object)
            .unwrap_or_else(crate::value::Value::null),
        vm_ctx.memory_manager().clone(),
    ));
    func_obj.define_property(
        PropertyKey::string("__realm_id__"),
        PropertyDescriptor::builtin_data(crate::value::Value::int32(vm_ctx.realm_id() as i32)),
    );

    // Set function length and name properties
    let fn_attrs = crate::object::PropertyAttributes {
        writable: false,
        enumerable: false,
        configurable: true,
    };
    func_obj.define_property(
        PropertyKey::string("length"),
        crate::object::PropertyDescriptor::Data {
            value: crate::value::Value::int32(func_def.param_count as i32),
            attributes: fn_attrs,
        },
    );
    let fn_name = func_def.name.as_deref().unwrap_or("");
    func_obj.define_property(
        PropertyKey::string("name"),
        crate::object::PropertyDescriptor::Data {
            value: crate::value::Value::string(JsString::intern(fn_name)),
            attributes: fn_attrs,
        },
    );

    // Create the .prototype for generator instances
    let gen_proto = vm_ctx
        .get_global("GeneratorPrototype")
        .and_then(|v| v.as_object());
    let proto = GcRef::new(JsObject::new(
        gen_proto
            .map(crate::value::Value::object)
            .unwrap_or_else(crate::value::Value::null),
        vm_ctx.memory_manager().clone(),
    ));

    let closure = GcRef::new(crate::value::Closure {
        function_index: func_idx as u32,
        module: std::sync::Arc::clone(&module),
        upvalues: captured,
        is_async: false,
        is_generator: true,
        object: func_obj.clone(),
        home_object: None,
    });
    let func_value = crate::value::Value::function(closure);

    // Generator prototype property
    func_obj.define_property(
        PropertyKey::string("prototype"),
        PropertyDescriptor::Data {
            value: crate::value::Value::object(proto.clone()),
            attributes: crate::object::PropertyAttributes {
                writable: true,
                enumerable: false,
                configurable: false,
            },
        },
    );
    let _ = proto.set(PropertyKey::string("constructor"), func_value.clone());
    func_obj.define_property(
        PropertyKey::string("__non_constructor"),
        PropertyDescriptor::Data {
            value: crate::value::Value::boolean(true),
            attributes: crate::object::PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: false,
            },
        },
    );

    func_value.to_jit_bits()
}

// ---------------------------------------------------------------------------
// AsyncGeneratorClosure — create async generator function closure
// ---------------------------------------------------------------------------

/// Runtime helper: AsyncGeneratorClosure — create an async generator closure object.
///
/// Signature: `(ctx: i64, func_idx: i64) -> i64`
///
/// Creates a Closure with is_async=true, is_generator=true, with async generator
/// prototype chain. Returns the function value as NaN-boxed bits.
#[allow(unsafe_code)]
extern "C" fn otter_rt_async_generator_closure(ctx_raw: i64, func_idx: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    // Get the module from the current frame
    let module = match vm_ctx.current_frame() {
        Some(frame) => frame.module.clone(),
        None => return BAILOUT_SENTINEL,
    };

    let func_def = match module.function(func_idx as u32) {
        Some(f) => f,
        None => return BAILOUT_SENTINEL,
    };

    // Capture upvalues
    let captured = match capture_upvalues_for_jit(vm_ctx, &func_def.upvalues) {
        Ok(c) => c,
        Err(_) => return BAILOUT_SENTINEL,
    };

    // Get AsyncGeneratorFunctionPrototype as the function's [[Prototype]]
    let async_gen_func_proto = vm_ctx
        .get_global("AsyncGeneratorFunctionPrototype")
        .and_then(|v| v.as_object());

    let func_obj = GcRef::new(JsObject::new(
        async_gen_func_proto
            .map(crate::value::Value::object)
            .unwrap_or_else(crate::value::Value::null),
        vm_ctx.memory_manager().clone(),
    ));
    func_obj.define_property(
        PropertyKey::string("__realm_id__"),
        PropertyDescriptor::builtin_data(crate::value::Value::int32(vm_ctx.realm_id() as i32)),
    );

    // Set function length and name properties
    let fn_attrs = crate::object::PropertyAttributes {
        writable: false,
        enumerable: false,
        configurable: true,
    };
    func_obj.define_property(
        PropertyKey::string("length"),
        crate::object::PropertyDescriptor::Data {
            value: crate::value::Value::int32(func_def.param_count as i32),
            attributes: fn_attrs,
        },
    );
    let fn_name = func_def.name.as_deref().unwrap_or("");
    func_obj.define_property(
        PropertyKey::string("name"),
        crate::object::PropertyDescriptor::Data {
            value: crate::value::Value::string(JsString::intern(fn_name)),
            attributes: fn_attrs,
        },
    );

    // Create the .prototype for instances
    let gen_proto = vm_ctx
        .get_global("GeneratorPrototype")
        .and_then(|v| v.as_object());
    let proto = GcRef::new(JsObject::new(
        gen_proto
            .map(crate::value::Value::object)
            .unwrap_or_else(crate::value::Value::null),
        vm_ctx.memory_manager().clone(),
    ));

    let closure = GcRef::new(crate::value::Closure {
        function_index: func_idx as u32,
        module: std::sync::Arc::clone(&module),
        upvalues: captured,
        is_async: true,
        is_generator: true,
        object: func_obj.clone(),
        home_object: None,
    });
    let func_value = crate::value::Value::function(closure);

    // Async generator prototype property
    func_obj.define_property(
        PropertyKey::string("prototype"),
        PropertyDescriptor::Data {
            value: crate::value::Value::object(proto.clone()),
            attributes: crate::object::PropertyAttributes {
                writable: true,
                enumerable: false,
                configurable: false,
            },
        },
    );
    let _ = proto.set(PropertyKey::string("constructor"), func_value.clone());
    func_obj.define_property(
        PropertyKey::string("__non_constructor"),
        PropertyDescriptor::Data {
            value: crate::value::Value::boolean(true),
            attributes: crate::object::PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: false,
            },
        },
    );

    func_value.to_jit_bits()
}

/// Capture upvalues for a JIT-created closure.
/// Equivalent to `Interpreter::capture_upvalues` but callable from JIT helpers.
fn capture_upvalues_for_jit(
    vm_ctx: &mut crate::context::VmContext,
    upvalue_specs: &[otter_vm_bytecode::function::UpvalueCapture],
) -> Result<Vec<UpvalueCell>, crate::error::VmError> {
    use otter_vm_bytecode::function::UpvalueCapture;
    let mut captured = Vec::with_capacity(upvalue_specs.len());
    for spec in upvalue_specs {
        let cell = match spec {
            UpvalueCapture::Local(idx) => vm_ctx.get_or_create_open_upvalue(idx.0)?,
            UpvalueCapture::Upvalue(idx) => vm_ctx.get_upvalue_cell(idx.0)?.clone(),
        };
        captured.push(cell);
    }
    Ok(captured)
}

// ---------------------------------------------------------------------------
// Bail-out stubs for opcodes that genuinely cannot work in JIT:
// - Yield/Await: JIT runs on native stack, can't suspend mid-execution
// ---------------------------------------------------------------------------

/// Generic bail-out stub: always returns BAILOUT_SENTINEL.
#[allow(unsafe_code)]
extern "C" fn otter_rt_bailout_stub(_ctx_raw: i64) -> i64 {
    BAILOUT_SENTINEL
}

/// Build a `RuntimeHelpers` table with all available helper functions.
pub fn build_runtime_helpers() -> RuntimeHelpers {
    let mut helpers = RuntimeHelpers::new();
    // SAFETY: Function signatures match HelperKind conventions.
    unsafe {
        helpers.set(
            HelperKind::GetPropConst,
            otter_rt_get_prop_const as *const u8,
        );
        helpers.set(
            HelperKind::SetPropConst,
            otter_rt_set_prop_const as *const u8,
        );
        helpers.set(
            HelperKind::CallFunction,
            otter_rt_call_function as *const u8,
        );
        helpers.set(HelperKind::NewObject, otter_rt_new_object as *const u8);
        helpers.set(HelperKind::NewArray, otter_rt_new_array as *const u8);
        helpers.set(HelperKind::GetGlobal, otter_rt_get_global as *const u8);
        helpers.set(HelperKind::SetGlobal, otter_rt_set_global as *const u8);
        helpers.set(HelperKind::GetUpvalue, otter_rt_get_upvalue as *const u8);
        helpers.set(HelperKind::SetUpvalue, otter_rt_set_upvalue as *const u8);
        helpers.set(HelperKind::LoadThis, otter_rt_load_this as *const u8);
        helpers.set(HelperKind::TypeOf, otter_rt_typeof as *const u8);
        helpers.set(HelperKind::TypeOfName, otter_rt_typeof_name as *const u8);
        helpers.set(HelperKind::Pow, otter_rt_pow as *const u8);
        helpers.set(
            HelperKind::CloseUpvalue,
            otter_rt_close_upvalue as *const u8,
        );
        helpers.set(HelperKind::GetElem, otter_rt_get_elem as *const u8);
        helpers.set(HelperKind::SetElem, otter_rt_set_elem as *const u8);
        helpers.set(HelperKind::GetProp, otter_rt_get_prop as *const u8);
        helpers.set(HelperKind::SetProp, otter_rt_set_prop as *const u8);
        helpers.set(HelperKind::DeleteProp, otter_rt_delete_prop as *const u8);
        helpers.set(
            HelperKind::DefineProperty,
            otter_rt_define_property as *const u8,
        );
        helpers.set(HelperKind::ThrowValue, otter_rt_throw_value as *const u8);
        helpers.set(HelperKind::Construct, otter_rt_construct as *const u8);
        helpers.set(HelperKind::CallMethod, otter_rt_call_method as *const u8);
        helpers.set(
            HelperKind::CallWithReceiver,
            otter_rt_call_with_receiver as *const u8,
        );
        helpers.set(
            HelperKind::CallMethodComputed,
            otter_rt_call_method_computed as *const u8,
        );
        helpers.set(HelperKind::ToNumber, otter_rt_to_number as *const u8);
        helpers.set(HelperKind::JsToString, otter_rt_to_string as *const u8);
        helpers.set(
            HelperKind::RequireCoercible,
            otter_rt_require_coercible as *const u8,
        );
        helpers.set(HelperKind::InstanceOf, otter_rt_instanceof as *const u8);
        helpers.set(HelperKind::InOp, otter_rt_in as *const u8);
        helpers.set(
            HelperKind::DeclareGlobalVar,
            otter_rt_declare_global_var as *const u8,
        );
        helpers.set(
            HelperKind::DefineGetter,
            otter_rt_define_getter as *const u8,
        );
        helpers.set(
            HelperKind::DefineSetter,
            otter_rt_define_setter as *const u8,
        );
        helpers.set(
            HelperKind::DefineMethod,
            otter_rt_define_method as *const u8,
        );
        helpers.set(HelperKind::SpreadArray, otter_rt_spread_array as *const u8);
        helpers.set(
            HelperKind::ClosureCreate,
            otter_rt_closure_create as *const u8,
        );
        helpers.set(
            HelperKind::CreateArguments,
            otter_rt_create_arguments as *const u8,
        );
        helpers.set(HelperKind::GetIterator, otter_rt_get_iterator as *const u8);
        helpers.set(
            HelperKind::IteratorNext,
            otter_rt_iterator_next as *const u8,
        );
        helpers.set(
            HelperKind::IteratorClose,
            otter_rt_iterator_close as *const u8,
        );
        helpers.set(HelperKind::CallSpread, otter_rt_call_spread as *const u8);
        helpers.set(
            HelperKind::ConstructSpread,
            otter_rt_construct_spread as *const u8,
        );
        helpers.set(
            HelperKind::CallMethodComputedSpread,
            otter_rt_call_method_computed_spread as *const u8,
        );
        helpers.set(HelperKind::TailCallHelper, otter_rt_tail_call as *const u8);
        // Real implementations for class/iterator/eval opcodes
        helpers.set(HelperKind::GetSuper, otter_rt_get_super as *const u8);
        helpers.set(
            HelperKind::SetHomeObject,
            otter_rt_set_home_object as *const u8,
        );
        helpers.set(
            HelperKind::GetSuperProp,
            otter_rt_get_super_prop as *const u8,
        );
        helpers.set(HelperKind::DefineClass, otter_rt_define_class as *const u8);
        helpers.set(HelperKind::CallSuper, otter_rt_call_super as *const u8);
        helpers.set(
            HelperKind::CallSuperSpread,
            otter_rt_call_super_spread as *const u8,
        );
        helpers.set(
            HelperKind::GetAsyncIterator,
            otter_rt_get_async_iterator as *const u8,
        );
        helpers.set(HelperKind::CallEval, otter_rt_call_eval as *const u8);
        // Real implementations for try/catch, closure variants, and CallSuperForward
        helpers.set(HelperKind::TryStart, otter_rt_try_start as *const u8);
        helpers.set(HelperKind::TryEnd, otter_rt_try_end as *const u8);
        helpers.set(HelperKind::CatchOp, otter_rt_catch as *const u8);
        helpers.set(
            HelperKind::CallSuperForward,
            otter_rt_call_super_forward as *const u8,
        );
        helpers.set(
            HelperKind::AsyncClosure,
            otter_rt_async_closure as *const u8,
        );
        helpers.set(
            HelperKind::GeneratorClosure,
            otter_rt_generator_closure as *const u8,
        );
        helpers.set(
            HelperKind::AsyncGeneratorClosure,
            otter_rt_async_generator_closure as *const u8,
        );
        helpers.set(HelperKind::ImportOp, otter_rt_import as *const u8);
        helpers.set(HelperKind::ExportOp, otter_rt_export as *const u8);
        helpers.set(HelperKind::ForInNext, otter_rt_for_in_next as *const u8);

        // Bail-out stubs: JIT can't suspend (Yield/Await)
        let stub = otter_rt_bailout_stub as *const u8;
        helpers.set(HelperKind::YieldOp, stub);
        helpers.set(HelperKind::AwaitOp, stub);
    }
    helpers
}
