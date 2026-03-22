//! Runtime helper implementations for JIT-compiled code.
//!
//! These `extern "C"` functions are called from Cranelift-generated machine code
//! to handle operations that need VM context (property access, function calls, etc.).
//!
//! # Safety
//!
//! All helpers receive a `*mut u8` context pointer that is actually a `*const JitContext`.
//! The context is constructed by `try_execute_jit` and is valid for the duration of
//! JIT execution. The historical "helpers don't allocate" assumption is false:
//! some helpers allocate, re-enter interpreter/JIT execution, or cross host
//! boundaries. See `otter_vm_jit::runtime_helpers::HelperKind::safety_class()`
//! and `JIT_HELPER_SAFETY_AUDIT.md` for the current conservative taxonomy.

use otter_vm_bytecode::Function;
use otter_vm_bytecode::function::InlineCacheState;
use otter_vm_gc::object::{GcAllocation, GcHeader, tags as gc_tags};
use otter_vm_jit::BAILOUT_SENTINEL;
use otter_vm_jit::runtime_helpers::{
    HelperKind, JIT_CTX_BAILOUT_PC_OFFSET, JIT_CTX_BAILOUT_REASON_OFFSET,
    JIT_CTX_SECONDARY_RESULT_OFFSET, RuntimeHelpers,
};

use crate::value::Value;
use crate::interpreter::Interpreter;

use crate::gc::GcRef;
use crate::jit_stubs::{
    JitCallReentryState, call_with_reentry_state, otter_rt_call_function_stub,
    otter_rt_call_mono_stub, otter_rt_get_prop_mono_stub,
};
use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::{UpvalueCell, UpvalueData};

// NaN-boxing constants (must match value.rs)
const TAG_MASK: u64 = 0xFFFF_0000_0000_0000;
const PAYLOAD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
const TAG_POINTER: u64 = 0x7FFC_0000_0000_0000;
const TAG_PTR_FUNCTION: u64 = 0x7FFE_0000_0000_0000;
const TAG_INT32: u64 = 0x7FF8_0001_0000_0000;

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
    /// Deopt reason code written by JIT-side bailout paths.
    pub bailout_reason: i64,
    /// Bytecode pc at which JIT-side bailout happened.
    pub bailout_pc: i64,
    /// Pointer to caller-allocated buffer for dumping local values on deopt.
    /// Null when precise resume is not requested.
    pub deopt_locals_ptr: *mut i64,
    /// Number of local slots in the deopt buffer.
    pub deopt_locals_count: u32,
    /// Pointer to caller-allocated buffer for dumping register values on deopt.
    /// Null when precise resume is not requested.
    pub deopt_regs_ptr: *mut i64,
    /// Number of register slots in the deopt buffer.
    pub deopt_regs_count: u32,
    /// OSR entry PC.  `-1` = normal entry (start from PC 0).
    /// `>= 0` = on-stack replacement: load locals/regs from deopt buffers
    /// and jump directly to the loop header at this bytecode PC.
    pub osr_entry_pc: i64,
    /// Tier-up budget for JIT back-edge recompilation.
    /// Decremented at each backward jump in JIT-compiled code.
    /// When it reaches 0, a tier-up check fires: if ICs warmed up since
    /// compilation, the function bails out for recompilation.
    pub tier_up_budget: i64,
    /// Pointer to JIT IC probe table (flat `[JitIcProbe]` array).
    /// JIT code reads from this at runtime for IC sites that were Uninitialized
    /// at compile time. Helpers update these probes as ICs warm up.
    /// Null if no probe table is initialized.
    pub ic_probes_ptr: *const otter_vm_bytecode::function::JitIcProbe,
    /// Number of entries in the IC probe table.
    pub ic_probes_count: u32,
    /// Raw pointer to the interrupt flag (AtomicBool).
    /// JIT code loads from this at backward jumps to check for timeout/cancellation.
    /// When the byte at this address is nonzero, JIT bails out to the interpreter.
    pub interrupt_flag_ptr: *const u8,
}

// Compile-time checks: offsets in runtime_helpers.rs must match JitContext layout.
use otter_vm_jit::runtime_helpers::{
    JIT_CTX_DEOPT_LOCALS_COUNT_OFFSET, JIT_CTX_DEOPT_LOCALS_PTR_OFFSET,
    JIT_CTX_DEOPT_REGS_COUNT_OFFSET, JIT_CTX_DEOPT_REGS_PTR_OFFSET, JIT_CTX_IC_PROBES_COUNT_OFFSET,
    JIT_CTX_IC_PROBES_PTR_OFFSET, JIT_CTX_INTERRUPT_FLAG_PTR_OFFSET, JIT_CTX_OSR_ENTRY_PC_OFFSET,
    JIT_CTX_TIER_UP_BUDGET_OFFSET, JIT_CTX_UPVALUE_COUNT_OFFSET, JIT_CTX_UPVALUES_PTR_OFFSET,
    JIT_UPVALUE_CELL_GCBOX_PTR_OFFSET, JIT_UPVALUE_CELL_SIZE, JIT_UPVALUE_DATA_VALUE_OFFSET,
    JIT_UPVALUE_GCBOX_VALUE_OFFSET,
};
const _: () = {
    assert!(
        std::mem::offset_of!(JitContext, upvalues_ptr) as i32 == JIT_CTX_UPVALUES_PTR_OFFSET,
        "JitContext::upvalues_ptr offset changed — update JIT_CTX_UPVALUES_PTR_OFFSET in runtime_helpers.rs"
    );
    assert!(
        std::mem::offset_of!(JitContext, upvalue_count) as i32 == JIT_CTX_UPVALUE_COUNT_OFFSET,
        "JitContext::upvalue_count offset changed — update JIT_CTX_UPVALUE_COUNT_OFFSET in runtime_helpers.rs"
    );
    assert!(
        std::mem::offset_of!(JitContext, secondary_result) as i32
            == JIT_CTX_SECONDARY_RESULT_OFFSET,
        "JitContext::secondary_result offset changed — update JIT_CTX_SECONDARY_RESULT_OFFSET in runtime_helpers.rs"
    );
    assert!(
        std::mem::offset_of!(JitContext, bailout_reason) as i32 == JIT_CTX_BAILOUT_REASON_OFFSET,
        "JitContext::bailout_reason offset changed — update JIT_CTX_BAILOUT_REASON_OFFSET in runtime_helpers.rs"
    );
    assert!(
        std::mem::offset_of!(JitContext, bailout_pc) as i32 == JIT_CTX_BAILOUT_PC_OFFSET,
        "JitContext::bailout_pc offset changed — update JIT_CTX_BAILOUT_PC_OFFSET in runtime_helpers.rs"
    );
    assert!(
        std::mem::offset_of!(JitContext, deopt_locals_ptr) as i32
            == JIT_CTX_DEOPT_LOCALS_PTR_OFFSET,
        "JitContext::deopt_locals_ptr offset changed — update JIT_CTX_DEOPT_LOCALS_PTR_OFFSET in runtime_helpers.rs"
    );
    assert!(
        std::mem::offset_of!(JitContext, deopt_locals_count) as i32
            == JIT_CTX_DEOPT_LOCALS_COUNT_OFFSET,
        "JitContext::deopt_locals_count offset changed — update JIT_CTX_DEOPT_LOCALS_COUNT_OFFSET in runtime_helpers.rs"
    );
    assert!(
        std::mem::offset_of!(JitContext, deopt_regs_ptr) as i32 == JIT_CTX_DEOPT_REGS_PTR_OFFSET,
        "JitContext::deopt_regs_ptr offset changed — update JIT_CTX_DEOPT_REGS_PTR_OFFSET in runtime_helpers.rs"
    );
    assert!(
        std::mem::offset_of!(JitContext, deopt_regs_count) as i32
            == JIT_CTX_DEOPT_REGS_COUNT_OFFSET,
        "JitContext::deopt_regs_count offset changed — update JIT_CTX_DEOPT_REGS_COUNT_OFFSET in runtime_helpers.rs"
    );
    assert!(
        std::mem::offset_of!(JitContext, osr_entry_pc) as i32 == JIT_CTX_OSR_ENTRY_PC_OFFSET,
        "JitContext::osr_entry_pc offset changed — update JIT_CTX_OSR_ENTRY_PC_OFFSET in runtime_helpers.rs"
    );
    assert!(
        std::mem::offset_of!(JitContext, tier_up_budget) as i32 == JIT_CTX_TIER_UP_BUDGET_OFFSET,
        "JitContext::tier_up_budget offset changed — update JIT_CTX_TIER_UP_BUDGET_OFFSET in runtime_helpers.rs"
    );
    assert!(
        std::mem::offset_of!(JitContext, ic_probes_ptr) as i32 == JIT_CTX_IC_PROBES_PTR_OFFSET,
        "JitContext::ic_probes_ptr offset changed — update JIT_CTX_IC_PROBES_PTR_OFFSET in runtime_helpers.rs"
    );
    assert!(
        std::mem::offset_of!(JitContext, ic_probes_count) as i32 == JIT_CTX_IC_PROBES_COUNT_OFFSET,
        "JitContext::ic_probes_count offset changed — update JIT_CTX_IC_PROBES_COUNT_OFFSET in runtime_helpers.rs"
    );
    assert!(
        std::mem::offset_of!(JitContext, interrupt_flag_ptr) as i32
            == JIT_CTX_INTERRUPT_FLAG_PTR_OFFSET,
        "JitContext::interrupt_flag_ptr offset changed — update JIT_CTX_INTERRUPT_FLAG_PTR_OFFSET in runtime_helpers.rs"
    );
    assert!(
        std::mem::size_of::<otter_vm_bytecode::function::JitIcProbe>() == 40,
        "JitIcProbe size must be 40 bytes for JIT offset computation"
    );
    assert!(
        std::mem::offset_of!(crate::object::JsObject, jit_elements_data) as i32
            == otter_vm_jit::runtime_helpers::JSOBJECT_ELEMENTS_DATA_OFFSET,
        "JsObject::jit_elements_data offset changed"
    );
    assert!(
        std::mem::offset_of!(crate::object::JsObject, jit_elements_len) as i32
            == otter_vm_jit::runtime_helpers::JSOBJECT_ELEMENTS_LEN_OFFSET,
        "JsObject::jit_elements_len offset changed"
    );
    assert!(
        std::mem::offset_of!(crate::object::JsObject, jit_elements_kind) as i32
            == otter_vm_jit::runtime_helpers::JSOBJECT_ELEMENTS_KIND_OFFSET,
        "JsObject::jit_elements_kind offset changed"
    );
    assert!(
        std::mem::size_of::<UpvalueCell>() as i32 == JIT_UPVALUE_CELL_SIZE,
        "UpvalueCell size changed — update JIT_UPVALUE_CELL_SIZE in runtime_helpers.rs"
    );
    assert!(
        std::mem::offset_of!(UpvalueCell, 0) as i32 == JIT_UPVALUE_CELL_GCBOX_PTR_OFFSET,
        "UpvalueCell pointer layout changed — update JIT_UPVALUE_CELL_GCBOX_PTR_OFFSET in runtime_helpers.rs"
    );
    assert!(
        std::mem::offset_of!(crate::gc::GcBox<UpvalueData>, value) as i32
            == JIT_UPVALUE_GCBOX_VALUE_OFFSET,
        "GcBox<UpvalueData>::value offset changed — update JIT_UPVALUE_GCBOX_VALUE_OFFSET in runtime_helpers.rs"
    );
    assert!(
        std::mem::offset_of!(UpvalueData, value) as i32 == JIT_UPVALUE_DATA_VALUE_OFFSET,
        "UpvalueData::value offset changed — update JIT_UPVALUE_DATA_VALUE_OFFSET in runtime_helpers.rs"
    );
    assert!(
        std::mem::size_of::<std::cell::Cell<crate::value::Value>>()
            == std::mem::size_of::<crate::value::Value>(),
        "Cell<Value> layout changed — raw JIT upvalue load assumptions must be revisited"
    );
    assert!(
        std::mem::align_of::<std::cell::Cell<crate::value::Value>>()
            == std::mem::align_of::<crate::value::Value>(),
        "Cell<Value> alignment changed — raw JIT upvalue load assumptions must be revisited"
    );
};

/// UTF-16 encoding of "length" for fast constant pool comparison.
const LENGTH_UTF16: [u16; 6] = [108, 101, 110, 103, 116, 104];
/// UTF-16 encoding of "toString" for hot CallMethod fast path.
const TO_STRING_UTF16: [u16; 8] = [116, 111, 83, 116, 114, 105, 110, 103];

/// Check if the constant at `name_idx` in the module constant pool is "length".
#[allow(unsafe_code)]
fn is_length_constant(ctx: &JitContext, name_idx: i64) -> bool {
    if ctx.constants.is_null() {
        return false;
    }
    let pool = unsafe { &*ctx.constants };
    if let Some(constant) = pool.get(name_idx as u32)
        && let otter_vm_bytecode::Constant::String(units) = constant
    {
        return units.as_slice() == LENGTH_UTF16;
    }
    false
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
///
/// Extract a JsObject reference from NaN-boxed bits (TAG_POINTER with OBJECT/ARRAY tag).
/// Returns None if not a valid heap object.
#[inline]
#[allow(unsafe_code)]
fn extract_js_object_ref(bits: u64) -> Option<&'static JsObject> {
    if (bits & TAG_MASK) != TAG_POINTER {
        return None;
    }
    let raw_ptr = (bits & PAYLOAD_MASK) as *const u8;
    if raw_ptr.is_null() {
        return None;
    }
    let header_offset = std::mem::offset_of!(GcAllocation<JsObject>, value);
    let header_ptr = unsafe { raw_ptr.sub(header_offset) as *const GcHeader };
    let tag = unsafe { (*header_ptr).tag() };
    if tag != gc_tags::OBJECT && tag != gc_tags::ARRAY {
        return None;
    }
    Some(unsafe { &*(raw_ptr as *const JsObject) })
}

/// Monomorphic property read — compile-time shape and offset baked in.
///
/// Skips JitContext, feedback vector, IC lookup, and proto epoch check.
/// Returns property value or BAILOUT_SENTINEL on shape mismatch / accessor.
///
/// Signature: `(obj: i64, expected_shape: i64, offset: i64) -> i64`
#[allow(unsafe_code)]
pub(crate) extern "C" fn otter_rt_get_prop_mono_impl(
    obj_raw: i64,
    expected_shape: i64,
    offset: i64,
) -> i64 {
    let obj_ref = match extract_js_object_ref(obj_raw as u64) {
        Some(o) => o,
        None => return BAILOUT_SENTINEL,
    };

    if unsafe { obj_ref.shape_id_unchecked() } != expected_shape as u64 {
        return BAILOUT_SENTINEL;
    }

    match unsafe { obj_ref.get_by_offset_unchecked(offset as usize) } {
        Some(val) => val.to_jit_bits(),
        None => BAILOUT_SENTINEL,
    }
}

/// Monomorphic property write — compile-time shape and offset baked in.
///
/// Skips JitContext, feedback vector, IC lookup, and proto epoch check.
/// Returns 0 on success or BAILOUT_SENTINEL on shape mismatch / frozen / accessor.
///
/// Signature: `(obj: i64, expected_shape: i64, offset: i64, value: i64) -> i64`
#[allow(unsafe_code)]
pub(crate) extern "C" fn otter_rt_set_prop_mono_impl(
    obj_raw: i64,
    expected_shape: i64,
    offset: i64,
    value_raw: i64,
) -> i64 {
    let obj_ref = match extract_js_object_ref(obj_raw as u64) {
        Some(o) => o,
        None => return BAILOUT_SENTINEL,
    };

    if unsafe { obj_ref.shape_id_unchecked() } != expected_shape as u64 {
        return BAILOUT_SENTINEL;
    }

    // Reconstruct the Value to write.
    let value_bits = value_raw as u64;
    let write_value = if (value_bits & TAG_MASK) == TAG_POINTER {
        match unsafe { crate::value::Value::from_raw_bits_unchecked(value_bits) } {
            Some(v) => v,
            None => return BAILOUT_SENTINEL,
        }
    } else {
        match crate::value::Value::from_jit_bits(value_bits) {
            Some(v) => v,
            None => return BAILOUT_SENTINEL,
        }
    };

    match obj_ref.set_by_offset(offset as usize, write_value) {
        Ok(()) => 0,
        Err(_) => BAILOUT_SENTINEL,
    }
}

/// GC write barrier for heap values stored inline by JIT code.
///
/// Called after JIT code stores a heap-tagged Value directly into
/// inline_slots (bypassing set_by_offset). Runs the generational barrier
/// (remembered set) and incremental barrier (gray marking) as needed.
///
/// The caller (JIT) has already verified the value is a heap pointer
/// (tag >= 0x7FFC). For non-heap values, this is a no-op.
///
/// Signature: `(value_raw: i64) -> i64`
#[allow(unsafe_code)]
pub(crate) extern "C" fn otter_rt_gc_write_barrier_jit(value_raw: i64) -> i64 {
    let value_bits = value_raw as u64;
    // Fast exit for non-heap values (defensive; caller should only call for heap)
    if (value_bits >> 48) < 0x7FFC {
        return 0;
    }
    // Extract raw payload pointer
    let raw_ptr = (value_bits & PAYLOAD_MASK) as *const u8;
    if raw_ptr.is_null() {
        return 0;
    }
    // Navigate from T* back to GcHeader*
    let offset = std::mem::offset_of!(GcAllocation<JsObject>, value);
    let header_ptr = unsafe { raw_ptr.sub(offset) as *const GcHeader };
    // Generational barrier: add to remembered set if value is in nursery
    otter_vm_gc::remembered_set_add_if_young(header_ptr);
    // Incremental barrier: gray the object if marking is in progress
    if otter_vm_gc::global_registry().is_marking() {
        let header = unsafe { &*header_ptr };
        if header.mark() == otter_vm_gc::object::MarkColor::White {
            header.set_mark(otter_vm_gc::object::MarkColor::Gray);
            otter_vm_gc::barrier_push(header_ptr);
        }
    }
    0
}

/// Primitive toString() — no context, no method resolution, no IC.
///
/// Handles the common case: `number.toString()`, `string.toString()`,
/// `boolean.toString()`. Returns the NaN-boxed string Value on success
/// or BAILOUT_SENTINEL for non-primitive receivers (objects, etc.).
///
/// This is ~10x cheaper than full CallMethod for primitive receivers:
/// no constant table lookup, no IC, no interpreter re-entry.
///
/// Signature: `(value_raw: i64) -> i64`
#[allow(unsafe_code)]
pub(crate) extern "C" fn otter_rt_primitive_to_string(value_raw: i64) -> i64 {
    let bits = value_raw as u64;

    // String → identity
    if (bits & TAG_MASK) == 0x7FFD_0000_0000_0000 {
        return value_raw;
    }

    // Boolean
    if bits == 0x7FF8_0000_0000_0002 {
        // TAG_TRUE
        return crate::value::Value::string(JsString::intern("true")).to_jit_bits();
    }
    if bits == 0x7FF8_0000_0000_0003 {
        // TAG_FALSE
        return crate::value::Value::string(JsString::intern("false")).to_jit_bits();
    }

    // Int32
    if (bits & 0xFFFF_FFFF_0000_0000) == TAG_INT32 {
        let n = (bits & 0x0000_0000_FFFF_FFFF) as i32;
        let s = crate::globals::js_number_to_string(n as f64);
        return crate::value::Value::string(JsString::intern(&s)).to_jit_bits();
    }

    // Float64 (raw double: quiet NaN bits NOT set)
    if (bits & 0x7FF8_0000_0000_0000) != 0x7FF8_0000_0000_0000 {
        let n = f64::from_bits(bits);
        let s = crate::globals::js_number_to_string(n);
        return crate::value::Value::string(JsString::intern(&s)).to_jit_bits();
    }

    // Canonical NaN (0x7FFA_0000_0000_0000)
    if bits == 0x7FFA_0000_0000_0000 {
        return crate::value::Value::string(JsString::intern("NaN")).to_jit_bits();
    }

    // Undefined → "undefined"
    if bits == 0x7FF8_0000_0000_0000 {
        return crate::value::Value::string(JsString::intern("undefined")).to_jit_bits();
    }

    // Null → "null" (would throw in real JS, but let the full helper handle that)
    // Everything else (objects, functions, etc.) → bail to full CallMethod
    BAILOUT_SENTINEL
}

/// Dense array element read — no IC, no context needed.
///
/// Checks: is array, index is non-negative int32, in-bounds, not a hole.
/// Returns element value or BAILOUT_SENTINEL on miss.
///
/// Signature: `(obj: i64, index_value: i64, unused: i64) -> i64`
///
/// The third argument is unused (padding to match 3-arg helper signature).
#[allow(unsafe_code)]
pub(crate) extern "C" fn otter_rt_get_elem_dense_impl(
    obj_raw: i64,
    index_raw: i64,
    _unused: i64,
) -> i64 {
    let obj_ref = match extract_js_object_ref(obj_raw as u64) {
        Some(o) => o,
        None => return BAILOUT_SENTINEL,
    };

    // Check it's an array
    let flags = obj_ref.flags.borrow();
    if !flags.is_array {
        return BAILOUT_SENTINEL;
    }
    drop(flags);

    // Extract int32 index from NaN-boxed value
    let index_bits = index_raw as u64;
    // TAG_INT32 = 0x7FF8_0001_0000_0000 — int32 values have this prefix
    if (index_bits & 0xFFFF_FFFF_0000_0000) != TAG_INT32 {
        return BAILOUT_SENTINEL;
    }
    let index = (index_bits & 0x0000_0000_FFFF_FFFF) as i32;
    if index < 0 {
        return BAILOUT_SENTINEL;
    }
    let idx = index as usize;

    // Direct element access
    let elements = obj_ref.elements.borrow();

    // Sync JIT elements cache so subsequent inline accesses can use cached values
    let (data_ptr, data_len) = elements.jit_data_ptr_and_len();
    let kind = elements.jit_kind();
    obj_ref.jit_elements_data.set(data_ptr);
    obj_ref.jit_elements_len.set(data_len);
    obj_ref.jit_elements_kind.set(kind);

    match &*elements {
        crate::object::ElementsKind::Smi(v) => {
            if idx < v.len() {
                crate::value::Value::int32(v[idx]).to_jit_bits()
            } else {
                BAILOUT_SENTINEL
            }
        }
        crate::object::ElementsKind::Double(v) => {
            if idx < v.len() {
                crate::value::Value::number(v[idx]).to_jit_bits()
            } else {
                BAILOUT_SENTINEL
            }
        }
        crate::object::ElementsKind::Object(v) => {
            if idx < v.len() {
                let val = v[idx];
                if val.is_hole() {
                    BAILOUT_SENTINEL
                } else {
                    val.to_jit_bits()
                }
            } else {
                BAILOUT_SENTINEL
            }
        }
    }
}

/// Runtime helper: fast array push (no method resolution).
///
/// Signature: `(obj: i64, value: i64) -> i64`
///
/// Extracts array from NaN-boxed pointer, pushes value, returns new length
/// as NaN-boxed int32. Returns BAILOUT_SENTINEL if not an array.
#[allow(unsafe_code)]
pub(crate) extern "C" fn otter_rt_array_push_impl(obj_raw: i64, value_raw: i64) -> i64 {
    let obj_ref = match extract_js_object_ref(obj_raw as u64) {
        Some(o) => o,
        None => return BAILOUT_SENTINEL,
    };
    let flags = obj_ref.flags.borrow();
    if !flags.is_array {
        return BAILOUT_SENTINEL;
    }
    drop(flags);

    let value = match crate::value::Value::from_jit_bits(value_raw as u64) {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };

    let new_len = obj_ref.array_push(value);
    obj_ref.sync_jit_elements();
    crate::value::Value::int32(new_len as i32).to_jit_bits()
}

/// Runtime helper: fast array pop (no method resolution).
///
/// Signature: `(obj: i64) -> i64`
///
/// Extracts array from NaN-boxed pointer, pops last element, returns it
/// as NaN-boxed value. Returns BAILOUT_SENTINEL if not an array.
#[allow(unsafe_code)]
pub(crate) extern "C" fn otter_rt_array_pop_impl(obj_raw: i64) -> i64 {
    let obj_ref = match extract_js_object_ref(obj_raw as u64) {
        Some(o) => o,
        None => return BAILOUT_SENTINEL,
    };
    let flags = obj_ref.flags.borrow();
    if !flags.is_array {
        return BAILOUT_SENTINEL;
    }
    drop(flags);

    let val = obj_ref.array_pop();
    obj_ref.sync_jit_elements();
    val.to_jit_bits()
}

/// JIT back-edge tier-up check.
///
/// Called when the JIT back-edge budget reaches zero. Checks if the function's
/// IC state has changed since compilation (Uninitialized → Monomorphic transitions).
/// If recompilation is needed, returns `NeedsRecompilation` bailout sentinel.
/// Otherwise resets the budget and returns 0 to continue execution.
///
/// Signature: `(ctx) -> 0_or_bailout`
#[allow(unsafe_code)]
extern "C" fn otter_rt_check_tier_up(ctx_raw: i64) -> i64 {
    let ctx = unsafe { &mut *(ctx_raw as *mut JitContext) };

    // Check if IC recompilation was requested by a helper
    let func = unsafe { &*ctx.function_ptr };
    if func
        .ic_recompilation_needed
        .load(std::sync::atomic::Ordering::Acquire)
    {
        // Signal the caller to bail out for recompilation.
        // The interpreter's try_back_edge_osr will handle the actual recompile.
        return BAILOUT_SENTINEL;
    }

    // No recompilation needed — reset the budget and continue
    ctx.tier_up_budget = otter_vm_jit::runtime_helpers::JIT_TIER_UP_BUDGET_DEFAULT;
    0
}

#[allow(unsafe_code)]
extern "C" fn otter_rt_get_prop_const(
    ctx_raw: i64,
    obj_raw: i64,
    name_idx: i64,
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

    // Accept both OBJECT and ARRAY tags (arrays are JsObjects with array flag).
    if tag != gc_tags::OBJECT && tag != gc_tags::ARRAY {
        return BAILOUT_SENTINEL;
    }

    // SAFETY: We verified the tag is OBJECT or ARRAY, so raw_ptr is *const JsObject.
    // The object is alive (reachable from interpreter stack). No GC during JIT.
    let obj_ref = unsafe { &*(raw_ptr as *const JsObject) };

    // Fast path: array .length access (virtual property, not in shape/IC).
    // Some array-like objects can carry OBJECT GC tag while still using array
    // semantics (`flags.is_array`), so rely on the object flag, not only tag.
    if is_length_constant(ctx, name_idx) {
        let flags = obj_ref.flags.borrow();
        if flags.is_array {
            return crate::value::Value::int32(obj_ref.array_length() as i32).to_jit_bits();
        }
    }

    // Get shape pointer for comparison — avoids Arc clone (no atomic refcount).
    let obj_shape_ptr = unsafe { obj_ref.shape_id_unchecked() };

    // Read IC from feedback vector
    let function = unsafe { &*ctx.function_ptr };
    let feedback = function.feedback_vector.write();
    let Some(ic) = feedback.get_mut(ic_idx as usize) else {
        return BAILOUT_SENTINEL;
    };

    // Check proto epoch — but only for ICs that have cached data.
    // Uninitialized ICs have no cached shape data, so the proto_epoch check
    // is meaningless for them. Skipping it allows the slow path to resolve
    // the property and warm up the IC + probe table, enabling the runtime IC
    // probe fast path on subsequent iterations.
    let is_uninitialized = matches!(ic.ic_state, InlineCacheState::Uninitialized);
    if !is_uninitialized && !ic.proto_epoch_matches(ctx.proto_epoch) {
        return BAILOUT_SENTINEL;
    }

    // IC fast path — extract offset from IC state, then read property.
    // If the shape matches the IC entry, the object is guaranteed not in
    // dictionary mode (dictionary transitions invalidate shape-based IC).
    let mut cached_val = None;
    match &mut ic.ic_state {
        InlineCacheState::Monomorphic {
            shape_id,
            proto_shape_id,
            depth,
            offset,
        } => {
            if obj_shape_ptr == *shape_id {
                if *depth == 0 {
                    cached_val = unsafe { obj_ref.get_by_offset_unchecked(*offset as usize) };
                } else {
                    let mut current = obj_ref;
                    let mut valid = true;
                    for _ in 0..*depth {
                        if let Some(proto) = current.prototype().as_object() {
                            current = unsafe { &*(&*proto as *const _) };
                        } else {
                            valid = false;
                            break;
                        }
                    }
                    if valid && current.shape_id() == *proto_shape_id {
                        cached_val = unsafe { current.get_by_offset_unchecked(*offset as usize) };
                    }
                }
            }
        }
        InlineCacheState::Polymorphic { count, entries } => {
            for i in 0..(*count as usize) {
                if obj_shape_ptr == entries[i].0 {
                    let proto_shape_id = entries[i].1;
                    let depth = entries[i].2;
                    let offset = entries[i].3;

                    if depth == 0 {
                        cached_val = unsafe { obj_ref.get_by_offset_unchecked(offset as usize) };
                    } else {
                        let mut current = obj_ref;
                        let mut valid = true;
                        for _ in 0..depth {
                            if let Some(proto) = current.prototype().as_object() {
                                current = unsafe { &*(&*proto as *const _) };
                            } else {
                                valid = false;
                                break;
                            }
                        }
                        if valid && current.shape_id() == proto_shape_id {
                            cached_val =
                                unsafe { current.get_by_offset_unchecked(offset as usize) };
                        }
                    }

                    // MRU: promote to front
                    if i > 0 && cached_val.is_some() {
                        entries.swap(0, i);
                    }
                    break;
                }
            }
        }
        _ => {}
    }

    if let Some(val) = cached_val {
        ic.record_hit();
        return val.to_jit_bits();
    }

    let Some(name_str) = (unsafe { resolve_constant_string(ctx, name_idx) }) else {
        return BAILOUT_SENTINEL;
    };
    let key = PropertyKey::from_js_string(JsString::intern_utf16(name_str));

    // Slow path on IC miss: resolve by shape offset and update IC.
    if !obj_ref.is_dictionary_mode() {
        let mut current_obj = Some(obj_ref);
        let mut depth = 0;
        let mut found_offset = None;
        let mut found_shape = 0;

        while let Some(obj) = current_obj.take() {
            if obj.is_dictionary_mode() {
                break;
            }
            if let Some(offset) = obj.shape_get_offset(&key) {
                found_offset = Some(offset);
                found_shape = obj.shape_id();
                break;
            }
            if let Some(proto) = obj.prototype().as_object() {
                current_obj = Some(unsafe { &*(&*proto as *const _) });
                depth += 1;
            }
        }

        if let Some(offset) = found_offset {
            let current_epoch = ctx.proto_epoch;
            let proto_shape_id = if depth > 0 { found_shape } else { 0 };

            let ic_idx_usize = ic_idx as usize;
            match &mut ic.ic_state {
                InlineCacheState::Uninitialized => {
                    ic.ic_state = InlineCacheState::Monomorphic {
                        shape_id: obj_shape_ptr,
                        proto_shape_id,
                        depth,
                        offset: offset as u32,
                    };
                    ic.proto_epoch = current_epoch;
                    // Update JIT IC probe table for runtime inline fast path
                    if depth == 0 {
                        if let Some(probe) = function.jit_ic_probes.get_mut(ic_idx_usize) {
                            probe.set_mono_inline(obj_shape_ptr, offset as u32);
                        }
                    } else if let Some(probe) = function.jit_ic_probes.get_mut(ic_idx_usize) {
                        probe.set_other();
                    }
                }
                InlineCacheState::Monomorphic {
                    shape_id: old_shape,
                    proto_shape_id: old_proto_shape,
                    depth: old_depth,
                    offset: old_offset,
                } => {
                    if *old_shape != obj_shape_ptr {
                        let mut entries = [(0u64, 0u64, 0u8, 0u32); 4];
                        entries[0] = (*old_shape, *old_proto_shape, *old_depth, *old_offset);
                        entries[1] = (obj_shape_ptr, proto_shape_id, depth, offset as u32);
                        ic.ic_state = InlineCacheState::Polymorphic { count: 2, entries };
                        ic.proto_epoch = current_epoch;
                        // Probe degrades: no longer simple monomorphic
                        if let Some(probe) = function.jit_ic_probes.get_mut(ic_idx_usize) {
                            probe.set_other();
                        }
                    }
                }
                InlineCacheState::Polymorphic { count, entries } => {
                    let mut found = false;
                    for entry in &entries[..(*count as usize)] {
                        if entry.0 == obj_shape_ptr {
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        if (*count as usize) < 4 {
                            entries[*count as usize] =
                                (obj_shape_ptr, proto_shape_id, depth, offset as u32);
                            *count += 1;
                            ic.proto_epoch = current_epoch;
                        } else {
                            ic.ic_state = InlineCacheState::Megamorphic;
                        }
                    }
                }
                _ => {}
            }

            // Read value back
            let mut current = obj_ref;
            for _ in 0..depth {
                if let Some(proto) = current.prototype().as_object() {
                    current = unsafe { &*(&*proto as *const _) };
                }
            }
            if let Some(val) = unsafe { current.get_by_offset_unchecked(offset) } {
                return val.to_jit_bits();
            }
        }
    }

    // Generic object property read fallback.
    obj_ref
        .get(&key)
        .unwrap_or_else(crate::value::Value::undefined)
        .to_jit_bits()
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
    name_idx: i64,
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

    if tag != gc_tags::OBJECT && tag != gc_tags::ARRAY {
        return BAILOUT_SENTINEL;
    }

    let obj_ref = unsafe { &*(raw_ptr as *const JsObject) };

    // Get shape pointer — avoids Arc clone (no atomic refcount).
    let obj_shape_ptr = unsafe { obj_ref.shape_id_unchecked() };

    let function = unsafe { &*ctx.function_ptr };
    let feedback = function.feedback_vector.write();
    let Some(ic) = feedback.get_mut(ic_idx as usize) else {
        return BAILOUT_SENTINEL;
    };

    // Reconstruct the Value to write.
    // SAFETY: We are in the JIT execution scope — no GC has occurred.
    // Pointer-tagged values (objects, strings, arrays) are still live.
    let value_bits = value_raw as u64;
    let write_value = if (value_bits & TAG_MASK) == TAG_POINTER {
        match unsafe { crate::value::Value::from_raw_bits_unchecked(value_bits) } {
            Some(v) => v,
            None => return BAILOUT_SENTINEL,
        }
    } else {
        match crate::value::Value::from_jit_bits(value_bits) {
            Some(v) => v,
            None => return BAILOUT_SENTINEL,
        }
    };

    // IC fast path — extract offset, then write.
    // Shape match implies object is not in dictionary mode.
    if ic.proto_epoch_matches(ctx.proto_epoch) {
        let cached_offset: Option<u32> = match &ic.ic_state {
            InlineCacheState::Monomorphic {
                shape_id, offset, ..
            } => {
                if obj_shape_ptr == *shape_id {
                    Some(*offset)
                } else {
                    None
                }
            }
            InlineCacheState::Polymorphic { count, entries } => {
                let mut found = None;
                for entry in &entries[..(*count as usize)] {
                    if obj_shape_ptr == entry.0 {
                        found = Some(entry.3);
                        break;
                    }
                }
                found
            }
            _ => None,
        };

        if let Some(offset) = cached_offset
            && obj_ref.set_by_offset(offset as usize, write_value).is_ok()
        {
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

    let Some(name_str) = (unsafe { resolve_constant_string(ctx, name_idx) }) else {
        return BAILOUT_SENTINEL;
    };
    let key = PropertyKey::from_js_string(JsString::intern_utf16(name_str));

    // Slow path on IC miss: resolve by shape offset and update IC.
    if !obj_ref.is_dictionary_mode()
        && let Some(offset) = obj_ref.shape_get_offset(&key)
    {
        let current_epoch = ctx.proto_epoch;
        let ic_idx_usize = ic_idx as usize;
        match &mut ic.ic_state {
            InlineCacheState::Uninitialized => {
                ic.ic_state = InlineCacheState::Monomorphic {
                    shape_id: obj_shape_ptr,
                    proto_shape_id: 0,
                    depth: 0,
                    offset: offset as u32,
                };
                ic.proto_epoch = current_epoch;
                // Update JIT IC probe table for runtime inline fast path
                if let Some(probe) = function.jit_ic_probes.get_mut(ic_idx_usize) {
                    probe.set_mono_inline(obj_shape_ptr, offset as u32);
                }
            }
            InlineCacheState::Monomorphic {
                shape_id: old_shape,
                offset: old_offset,
                ..
            } => {
                if *old_shape != obj_shape_ptr {
                    let mut entries = [(0u64, 0u64, 0u8, 0u32); 4];
                    entries[0] = (*old_shape, 0, 0, *old_offset);
                    entries[1] = (obj_shape_ptr, 0, 0, offset as u32);
                    ic.ic_state = InlineCacheState::Polymorphic { count: 2, entries };
                    ic.proto_epoch = current_epoch;
                    if let Some(probe) = function.jit_ic_probes.get_mut(ic_idx_usize) {
                        probe.set_other();
                    }
                }
            }
            InlineCacheState::Polymorphic { count, entries } => {
                let mut found = false;
                for entry in &entries[..(*count as usize)] {
                    if entry.0 == obj_shape_ptr {
                        found = true;
                        break;
                    }
                }
                if !found {
                    if (*count as usize) < 4 {
                        entries[*count as usize] = (obj_shape_ptr, 0, 0, offset as u32);
                        *count += 1;
                        ic.proto_epoch = current_epoch;
                    } else {
                        ic.ic_state = InlineCacheState::Megamorphic;
                    }
                }
            }
            _ => {}
        }

        if obj_ref.set_by_offset(offset, write_value).is_ok() {
            return 0;
        }
    }

    // Generic object property write fallback.
    if obj_ref.set(key, write_value).is_ok() {
        0
    } else {
        BAILOUT_SENTINEL
    }
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
pub(crate) extern "C" fn otter_rt_call_function(
    ctx_raw: i64,
    callee_raw: i64,
    argc_raw: i64,
    argv_ptr_raw: i64,
) -> i64 {
    let callee_bits = callee_raw as u64;
    let argc = argc_raw as usize;

    // Fast path: callee is a Closure with TAG_PTR_FUNCTION + CLOSURE gc tag.
    // Inline the checks to avoid the overhead of Value reconstruction + as_function().
    if (callee_bits & TAG_MASK) == TAG_PTR_FUNCTION {
        let gc_tag = unsafe {
            let raw_ptr = (callee_bits & PAYLOAD_MASK) as *const u8;
            let offset = std::mem::offset_of!(crate::gc::GcBox<crate::value::Closure>, value);
            let header = &*(raw_ptr.sub(offset) as *const GcHeader);
            header.tag()
        };
        if gc_tag == gc_tags::CLOSURE {
            let closure: GcRef<crate::value::Closure> = unsafe {
                let raw_ptr = (callee_bits & PAYLOAD_MASK) as *const u8;
                let offset = std::mem::offset_of!(crate::gc::GcBox<crate::value::Closure>, value);
                let box_ptr = raw_ptr.sub(offset) as *mut crate::gc::GcBox<crate::value::Closure>;
                GcRef::from_gc(crate::gc::Gc::from_raw(std::ptr::NonNull::new_unchecked(
                    box_ptr,
                )))
            };
            if !closure.is_generator
                && !closure.is_async
                && let Some(func_info) = closure.module.function(closure.function_index)
                && !func_info.flags.has_rest
            {
                // Ultra-fast path: callee has JIT code — call directly via
                // rewritten JitContext, avoiding a full new JitContext allocation.
                let jit_ptr = func_info.jit_entry_ptr();
                if jit_ptr != 0 {
                    let ctx = unsafe { &mut *(ctx_raw as *mut JitContext) };
                    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
                    let this_raw = if func_info.flags.is_strict {
                        crate::value::Value::undefined().to_jit_bits()
                    } else {
                        crate::value::Value::object(vm_ctx.global()).to_jit_bits()
                    };
                    let reentry = JitCallReentryState::new(
                        func_info as *const Function,
                        &closure.module.constants as *const _,
                        if closure.upvalues.is_empty() {
                            std::ptr::null()
                        } else {
                            closure.upvalues.as_ptr()
                        },
                        closure.upvalues.len() as u32,
                        this_raw,
                        callee_raw,
                        crate::value::Value::null().to_jit_bits(),
                    );

                    let args_ptr = if argc == 0 {
                        std::ptr::null()
                    } else {
                        argv_ptr_raw as *const i64
                    };
                    let result = unsafe {
                        call_with_reentry_state(
                            ctx_raw as *mut JitContext,
                            args_ptr,
                            argc as u32,
                            jit_ptr,
                            &reentry,
                        )
                    };

                    if result != BAILOUT_SENTINEL {
                        return result;
                    }
                    // JIT bailout → fall through to interpreter
                } else {
                    // No JIT code yet — try full JIT path (may compile)
                    let args_ptr = if argc == 0 {
                        std::ptr::null()
                    } else {
                        argv_ptr_raw as *const i64
                    };
                    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
                    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
                    let this_raw = if func_info.flags.is_strict {
                        crate::value::Value::undefined().to_jit_bits()
                    } else {
                        crate::value::Value::object(vm_ctx.global()).to_jit_bits()
                    };
                    if let crate::jit_runtime::JitCallResult::Ok(value) =
                        crate::jit_runtime::try_execute_jit_from_raw_args(
                            closure.module.module_id,
                            closure.function_index,
                            func_info,
                            argc as u32,
                            args_ptr,
                            this_raw,
                            callee_raw,
                            crate::value::Value::null().to_jit_bits(),
                            vm_ctx.cached_proto_epoch,
                            ctx.interpreter,
                            ctx.vm_ctx,
                            &closure.module.constants as *const _,
                            &closure.upvalues,
                        )
                    {
                        return value.to_jit_bits();
                    }
                }
            }
        }
    }

    // Slow path: non-closure callee or JIT not available — full interpreter re-entry
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.interpreter.is_null() || ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }
    let callee = match unsafe { crate::value::Value::from_raw_bits_unchecked(callee_bits) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };
    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    match unsafe {
        with_collected_args(argc, argv_ptr_raw, |args| {
            interpreter.call_function(vm_ctx, &callee, crate::value::Value::undefined(), args)
        })
    } {
        Some(Ok(result)) => result.to_jit_bits(),
        _ => BAILOUT_SENTINEL,
    }
}

/// Runtime helper: monomorphic call — like `otter_rt_call_function` but with
/// expected `function_index` passed from JIT feedback. When the callee matches
/// the expected function_index, skips is_generator/is_async/has_rest checks
/// (we know they were false at profiling time).
///
/// Signature: `(ctx: i64, callee: i64, argc: i64, argv_ptr: i64, expected_func_index: i64) -> i64`
///
/// `expected_func_index` encodes `(function_index + 1)` so 0 = no hint.
#[allow(unsafe_code)]
#[allow(dead_code)]
pub(crate) extern "C" fn otter_rt_call_mono_impl(
    ctx_raw: i64,
    callee_raw: i64,
    argc_raw: i64,
    argv_ptr_raw: i64,
    expected_func_index_raw: i64,
) -> i64 {
    let callee_bits = callee_raw as u64;
    let argc = argc_raw as usize;
    let expected_func_index = expected_func_index_raw as u32;

    // Fast path: callee is a Closure with TAG_PTR_FUNCTION + CLOSURE gc tag.
    if (callee_bits & TAG_MASK) == TAG_PTR_FUNCTION {
        let gc_tag = unsafe {
            let raw_ptr = (callee_bits & PAYLOAD_MASK) as *const u8;
            let offset = std::mem::offset_of!(crate::gc::GcBox<crate::value::Closure>, value);
            let header = &*(raw_ptr.sub(offset) as *const GcHeader);
            header.tag()
        };
        if gc_tag == gc_tags::CLOSURE {
            let closure: GcRef<crate::value::Closure> = unsafe {
                let raw_ptr = (callee_bits & PAYLOAD_MASK) as *const u8;
                let offset = std::mem::offset_of!(crate::gc::GcBox<crate::value::Closure>, value);
                let box_ptr = raw_ptr.sub(offset) as *mut crate::gc::GcBox<crate::value::Closure>;
                GcRef::from_gc(crate::gc::Gc::from_raw(std::ptr::NonNull::new_unchecked(
                    box_ptr,
                )))
            };

            // Monomorphic guard: if function_index matches expected, skip
            // is_generator/is_async checks (they were false at profiling time).
            let mono_hit = expected_func_index != 0
                && closure.function_index.wrapping_add(1) == expected_func_index;

            if (mono_hit || (!closure.is_generator && !closure.is_async))
                && let Some(func_info) = closure.module.function(closure.function_index)
                && (mono_hit || !func_info.flags.has_rest)
            {
                let jit_ptr = func_info.jit_entry_ptr();
                if jit_ptr != 0 {
                    let ctx = unsafe { &mut *(ctx_raw as *mut JitContext) };
                    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
                    let this_raw = if func_info.flags.is_strict {
                        crate::value::Value::undefined().to_jit_bits()
                    } else {
                        crate::value::Value::object(vm_ctx.global()).to_jit_bits()
                    };
                    let reentry = JitCallReentryState::new(
                        func_info as *const Function,
                        &closure.module.constants as *const _,
                        if closure.upvalues.is_empty() {
                            std::ptr::null()
                        } else {
                            closure.upvalues.as_ptr()
                        },
                        closure.upvalues.len() as u32,
                        this_raw,
                        callee_raw,
                        crate::value::Value::null().to_jit_bits(),
                    );

                    let args_ptr = if argc == 0 {
                        std::ptr::null()
                    } else {
                        argv_ptr_raw as *const i64
                    };
                    let result = unsafe {
                        call_with_reentry_state(
                            ctx_raw as *mut JitContext,
                            args_ptr,
                            argc as u32,
                            jit_ptr,
                            &reentry,
                        )
                    };

                    if result != BAILOUT_SENTINEL {
                        return result;
                    }
                } else {
                    // No JIT code yet — try full JIT path
                    let args_ptr = if argc == 0 {
                        std::ptr::null()
                    } else {
                        argv_ptr_raw as *const i64
                    };
                    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
                    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
                    let this_raw = if func_info.flags.is_strict {
                        crate::value::Value::undefined().to_jit_bits()
                    } else {
                        crate::value::Value::object(vm_ctx.global()).to_jit_bits()
                    };
                    if let crate::jit_runtime::JitCallResult::Ok(value) =
                        crate::jit_runtime::try_execute_jit_from_raw_args(
                            closure.module.module_id,
                            closure.function_index,
                            func_info,
                            argc as u32,
                            args_ptr,
                            this_raw,
                            callee_raw,
                            crate::value::Value::null().to_jit_bits(),
                            vm_ctx.cached_proto_epoch,
                            ctx.interpreter,
                            ctx.vm_ctx,
                            &closure.module.constants as *const _,
                            &closure.upvalues,
                        )
                    {
                        return value.to_jit_bits();
                    }
                }
            }
        }
    }

    // Slow path: fall through to regular call
    otter_rt_call_function(ctx_raw, callee_raw, argc_raw, argv_ptr_raw)
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

    // Fast path: resolve Object.prototype via realm intrinsics (no global lookup).
    // Fallback to the global-property path only if realm metadata is unavailable.
    let realm_id = vm_ctx
        .current_frame()
        .map(|frame| frame.realm_id)
        .unwrap_or_else(|| vm_ctx.realm_id());
    let proto = vm_ctx
        .realm_intrinsics(realm_id)
        .map(|intrinsics| intrinsics.object_prototype)
        .or_else(|| {
            vm_ctx
                .global()
                .get(&crate::object::PropertyKey::string("Object"))
                .and_then(|obj_ctor| {
                    obj_ctor
                        .as_object()
                        .and_then(|o| o.get(&crate::object::PropertyKey::string("prototype")))
                })
                .and_then(|proto_val| proto_val.as_object())
        });

    let obj = crate::gc::GcRef::new(JsObject::new_with_shared_shape(
        proto
            .map(crate::value::Value::object)
            .unwrap_or_else(crate::value::Value::null),
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

    let arr = crate::gc::GcRef::new(JsObject::array(len));

    // Fast path: realm intrinsics; fallback to global lookup.
    let realm_id = vm_ctx
        .current_frame()
        .map(|frame| frame.realm_id)
        .unwrap_or_else(|| vm_ctx.realm_id());
    if let Some(array_proto) = vm_ctx
        .realm_intrinsics(realm_id)
        .map(|intrinsics| intrinsics.array_prototype)
    {
        arr.set_prototype(crate::value::Value::object(array_proto));
    } else if let Some(array_obj) = vm_ctx.get_global("Array").and_then(|v| v.as_object())
        && let Some(array_proto) = array_obj
            .get(&crate::object::PropertyKey::string("prototype"))
            .and_then(|v| v.as_object())
    {
        arr.set_prototype(crate::value::Value::object(array_proto));
    }

    crate::value::Value::array(arr).to_jit_bits()
}

/// Runtime helper: LoadConst — materialize a module constant as a JS value.
///
/// Signature: `(ctx: i64, const_idx: i64) -> i64`
///
/// Supports Number/String/BigInt/Symbol constants directly.
/// RegExp and TemplateLiteral constants currently deopt to interpreter.
#[allow(unsafe_code)]
extern "C" fn otter_rt_load_const(ctx_raw: i64, const_idx_raw: i64) -> i64 {
    if const_idx_raw < 0 {
        return BAILOUT_SENTINEL;
    }

    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.constants.is_null() {
        return BAILOUT_SENTINEL;
    }

    let constants = unsafe { &*ctx.constants };
    let Some(constant) = constants.get(const_idx_raw as u32) else {
        return BAILOUT_SENTINEL;
    };

    match constant {
        otter_vm_bytecode::Constant::Number(n) => crate::value::Value::number(*n).to_jit_bits(),
        otter_vm_bytecode::Constant::String(units) => {
            crate::value::Value::string(JsString::intern_utf16(units)).to_jit_bits()
        }
        otter_vm_bytecode::Constant::BigInt(s) => {
            crate::value::Value::bigint(s.to_string()).to_jit_bits()
        }
        otter_vm_bytecode::Constant::Symbol(id) => {
            let sym = GcRef::new(crate::value::Symbol {
                id: *id,
                description: None,
            });
            crate::value::Value::symbol(sym).to_jit_bits()
        }
        otter_vm_bytecode::Constant::RegExp { .. }
        | otter_vm_bytecode::Constant::TemplateLiteral { .. } => BAILOUT_SENTINEL,
    }
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
        if let Some(ic) = feedback.get(ic_idx as usize)
            && let InlineCacheState::Monomorphic {
                shape_id: shape_addr,
                offset,
                ..
            } = &ic.ic_state
            && global_obj.shape_id() == *shape_addr
            && let Some(val) = global_obj.get_by_offset(*offset as usize)
        {
            return val.to_jit_bits();
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
    let header_ptr = unsafe { raw_ptr.sub(header_offset) as *const GcHeader };
    let tag = unsafe { (*header_ptr).tag() };
    // Accept both OBJECT and ARRAY tags — arrays are JsObjects.
    if tag != gc_tags::OBJECT && tag != gc_tags::ARRAY {
        return None;
    }
    Some(unsafe { &*(raw_ptr as *const JsObject) })
}

// ---------------------------------------------------------------------------
// Helper: reconstruct arguments from argv pointer
// ---------------------------------------------------------------------------
#[allow(unsafe_code)]
unsafe fn collect_args(argc: usize, argv_ptr: i64) -> Option<Vec<crate::value::Value>> {
    let mut args = Vec::with_capacity(argc);
    if argc > 0 {
        let argv = argv_ptr as *const i64;
        if argv.is_null() {
            return None;
        }
        for i in 0..argc {
            let bits = unsafe { *argv.add(i) as u64 };
            let arg = unsafe { crate::value::Value::from_raw_bits_unchecked(bits) }?;
            args.push(arg);
        }
    }
    Some(args)
}

/// Collect raw call arguments and pass them as a temporary slice.
///
/// For small arity calls, arguments live in a stack buffer to avoid heap
/// allocation on the hottest call helper paths.
#[allow(unsafe_code)]
unsafe fn with_collected_args<R, F>(argc: usize, argv_ptr: i64, f: F) -> Option<R>
where
    F: FnOnce(&[crate::value::Value]) -> R,
{
    const STACK_ARG_CAP: usize = 8;

    if argc == 0 {
        return Some(f(&[]));
    }

    let argv = argv_ptr as *const i64;
    if argv.is_null() {
        return None;
    }

    if argc <= STACK_ARG_CAP {
        let mut stack: [std::mem::MaybeUninit<crate::value::Value>; STACK_ARG_CAP] =
            [const { std::mem::MaybeUninit::uninit() }; STACK_ARG_CAP];
        let mut initialized = 0usize;

        for i in 0..argc {
            let bits = unsafe { *argv.add(i) as u64 };
            let arg = match unsafe { crate::value::Value::from_raw_bits_unchecked(bits) } {
                Some(v) => v,
                None => {
                    for slot in stack.iter_mut().take(initialized) {
                        unsafe { slot.assume_init_drop() };
                    }
                    return None;
                }
            };
            stack[i].write(arg);
            initialized += 1;
        }

        let args = unsafe {
            std::slice::from_raw_parts(stack.as_ptr() as *const crate::value::Value, argc)
        };
        let result = f(args);
        for slot in stack.iter_mut().take(initialized) {
            unsafe { slot.assume_init_drop() };
        }
        Some(result)
    } else {
        let mut heap_args = Vec::with_capacity(argc);
        for i in 0..argc {
            let bits = unsafe { *argv.add(i) as u64 };
            let arg = unsafe { crate::value::Value::from_raw_bits_unchecked(bits) }?;
            heap_args.push(arg);
        }
        Some(f(&heap_args))
    }
}

// ---------------------------------------------------------------------------
// Helper: simplified value_to_property_key (no ToPrimitive for objects)
// ---------------------------------------------------------------------------
fn value_to_property_key_simple(value: &crate::value::Value) -> Option<PropertyKey> {
    if let Some(sym) = value.as_symbol() {
        return Some(PropertyKey::Symbol(sym));
    }
    if let Some(n) = value.as_int32()
        && n >= 0
    {
        return Some(PropertyKey::Index(n as u32));
    }
    if let Some(s) = value.as_string() {
        // Check if it's an array index
        let str_val = s.as_str();
        if let Ok(idx) = str_val.parse::<u32>()
            && idx.to_string() == str_val
        {
            return Some(PropertyKey::Index(idx));
        }
        return Some(PropertyKey::from_js_string(s));
    }
    if let Some(n) = value.as_number() {
        // Numeric keys like 1.5 become string "1.5"
        let s = crate::globals::js_number_to_string(n);
        if let Ok(idx) = s.parse::<u32>()
            && idx.to_string() == s
        {
            return Some(PropertyKey::Index(idx));
        }
        return Some(PropertyKey::string_transient(&s));
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
    if vm_ctx
        .current_frame()
        .map(|frame| frame.open_upvalue_count == 0)
        .unwrap_or(true)
    {
        return 0;
    }
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
extern "C" fn otter_rt_get_elem_int(_ctx_raw: i64, obj_raw: i64, idx_raw: i64) -> i64 {
    let obj_bits = obj_raw as u64;
    let idx_val = match unsafe { crate::value::Value::from_raw_bits_unchecked(idx_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };

    // Only handle heap objects
    let Some(obj_ref) = (unsafe { extract_js_object(obj_bits) }) else {
        return BAILOUT_SENTINEL;
    };

    if obj_ref.is_array()
        && let Some(n) = idx_val.as_int32()
        && n >= 0
    {
        let elements = obj_ref.get_elements_storage().borrow();
        if let Some(v) = elements.get(n as usize)
            && !v.is_hole()
        {
            return v.to_jit_bits();
        }
    }

    BAILOUT_SENTINEL
}

/// Runtime helper: GetElem — dynamic property access.
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
    if obj_ref.is_array()
        && let Some(n) = idx_val.as_int32()
        && n >= 0
    {
        let elements = obj_ref.get_elements_storage().borrow();
        if let Some(v) = elements.get(n as usize)
            && !v.is_hole()
        {
            return v.to_jit_bits();
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
        let obj_shape_ptr = unsafe { obj_ref.shape_id_unchecked() };
        let function = unsafe { &*ctx.function_ptr };
        let feedback = function.feedback_vector.write();
        if let Some(ic) = feedback.get_mut(ic_idx as usize)
            && ic.proto_epoch_matches(ctx.proto_epoch)
        {
            let cached_offset = match &ic.ic_state {
                InlineCacheState::Monomorphic {
                    shape_id, offset, ..
                } => {
                    if obj_shape_ptr == *shape_id {
                        Some(*offset)
                    } else {
                        None
                    }
                }
                InlineCacheState::Polymorphic { count, entries } => {
                    let mut found = None;
                    for entry in &entries[..(*count as usize)] {
                        if obj_shape_ptr == entry.0 {
                            found = Some(entry.3);
                            break;
                        }
                    }
                    found
                }
                _ => None,
            };
            if let Some(offset) = cached_offset
                && let Some(val) = unsafe { obj_ref.get_by_offset_unchecked(offset as usize) }
            {
                ic.record_hit();
                return val.to_jit_bits();
            }
        }
    }

    // For integer indices on non-array objects (e.g., arguments, typed arrays),
    // try direct property lookup
    if let PropertyKey::Index(idx) = &key
        && let Some(val) = obj_ref.get(&PropertyKey::Index(*idx))
    {
        return val.to_jit_bits();
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
    if obj_ref.is_array()
        && let Some(n) = idx_val.as_int32()
        && n >= 0
    {
        let mut elements = obj_ref.get_elements_storage().borrow_mut();
        let idx = n as usize;
        if idx < elements.len() {
            crate::object::gc_write_barrier(&write_val);
            elements.set(idx, write_val);
            return 0;
        } else if idx == elements.len() {
            crate::object::gc_write_barrier(&write_val);
            elements.push(write_val);
            // Update length property
            let length_key = PropertyKey::string("length");
            if let Some(len_offset) = obj_ref.shape_get_offset(&length_key) {
                let _ = obj_ref
                    .set_by_offset(len_offset, crate::value::Value::number((idx + 1) as f64));
            }
            return 0;
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
        let obj_shape_ptr = unsafe { obj_ref.shape_id_unchecked() };
        let function = unsafe { &*ctx.function_ptr };
        let feedback = function.feedback_vector.write();
        if let Some(ic) = feedback.get_mut(ic_idx as usize)
            && ic.proto_epoch_matches(ctx.proto_epoch)
        {
            let cached_offset = match &ic.ic_state {
                InlineCacheState::Monomorphic {
                    shape_id, offset, ..
                } => {
                    if obj_shape_ptr == *shape_id {
                        Some(*offset)
                    } else {
                        None
                    }
                }
                InlineCacheState::Polymorphic { count, entries } => {
                    let mut found = None;
                    for entry in &entries[..(*count as usize)] {
                        if obj_shape_ptr == entry.0 {
                            found = Some(entry.3);
                            break;
                        }
                    }
                    found
                }
                _ => None,
            };
            if let Some(offset) = cached_offset
                && obj_ref.set_by_offset(offset as usize, write_val).is_ok()
            {
                ic.record_hit();
                return 0;
            }
        }
    }

    // For integer indices, try direct set on the object
    if let PropertyKey::Index(idx) = &key
        && obj_ref.set(PropertyKey::Index(*idx), write_val).is_ok()
    {
        return 0;
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
    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    match unsafe {
        with_collected_args(argc, argv_ptr_raw, |args| {
            interpreter.call_function_construct(
                vm_ctx,
                &callee,
                crate::value::Value::undefined(),
                args,
            )
        })
    } {
        Some(Ok(result)) => result.to_jit_bits(),
        _ => BAILOUT_SENTINEL,
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
    if ctx.constants.is_null() || method_name_idx < 0 {
        return BAILOUT_SENTINEL;
    }

    let receiver = match unsafe { crate::value::Value::from_raw_bits_unchecked(obj_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };
    let argc = argc_raw as usize;

    let constants = unsafe { &*ctx.constants };
    let method_name_units = match constants.get(method_name_idx as u32) {
        Some(otter_vm_bytecode::Constant::String(units)) => units,
        _ => return BAILOUT_SENTINEL,
    };
    let method_name = JsString::intern_utf16(method_name_units);
    let method_key = PropertyKey::from_js_string(method_name);

    // Very hot path in string benchmark: `(i % 10).toString()`.
    // Skip method lookup/call overhead for no-arg primitive toString.
    if argc == 0 && method_name_units.as_slice() == TO_STRING_UTF16 {
        if let Some(n) = receiver.as_number() {
            let s = crate::globals::js_number_to_string(n);
            return crate::value::Value::string(JsString::intern(&s)).to_jit_bits();
        }
        if let Some(s) = receiver.as_string() {
            return crate::value::Value::string(s).to_jit_bits();
        }
        if let Some(b) = receiver.as_boolean() {
            let out = if b { "true" } else { "false" };
            return crate::value::Value::string(JsString::intern(out)).to_jit_bits();
        }
    }

    let mut method: Option<crate::value::Value> = None;

    if let Some(obj_ref) = receiver.as_object() {
        let function = unsafe { &*ctx.function_ptr };
        let feedback = function.feedback_vector.write();

        if let Some(ic) = feedback.get_mut(ic_idx as usize) {
            // IC fast path
            if !obj_ref.is_dictionary_mode() && ic.proto_epoch_matches(ctx.proto_epoch) {
                let obj_shape_ptr = unsafe { obj_ref.shape_id_unchecked() };
                let cached_offset = match &mut ic.ic_state {
                    InlineCacheState::Monomorphic {
                        shape_id, offset, ..
                    } => {
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
                                found = Some(entries[i].3);
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
                if let Some(offset) = cached_offset
                    && let Some(val) = unsafe { obj_ref.get_by_offset_unchecked(offset as usize) }
                {
                    ic.record_hit();
                    method = Some(val);
                }
            }

            // Slow path on IC miss: resolve method and update IC.
            if method.is_none()
                && !obj_ref.is_dictionary_mode()
                && let Some(offset) = obj_ref.shape_get_offset(&method_key)
            {
                let shape_ptr = unsafe { obj_ref.shape_id_unchecked() };
                let current_epoch = ctx.proto_epoch;
                match &mut ic.ic_state {
                    InlineCacheState::Uninitialized => {
                        ic.ic_state = InlineCacheState::Monomorphic {
                            shape_id: shape_ptr,
                            proto_shape_id: 0,
                            depth: 0,
                            offset: offset as u32,
                        };
                        ic.proto_epoch = current_epoch;
                    }
                    InlineCacheState::Monomorphic {
                        shape_id: old_shape,
                        offset: old_offset,
                        ..
                    } => {
                        if *old_shape != shape_ptr {
                            let mut entries = [(0u64, 0u64, 0u8, 0u32); 4];
                            entries[0] = (*old_shape, 0, 0, *old_offset);
                            entries[1] = (shape_ptr, 0, 0, offset as u32);
                            ic.ic_state = InlineCacheState::Polymorphic { count: 2, entries };
                            ic.proto_epoch = current_epoch;
                        }
                    }
                    InlineCacheState::Polymorphic { count, entries } => {
                        let mut found = false;
                        for entry in &entries[..(*count as usize)] {
                            if entry.0 == shape_ptr {
                                found = true;
                                break;
                            }
                        }
                        if !found {
                            if (*count as usize) < 4 {
                                entries[*count as usize] = (shape_ptr, 0, 0, offset as u32);
                                *count += 1;
                                ic.proto_epoch = current_epoch;
                            } else {
                                ic.ic_state = InlineCacheState::Megamorphic;
                            }
                        }
                    }
                    _ => {}
                }
                method = unsafe { obj_ref.get_by_offset_unchecked(offset) };
            }
        }
        if method.is_none() {
            method = obj_ref.get(&method_key);
        }
        // ic is no longer used here, so feedback can be dropped at end of scope.
    } else {
        let vm_ctx = unsafe { &mut *ctx.vm_ctx };
        method = if receiver.is_string() {
            let string_obj = vm_ctx.get_global("String").and_then(|v| v.as_object());
            let string_proto = string_obj.and_then(|ctor| {
                ctor.get(&PropertyKey::string("prototype"))
                    .and_then(|v| v.as_object())
            });
            string_proto
                .and_then(|proto| proto.get(&method_key))
                .or_else(|| Some(crate::value::Value::undefined()))
        } else if receiver.is_number() {
            let number_obj = vm_ctx.get_global("Number").and_then(|v| v.as_object());
            let number_proto = number_obj.and_then(|ctor| {
                ctor.get(&PropertyKey::string("prototype"))
                    .and_then(|v| v.as_object())
            });
            number_proto
                .and_then(|proto| proto.get(&method_key))
                .or_else(|| Some(crate::value::Value::undefined()))
        } else if receiver.is_boolean() {
            let boolean_obj = vm_ctx.get_global("Boolean").and_then(|v| v.as_object());
            let boolean_proto = boolean_obj.and_then(|ctor| {
                ctor.get(&PropertyKey::string("prototype"))
                    .and_then(|v| v.as_object())
            });
            boolean_proto
                .and_then(|proto| proto.get(&method_key))
                .or_else(|| Some(crate::value::Value::undefined()))
        } else {
            None
        };
    }
    let method = method.unwrap_or_else(crate::value::Value::undefined);

    // Array push/pop fast path for JIT
    if let Some(fn_obj) = method.native_function_object() {
        let flags = fn_obj.flags.borrow();
        if (flags.is_array_push || flags.is_array_pop)
            && let Some(receiver_obj) = receiver.as_object()
            && receiver_obj.is_array()
            && !receiver_obj.is_dictionary_mode()
            && receiver_obj.array_length_writable()
            && !receiver_obj.is_frozen()
        {
            if flags.is_array_push {
                return match unsafe {
                    with_collected_args(argc, argv_ptr_raw, |args| {
                        let mut last_len = receiver_obj.array_length();
                        for arg in args {
                            last_len = receiver_obj.array_push(*arg);
                        }
                        if args.is_empty() {
                            last_len = receiver_obj.array_length();
                        }
                        crate::value::Value::number(last_len as f64)
                    })
                } {
                    Some(result) => result.to_jit_bits(),
                    _ => BAILOUT_SENTINEL,
                };
            } else if flags.is_array_pop {
                let val = receiver_obj.array_pop();
                return val.to_jit_bits();
            }
        }
    }

    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    match unsafe {
        with_collected_args(argc, argv_ptr_raw, |args| {
            interpreter.call_function(vm_ctx, &method, receiver, args)
        })
    } {
        Some(Ok(result)) => result.to_jit_bits(),
        _ => BAILOUT_SENTINEL,
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

    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    // Fast path: direct JIT-to-JIT call for JS closures with compiled code.
    if let Some(closure) = callee.as_function()
        && !closure.is_generator
        && !closure.is_async
        && let Some(func_info) = closure.module.function(closure.function_index)
        && !func_info.flags.has_rest
    {
        let args_ptr = if argc == 0 {
            std::ptr::null()
        } else {
            argv_ptr_raw as *const i64
        };
        if argc > 0 && args_ptr.is_null() {
            return BAILOUT_SENTINEL;
        }
        let this_raw_for_jit =
            if !func_info.flags.is_strict && (this_val.is_undefined() || this_val.is_null()) {
                crate::value::Value::object(vm_ctx.global()).to_jit_bits()
            } else {
                this_val.to_jit_bits()
            };
        if let crate::jit_runtime::JitCallResult::Ok(value) =
            crate::jit_runtime::try_execute_jit_from_raw_args(
                closure.module.module_id,
                closure.function_index,
                func_info,
                argc as u32,
                args_ptr,
                this_raw_for_jit,
                callee.to_jit_bits(),
                crate::value::Value::null().to_jit_bits(),
                vm_ctx.cached_proto_epoch,
                ctx.interpreter,
                ctx.vm_ctx,
                &closure.module.constants as *const _,
                &closure.upvalues,
            )
        {
            return value.to_jit_bits();
        }
    }

    match unsafe {
        with_collected_args(argc, argv_ptr_raw, |args| {
            interpreter.call_function(vm_ctx, &callee, this_val, args)
        })
    } {
        Some(Ok(result)) => result.to_jit_bits(),
        _ => BAILOUT_SENTINEL,
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

    // Array push/pop fast path for JIT
    if let Some(fn_obj) = method.native_function_object() {
        let flags = fn_obj.flags.borrow();
        if (flags.is_array_push || flags.is_array_pop)
            && let Some(receiver_obj) = receiver.as_object()
            && receiver_obj.is_array()
            && !receiver_obj.is_dictionary_mode()
            && receiver_obj.array_length_writable()
            && !receiver_obj.is_frozen()
        {
            if flags.is_array_push {
                return match unsafe {
                    with_collected_args(argc, argv_ptr_raw, |args| {
                        let mut last_len = receiver_obj.array_length();
                        for arg in args {
                            last_len = receiver_obj.array_push(*arg);
                        }
                        if args.is_empty() {
                            last_len = receiver_obj.array_length();
                        }
                        crate::value::Value::number(last_len as f64)
                    })
                } {
                    Some(result) => result.to_jit_bits(),
                    _ => BAILOUT_SENTINEL,
                };
            } else if flags.is_array_pop {
                let val = receiver_obj.array_pop();
                return val.to_jit_bits();
            }
        }
    }

    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    match unsafe {
        with_collected_args(argc, argv_ptr_raw, |args| {
            interpreter.call_function(vm_ctx, &method, receiver, args)
        })
    } {
        Some(Ok(result)) => result.to_jit_bits(),
        _ => BAILOUT_SENTINEL,
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
/// Creates a sync/arrow closure with interpreter-equivalent constructor/prototype
/// semantics so JIT does not have to bail on ordinary nested closure creation.
#[allow(unsafe_code)]
extern "C" fn otter_rt_closure_create(ctx_raw: i64, func_idx: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    if ctx.vm_ctx.is_null() {
        return BAILOUT_SENTINEL;
    }
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    let module = match vm_ctx.current_frame() {
        Some(frame) => std::sync::Arc::clone(vm_ctx.module_table.get(frame.module_id)),
        None => return BAILOUT_SENTINEL,
    };

    let func_def = match module.function(func_idx as u32) {
        Some(f) => f,
        None => return BAILOUT_SENTINEL,
    };

    let captured = match capture_upvalues_for_jit(vm_ctx, &func_def.upvalues) {
        Ok(c) => c,
        Err(_) => return BAILOUT_SENTINEL,
    };

    let func_obj = GcRef::new(JsObject::new(crate::value::Value::null()));

    let obj_proto = vm_ctx
        .global()
        .get(&PropertyKey::string("Object"))
        .and_then(|obj_ctor| {
            obj_ctor
                .as_object()
                .and_then(|o| o.get(&PropertyKey::string("prototype")))
        })
        .and_then(|proto_val| proto_val.as_object());
    let proto = GcRef::new(JsObject::new(
        obj_proto
            .map(crate::value::Value::object)
            .unwrap_or_else(crate::value::Value::null),
    ));

    if let Some(fn_proto) = vm_ctx.function_prototype() {
        func_obj.set_prototype(crate::value::Value::object(fn_proto));
    }
    func_obj.define_property(
        PropertyKey::string("__realm_id__"),
        PropertyDescriptor::builtin_data(crate::value::Value::int32(vm_ctx.realm_id() as i32)),
    );

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
        is_async: func_def.is_async(),
        is_generator: false,
        object: func_obj,
        home_object: None,
    });
    let func_value = crate::value::Value::function(closure);

    if func_def.is_arrow() || func_def.is_async() {
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
    } else {
        func_obj.define_property(
            PropertyKey::string("prototype"),
            PropertyDescriptor::Data {
                value: crate::value::Value::object(proto),
                attributes: crate::object::PropertyAttributes {
                    writable: true,
                    enumerable: false,
                    configurable: false,
                },
            },
        );
        let _ = proto.set(PropertyKey::string("constructor"), func_value);
    }

    func_value.to_jit_bits()
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

    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    match unsafe {
        with_collected_args(argc as usize, argv_ptr, |args| {
            interpreter.call_function(vm_ctx, &callee, crate::value::Value::undefined(), args)
        })
    } {
        Some(Ok(result)) => result.to_jit_bits(),
        _ => BAILOUT_SENTINEL,
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
            if let Some(desc) = proto_obj.lookup_property_descriptor(&key)
                && let PropertyDescriptor::Accessor {
                    get: Some(getter), ..
                } = desc
            {
                // Call getter with `this` from JitContext
                if ctx.interpreter.is_null() || ctx.vm_ctx.is_null() {
                    return BAILOUT_SENTINEL;
                }
                let this_val =
                    unsafe { crate::value::Value::from_raw_bits_unchecked(ctx.this_raw as u64) };
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

    let _vm_ctx = unsafe { &mut *ctx.vm_ctx };

    if super_val.is_undefined() || super_val.is_null() {
        // Base class: ensure ctor.prototype.constructor = ctor
        if let Some(proto_val) = ctor_obj.get(&PropertyKey::string("prototype"))
            && let Some(proto_obj) = proto_val.as_object()
        {
            let _ = proto_obj.set(PropertyKey::string("constructor"), ctor_val);
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
    ));

    // Set ctor.prototype = derived_proto
    let _ = ctor_obj.set(
        PropertyKey::string("prototype"),
        crate::value::Value::object(derived_proto),
    );

    // Set derived_proto.constructor = ctor
    let _ = derived_proto.set(PropertyKey::string("constructor"), ctor_val);

    // Set ctor.__proto__ = super for static method inheritance
    ctor_obj.set_prototype(super_val);

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

    let interpreter = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };

    // Call super constructor
    match unsafe {
        with_collected_args(argc_raw as usize, argv_ptr_raw, |args| {
            interpreter.call_function_construct(
                vm_ctx,
                &super_ctor,
                crate::value::Value::undefined(),
                args,
            )
        })
    } {
        Some(Ok(result)) => result.to_jit_bits(),
        _ => BAILOUT_SENTINEL,
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
        let home_object = match frame.home_object {
            Some(ho) => ho,
            None => return BAILOUT_SENTINEL,
        };
        let new_target_proto = frame.new_target_proto.unwrap_or(home_object);
        let argc = frame.argc as usize;
        let callee_value = frame.callee_value;
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
            if let Some(proto_val) = super_closure.object.get(&proto_key)
                && let Some(proto_obj) = proto_val.as_object()
            {
                vm_ctx.set_pending_home_object(proto_obj);
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
        let _mm = vm_ctx.memory_manager().clone();
        let new_obj = GcRef::new(JsObject::new(crate::value::Value::object(new_target_proto)));
        let new_obj_value = crate::value::Value::object(new_obj);
        match interpreter.call_function_construct(vm_ctx, &super_ctor_val, new_obj_value, &args) {
            Ok(result) => {
                if result.is_object() {
                    if let Some(obj) = result.as_object() {
                        obj.set_prototype(crate::value::Value::object(new_target_proto));
                    }
                    result
                } else {
                    new_obj_value
                }
            }
            Err(_) => return BAILOUT_SENTINEL,
        }
    } else {
        let _mm = vm_ctx.memory_manager().clone();
        let new_obj = GcRef::new(JsObject::new(crate::value::Value::object(new_target_proto)));
        let new_obj_value = crate::value::Value::object(new_obj);
        match interpreter.call_function(vm_ctx, &super_ctor_val, new_obj_value, &args) {
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
        frame.this_value = this_value;
        frame.flags.set_this_initialized(true);
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
        Some(frame) => std::sync::Arc::clone(vm_ctx.module_table.get(frame.module_id)),
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

    let func_obj = GcRef::new(JsObject::new(crate::value::Value::null()));

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
        object: func_obj,
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
        Some(frame) => std::sync::Arc::clone(vm_ctx.module_table.get(frame.module_id)),
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
    ));

    let closure = GcRef::new(crate::value::Closure {
        function_index: func_idx as u32,
        module: std::sync::Arc::clone(&module),
        upvalues: captured,
        is_async: false,
        is_generator: true,
        object: func_obj,
        home_object: None,
    });
    let func_value = crate::value::Value::function(closure);

    // Generator prototype property
    func_obj.define_property(
        PropertyKey::string("prototype"),
        PropertyDescriptor::Data {
            value: crate::value::Value::object(proto),
            attributes: crate::object::PropertyAttributes {
                writable: true,
                enumerable: false,
                configurable: false,
            },
        },
    );
    let _ = proto.set(PropertyKey::string("constructor"), func_value);
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
        Some(frame) => std::sync::Arc::clone(vm_ctx.module_table.get(frame.module_id)),
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
    ));

    let closure = GcRef::new(crate::value::Closure {
        function_index: func_idx as u32,
        module: std::sync::Arc::clone(&module),
        upvalues: captured,
        is_async: true,
        is_generator: true,
        object: func_obj,
        home_object: None,
    });
    let func_value = crate::value::Value::function(closure);

    // Async generator prototype property
    func_obj.define_property(
        PropertyKey::string("prototype"),
        PropertyDescriptor::Data {
            value: crate::value::Value::object(proto),
            attributes: crate::object::PropertyAttributes {
                writable: true,
                enumerable: false,
                configurable: false,
            },
        },
    );
    let _ = proto.set(PropertyKey::string("constructor"), func_value);
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
            UpvalueCapture::Upvalue(idx) => *vm_ctx.get_upvalue_cell(idx.0)?,
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

// ---------------------------------------------------------------------------
// Generic arithmetic / comparison helpers
// ---------------------------------------------------------------------------
//
// These handle the slow path when JIT type guards fail (e.g., Int32 overflow,
// mixed Int32/Float64 operands). They cover numeric cases only; non-numeric
// operands (strings, objects, BigInt) return BAILOUT_SENTINEL.

/// Extract an f64 from a JIT NaN-boxed value (int32 or float64).
#[inline]
#[allow(unsafe_code)]
fn jit_to_f64(raw: i64) -> Option<f64> {
    let bits = raw as u64;
    // Fast check: is it an int32?
    if (bits & 0xFFFF_FFFF_0000_0000) == TAG_INT32 {
        let i = (bits & 0xFFFF_FFFF) as u32 as i32;
        Some(i as f64)
    } else {
        // Try to reconstruct as Value and check for number
        match unsafe { crate::value::Value::from_raw_bits_unchecked(bits) } {
            Some(v) => v.as_number().or_else(|| v.as_int32().map(|i| i as f64)),
            None => None,
        }
    }
}

/// Generic JS `+` for numeric and string operands.
/// Signature: `(ctx: i64, lhs: i64, rhs: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_generic_add(ctx_raw: i64, lhs_raw: i64, rhs_raw: i64) -> i64 {
    let bits_l = lhs_raw as u64;
    let bits_r = rhs_raw as u64;

    // Fast path: both are numbers
    if let (Some(l), Some(r)) = (jit_to_f64(lhs_raw), jit_to_f64(rhs_raw)) {
        return crate::value::Value::number(l + r).to_jit_bits() as i64;
    }

    // String concatenation path
    // SAFETY: from_raw_bits_unchecked is safe here because we are in a JIT helper scope.
    // It will reconstruct HeapRef::String if the bits match a string pointer.
    unsafe {
        let val_l = crate::value::Value::from_raw_bits_unchecked(bits_l);
        let val_r = crate::value::Value::from_raw_bits_unchecked(bits_r);

        if let (Some(l), Some(r)) = (val_l, val_r) {
            if let (Some(s_l), Some(s_r)) = (l.as_string(), r.as_string()) {
                // String + String
                return crate::value::Value::string(JsString::concat_gc(s_l, s_r)).to_jit_bits();
            }

            // Fallback for mixed types (e.g. String + Number)
            // If one is string, we convert the other to string too.
            if l.is_string() || r.is_string() {
                let ctx = &*(ctx_raw as *const JitContext);
                if !ctx.vm_ctx.is_null() && !ctx.interpreter.is_null() {
                    let vm_ctx = &mut *ctx.vm_ctx;
                    let interp = &*ctx.interpreter;

                    let s_l = if let Some(s) = l.as_string() {
                        s
                    } else {
                        match interp.to_string_value(vm_ctx, &l) {
                            Ok(s) => JsString::intern(&s),
                            Err(_) => return BAILOUT_SENTINEL,
                        }
                    };

                    let s_r = if let Some(s) = r.as_string() {
                        s
                    } else {
                        match interp.to_string_value(vm_ctx, &r) {
                            Ok(s) => JsString::intern(&s),
                            Err(_) => return BAILOUT_SENTINEL,
                        }
                    };

                    return crate::value::Value::string(JsString::concat_gc(s_l, s_r))
                        .to_jit_bits() as i64;
                }
            }
        }
    }

    BAILOUT_SENTINEL
}

/// Specialized JS `+` with type feedback update for JIT ICs.
/// Signature: `(ctx: i64, lhs: i64, rhs: i64, ic_idx: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_arith_add(ctx_raw: i64, lhs_raw: i64, rhs_raw: i64, ic_idx_raw: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    let lhs = Value::from_jit_bits(lhs_raw as u64).unwrap_or(Value::undefined());
    let rhs = Value::from_jit_bits(rhs_raw as u64).unwrap_or(Value::undefined());

    // Update IC feedback
    let func = unsafe { &*ctx.function_ptr };
    let vm_ctx = unsafe { &*ctx.vm_ctx };
    Interpreter::update_arithmetic_ic_on_function(vm_ctx, func, ic_idx_raw as u16, &lhs, &rhs, None);

    // Perform operation using Interpreter
    let interp = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    interp
        .op_add(vm_ctx, &lhs, &rhs)
        .map(|v| v.to_jit_bits() as i64)
        .unwrap_or(BAILOUT_SENTINEL)
}

/// Specialized JS `-` with type feedback update for JIT ICs.
#[allow(unsafe_code)]
extern "C" fn otter_rt_arith_sub(ctx_raw: i64, lhs_raw: i64, rhs_raw: i64, ic_idx_raw: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    let lhs = Value::from_jit_bits(lhs_raw as u64).unwrap_or(Value::undefined());
    let rhs = Value::from_jit_bits(rhs_raw as u64).unwrap_or(Value::undefined());
    let func = unsafe { &*ctx.function_ptr };
    Interpreter::update_arithmetic_ic_on_function(unsafe { &*ctx.vm_ctx }, func, ic_idx_raw as u16, &lhs, &rhs, None);
    let interp = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    interp
        .op_sub(vm_ctx, &lhs, &rhs)
        .map(|v| v.to_jit_bits() as i64)
        .unwrap_or(BAILOUT_SENTINEL)
}

/// Specialized JS `*` with type feedback update for JIT ICs.
#[allow(unsafe_code)]
extern "C" fn otter_rt_arith_mul(ctx_raw: i64, lhs_raw: i64, rhs_raw: i64, ic_idx_raw: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    let lhs = Value::from_jit_bits(lhs_raw as u64).unwrap_or(Value::undefined());
    let rhs = Value::from_jit_bits(rhs_raw as u64).unwrap_or(Value::undefined());
    let func = unsafe { &*ctx.function_ptr };
    Interpreter::update_arithmetic_ic_on_function(unsafe { &*ctx.vm_ctx }, func, ic_idx_raw as u16, &lhs, &rhs, None);
    let interp = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    interp
        .op_mul(vm_ctx, &lhs, &rhs)
        .map(|v| v.to_jit_bits() as i64)
        .unwrap_or(BAILOUT_SENTINEL)
}

/// Specialized JS `/` with type feedback update for JIT ICs.
#[allow(unsafe_code)]
extern "C" fn otter_rt_arith_div(ctx_raw: i64, lhs_raw: i64, rhs_raw: i64, ic_idx_raw: i64) -> i64 {
    let ctx = unsafe { &*(ctx_raw as *const JitContext) };
    let lhs = Value::from_jit_bits(lhs_raw as u64).unwrap_or(Value::undefined());
    let rhs = Value::from_jit_bits(rhs_raw as u64).unwrap_or(Value::undefined());
    let func = unsafe { &*ctx.function_ptr };
    Interpreter::update_arithmetic_ic_on_function(unsafe { &*ctx.vm_ctx }, func, ic_idx_raw as u16, &lhs, &rhs, None);
    let interp = unsafe { &*ctx.interpreter };
    let vm_ctx = unsafe { &mut *ctx.vm_ctx };
    interp
        .op_div(vm_ctx, &lhs, &rhs)
        .map(|v| v.to_jit_bits() as i64)
        .unwrap_or(BAILOUT_SENTINEL)
}

/// Generic JS `-` for numeric operands.
#[allow(unsafe_code)]
extern "C" fn otter_rt_generic_sub(_ctx_raw: i64, lhs_raw: i64, rhs_raw: i64) -> i64 {
    match (jit_to_f64(lhs_raw), jit_to_f64(rhs_raw)) {
        (Some(l), Some(r)) => crate::value::Value::number(l - r).to_jit_bits(),
        _ => BAILOUT_SENTINEL,
    }
}

/// Generic JS `*` for numeric operands.
#[allow(unsafe_code)]
extern "C" fn otter_rt_generic_mul(_ctx_raw: i64, lhs_raw: i64, rhs_raw: i64) -> i64 {
    match (jit_to_f64(lhs_raw), jit_to_f64(rhs_raw)) {
        (Some(l), Some(r)) => crate::value::Value::number(l * r).to_jit_bits(),
        _ => BAILOUT_SENTINEL,
    }
}

/// Generic JS `/` for numeric operands.
#[allow(unsafe_code)]
extern "C" fn otter_rt_generic_div(_ctx_raw: i64, lhs_raw: i64, rhs_raw: i64) -> i64 {
    match (jit_to_f64(lhs_raw), jit_to_f64(rhs_raw)) {
        (Some(l), Some(r)) => crate::value::Value::number(l / r).to_jit_bits(),
        _ => BAILOUT_SENTINEL,
    }
}

/// Generic JS `%` for numeric operands.
#[allow(unsafe_code)]
extern "C" fn otter_rt_generic_mod(_ctx_raw: i64, lhs_raw: i64, rhs_raw: i64) -> i64 {
    match (jit_to_f64(lhs_raw), jit_to_f64(rhs_raw)) {
        (Some(l), Some(r)) => crate::value::Value::number(l % r).to_jit_bits(),
        _ => BAILOUT_SENTINEL,
    }
}

/// Generic unary `-` for numeric operands.
/// Signature: `(ctx: i64, val: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_generic_neg(_ctx_raw: i64, val_raw: i64) -> i64 {
    match jit_to_f64(val_raw) {
        Some(n) => crate::value::Value::number(-n).to_jit_bits(),
        None => BAILOUT_SENTINEL,
    }
}

/// Generic `++` (increment) for numeric operands.
#[allow(unsafe_code)]
extern "C" fn otter_rt_generic_inc(_ctx_raw: i64, val_raw: i64) -> i64 {
    match jit_to_f64(val_raw) {
        Some(n) => crate::value::Value::number(n + 1.0).to_jit_bits(),
        None => BAILOUT_SENTINEL,
    }
}

/// Generic `--` (decrement) for numeric operands.
#[allow(unsafe_code)]
extern "C" fn otter_rt_generic_dec(_ctx_raw: i64, val_raw: i64) -> i64 {
    match jit_to_f64(val_raw) {
        Some(n) => crate::value::Value::number(n - 1.0).to_jit_bits(),
        None => BAILOUT_SENTINEL,
    }
}

/// Generic `<` comparison for numeric operands.
/// Returns JS true/false as NaN-boxed values.
#[allow(unsafe_code)]
extern "C" fn otter_rt_generic_lt(_ctx_raw: i64, lhs_raw: i64, rhs_raw: i64) -> i64 {
    match (jit_to_f64(lhs_raw), jit_to_f64(rhs_raw)) {
        (Some(l), Some(r)) => crate::value::Value::boolean(l < r).to_jit_bits(),
        _ => BAILOUT_SENTINEL,
    }
}

/// Generic `<=` comparison for numeric operands.
#[allow(unsafe_code)]
extern "C" fn otter_rt_generic_le(_ctx_raw: i64, lhs_raw: i64, rhs_raw: i64) -> i64 {
    match (jit_to_f64(lhs_raw), jit_to_f64(rhs_raw)) {
        (Some(l), Some(r)) => crate::value::Value::boolean(l <= r).to_jit_bits(),
        _ => BAILOUT_SENTINEL,
    }
}

/// Generic `>` comparison for numeric operands.
#[allow(unsafe_code)]
extern "C" fn otter_rt_generic_gt(_ctx_raw: i64, lhs_raw: i64, rhs_raw: i64) -> i64 {
    match (jit_to_f64(lhs_raw), jit_to_f64(rhs_raw)) {
        (Some(l), Some(r)) => crate::value::Value::boolean(l > r).to_jit_bits(),
        _ => BAILOUT_SENTINEL,
    }
}

/// Generic `>=` comparison for numeric operands.
#[allow(unsafe_code)]
extern "C" fn otter_rt_generic_ge(_ctx_raw: i64, lhs_raw: i64, rhs_raw: i64) -> i64 {
    match (jit_to_f64(lhs_raw), jit_to_f64(rhs_raw)) {
        (Some(l), Some(r)) => crate::value::Value::boolean(l >= r).to_jit_bits(),
        _ => BAILOUT_SENTINEL,
    }
}

/// Generic `==` (abstract equality) for numeric operands.
/// Only handles numeric equality; non-numeric operands bail.
#[allow(unsafe_code)]
extern "C" fn otter_rt_generic_eq(_ctx_raw: i64, lhs_raw: i64, rhs_raw: i64) -> i64 {
    match (jit_to_f64(lhs_raw), jit_to_f64(rhs_raw)) {
        (Some(l), Some(r)) => crate::value::Value::boolean(l == r).to_jit_bits(),
        _ => BAILOUT_SENTINEL,
    }
}

/// Generic `!=` (abstract inequality) for numeric operands.
#[allow(unsafe_code)]
extern "C" fn otter_rt_generic_neq(_ctx_raw: i64, lhs_raw: i64, rhs_raw: i64) -> i64 {
    match (jit_to_f64(lhs_raw), jit_to_f64(rhs_raw)) {
        (Some(l), Some(r)) => crate::value::Value::boolean(l != r).to_jit_bits(),
        _ => BAILOUT_SENTINEL,
    }
}

/// Convert f64 to Int32 per ECMAScript ToInt32 spec.
#[inline]
fn f64_to_int32(n: f64) -> i32 {
    if n.is_nan() || n.is_infinite() || n == 0.0 {
        return 0;
    }
    // Truncate, mod 2^32, interpret as signed
    n as i64 as i32
}

/// Convert f64 to Uint32 per ECMAScript ToUint32 spec.
#[inline]
fn f64_to_uint32(n: f64) -> u32 {
    if n.is_nan() || n.is_infinite() || n == 0.0 {
        return 0;
    }
    n as i64 as u32
}

/// Generic bitwise operation for non-int32 operands.
/// Signature: `(ctx: i64, lhs: i64, rhs: i64, op_id: i64) -> i64`
/// op_id: 0=And, 1=Or, 2=Xor, 3=Shl, 4=Shr, 5=Ushr
#[allow(unsafe_code)]
extern "C" fn otter_rt_generic_bitop(_ctx_raw: i64, lhs_raw: i64, rhs_raw: i64, op_id: i64) -> i64 {
    let lhs_f = match jit_to_f64(lhs_raw) {
        Some(n) => n,
        None => return BAILOUT_SENTINEL,
    };
    let rhs_f = match jit_to_f64(rhs_raw) {
        Some(n) => n,
        None => return BAILOUT_SENTINEL,
    };

    match op_id {
        0 => {
            // And
            let result = f64_to_int32(lhs_f) & f64_to_int32(rhs_f);
            crate::value::Value::int32(result).to_jit_bits()
        }
        1 => {
            // Or
            let result = f64_to_int32(lhs_f) | f64_to_int32(rhs_f);
            crate::value::Value::int32(result).to_jit_bits()
        }
        2 => {
            // Xor
            let result = f64_to_int32(lhs_f) ^ f64_to_int32(rhs_f);
            crate::value::Value::int32(result).to_jit_bits()
        }
        3 => {
            // Shl
            let left = f64_to_int32(lhs_f);
            let shift = f64_to_uint32(rhs_f) & 0x1F;
            crate::value::Value::int32(left << shift).to_jit_bits()
        }
        4 => {
            // Shr (signed)
            let left = f64_to_int32(lhs_f);
            let shift = f64_to_uint32(rhs_f) & 0x1F;
            crate::value::Value::int32(left >> shift).to_jit_bits()
        }
        5 => {
            // Ushr (unsigned) — result is uint32, may not fit in int32
            let left = f64_to_uint32(lhs_f);
            let shift = f64_to_uint32(rhs_f) & 0x1F;
            let result = left >> shift;
            if result <= i32::MAX as u32 {
                crate::value::Value::int32(result as i32).to_jit_bits()
            } else {
                crate::value::Value::number(result as f64).to_jit_bits()
            }
        }
        _ => BAILOUT_SENTINEL,
    }
}

/// Generic bitwise NOT for non-int32 operands.
/// Signature: `(ctx: i64, val: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_generic_bitnot(_ctx_raw: i64, val_raw: i64) -> i64 {
    match jit_to_f64(val_raw) {
        Some(n) => {
            let result = !f64_to_int32(n);
            crate::value::Value::int32(result).to_jit_bits()
        }
        None => BAILOUT_SENTINEL,
    }
}

/// Generic logical NOT.
/// Signature: `(ctx: i64, val: i64) -> i64`
#[allow(unsafe_code)]
extern "C" fn otter_rt_generic_not(_ctx_raw: i64, val_raw: i64) -> i64 {
    let val = match unsafe { crate::value::Value::from_raw_bits_unchecked(val_raw as u64) } {
        Some(v) => v,
        None => return BAILOUT_SENTINEL,
    };
    crate::value::Value::boolean(!val.to_boolean()).to_jit_bits()
}

// ---------------------------------------------------------------------------
// CallFfi — direct FFI call from JIT code
// ---------------------------------------------------------------------------

/// Runtime helper: CallFfi — call an FFI function directly via trampoline.
///
/// Signature: `(ctx: i64, callee: i64, argc: i64, argv_ptr: i64, ffi_call_info_ptr: i64) -> i64`
///
/// The `ffi_call_info_ptr` is a pointer to `FfiCallInfo` that was cached in the
/// feedback vector when the interpreter first saw this FFI call site.
///
/// Guards: verifies callee identity (NaN-boxed bits match). On mismatch, bails out.
#[allow(unsafe_code)]
extern "C" fn otter_rt_call_ffi(
    _ctx_raw: i64,
    callee_raw: i64,
    argc: i64,
    argv_ptr_raw: i64,
    ffi_call_info_ptr: i64,
) -> i64 {
    if ffi_call_info_ptr == 0 {
        return BAILOUT_SENTINEL;
    }

    let ffi_info = unsafe { &*(ffi_call_info_ptr as *const crate::value::FfiCallInfo) };

    // Verify callee identity: if the callee has changed since we cached
    // the FFI info, bail out to let the interpreter handle it.
    let callee_bits = callee_raw as u64;
    if (callee_bits & TAG_MASK) != TAG_PTR_FUNCTION {
        return BAILOUT_SENTINEL;
    }

    // Check that callee is a native function with matching ffi_info pointer.
    // The callee Value is on the stack, so it's rooted and the NativeFunctionObject is alive.
    let raw_ptr = (callee_bits & PAYLOAD_MASK) as *const u8;
    if raw_ptr.is_null() {
        return BAILOUT_SENTINEL;
    }
    let header_offset =
        std::mem::offset_of!(GcAllocation<crate::value::NativeFunctionObject>, value);
    let header_ptr = unsafe { raw_ptr.sub(header_offset) as *const GcHeader };
    let tag = unsafe { (*header_ptr).tag() };
    if tag != gc_tags::FUNCTION {
        return BAILOUT_SENTINEL;
    }
    let nfo = unsafe { &*(raw_ptr as *const crate::value::NativeFunctionObject) };
    let current_ffi_ptr = match &nfo.ffi_info {
        Some(info) => &**info as *const crate::value::FfiCallInfo,
        None => return BAILOUT_SENTINEL,
    };
    if current_ffi_ptr != ffi_call_info_ptr as *const crate::value::FfiCallInfo {
        return BAILOUT_SENTINEL;
    }

    // Call through the trampoline
    let js_args = if argc > 0 && argv_ptr_raw != 0 {
        argv_ptr_raw as *const i64
    } else {
        std::ptr::null()
    };

    unsafe { (ffi_info.trampoline)(ffi_info.opaque, ffi_info.fn_ptr, js_args, argc as u16) }
}

/// Build a `RuntimeHelpers` table with all available helper functions.
/// Compute JsObject field layout offsets for JIT inline property access.
///
/// These offsets are computed empirically because RefCell's internal layout
/// (borrow flag position relative to data) is not guaranteed by Rust.
/// The function creates a temporary JsObject and measures pointer differences.
#[allow(unsafe_code)]
fn compute_jsobject_layout() -> otter_vm_jit::runtime_helpers::JsObjectLayoutOffsets {
    // We can't create a full JsObject here (needs StringTable for Shape::root()).
    // Instead, compute offsets from the struct definition using offset_of! for
    // the ObjectCell fields, then add the RefCell borrow flag size.
    //
    // ObjectCell<T> is a single-field struct wrapping RefCell<T>.
    // RefCell<T> layout: Cell<BorrowFlag=isize>(8 bytes) + UnsafeCell<T>.
    // UnsafeCell<T> is #[repr(transparent)] so it has the same layout as T.
    //
    // We verify this assumption with a static assertion.
    const REFCELL_BORROW_FLAG_SIZE: usize = std::mem::size_of::<isize>();

    // Verify RefCell layout assumption: data follows borrow flag at the expected offset.
    // RefCell<u64>::as_ptr() should return borrow_flag_addr + sizeof(isize).
    let test_cell = std::cell::RefCell::new(42u64);
    let cell_base = &test_cell as *const _ as usize;
    let data_ptr = test_cell.as_ptr() as usize;
    let measured_borrow_size = data_ptr - cell_base;
    assert_eq!(
        measured_borrow_size, REFCELL_BORROW_FLAG_SIZE,
        "RefCell layout assumption violated: borrow flag is {} bytes, expected {}",
        measured_borrow_size, REFCELL_BORROW_FLAG_SIZE
    );

    let inline_slots_field = std::mem::offset_of!(JsObject, inline_slots);
    let inline_meta_field = std::mem::offset_of!(JsObject, inline_meta);

    // Data is at field_offset + sizeof(ObjectCell wrapper=0) + sizeof(RefCell borrow flag)
    // ObjectCell is a single-field struct so it adds 0 to the offset.
    let inline_slots_data = (inline_slots_field + REFCELL_BORROW_FLAG_SIZE) as i32;
    let inline_meta_data = (inline_meta_field + REFCELL_BORROW_FLAG_SIZE) as i32;

    otter_vm_jit::runtime_helpers::JsObjectLayoutOffsets {
        inline_slots_data,
        inline_meta_data,
    }
}

pub fn build_runtime_helpers() -> RuntimeHelpers {
    // Initialize JsObject layout offsets for JIT inline property access.
    let layout = compute_jsobject_layout();
    otter_vm_jit::runtime_helpers::set_jsobject_layout(layout);

    let mut helpers = RuntimeHelpers::new();

    macro_rules! register_helper_1 {
        ($kind:expr, $inner:path) => {{
            extern "C" fn wrapper(a0: i64) -> i64 {
                otter_vm_jit::runtime_helpers::record_helper_call($kind);
                $inner(a0)
            }
            helpers.set($kind, wrapper as *const u8);
        }};
    }

    macro_rules! register_helper_2 {
        ($kind:expr, $inner:path) => {{
            extern "C" fn wrapper(a0: i64, a1: i64) -> i64 {
                otter_vm_jit::runtime_helpers::record_helper_call($kind);
                $inner(a0, a1)
            }
            helpers.set($kind, wrapper as *const u8);
        }};
    }

    macro_rules! register_helper_3 {
        ($kind:expr, $inner:path) => {{
            extern "C" fn wrapper(a0: i64, a1: i64, a2: i64) -> i64 {
                otter_vm_jit::runtime_helpers::record_helper_call($kind);
                $inner(a0, a1, a2)
            }
            helpers.set($kind, wrapper as *const u8);
        }};
    }

    macro_rules! register_helper_4 {
        ($kind:expr, $inner:path) => {{
            extern "C" fn wrapper(a0: i64, a1: i64, a2: i64, a3: i64) -> i64 {
                otter_vm_jit::runtime_helpers::record_helper_call($kind);
                $inner(a0, a1, a2, a3)
            }
            helpers.set($kind, wrapper as *const u8);
        }};
    }

    macro_rules! register_helper_5 {
        ($kind:expr, $inner:path) => {{
            extern "C" fn wrapper(a0: i64, a1: i64, a2: i64, a3: i64, a4: i64) -> i64 {
                otter_vm_jit::runtime_helpers::record_helper_call($kind);
                $inner(a0, a1, a2, a3, a4)
            }
            helpers.set($kind, wrapper as *const u8);
        }};
    }

    macro_rules! register_helper_6 {
        ($kind:expr, $inner:path) => {{
            extern "C" fn wrapper(a0: i64, a1: i64, a2: i64, a3: i64, a4: i64, a5: i64) -> i64 {
                otter_vm_jit::runtime_helpers::record_helper_call($kind);
                $inner(a0, a1, a2, a3, a4, a5)
            }
            helpers.set($kind, wrapper as *const u8);
        }};
    }

    // SAFETY: Function signatures match HelperKind conventions.
    unsafe {
        register_helper_2!(HelperKind::LoadConst, otter_rt_load_const);
        register_helper_4!(HelperKind::GetPropConst, otter_rt_get_prop_const);
        register_helper_5!(HelperKind::SetPropConst, otter_rt_set_prop_const);
        register_helper_4!(HelperKind::CallFunction, otter_rt_call_function_stub);
        register_helper_5!(HelperKind::CallMono, otter_rt_call_mono_stub);
        register_helper_1!(HelperKind::NewObject, otter_rt_new_object);
        register_helper_2!(HelperKind::NewArray, otter_rt_new_array);
        register_helper_3!(HelperKind::GetGlobal, otter_rt_get_global);
        register_helper_5!(HelperKind::SetGlobal, otter_rt_set_global);
        register_helper_2!(HelperKind::GetUpvalue, otter_rt_get_upvalue);
        register_helper_3!(HelperKind::SetUpvalue, otter_rt_set_upvalue);
        register_helper_1!(HelperKind::LoadThis, otter_rt_load_this);
        register_helper_2!(HelperKind::TypeOf, otter_rt_typeof);
        register_helper_2!(HelperKind::TypeOfName, otter_rt_typeof_name);
        register_helper_3!(HelperKind::Pow, otter_rt_pow);
        register_helper_2!(HelperKind::CloseUpvalue, otter_rt_close_upvalue);
        register_helper_4!(HelperKind::GetElem, otter_rt_get_elem);
        register_helper_3!(HelperKind::GetElemInt, otter_rt_get_elem_int);
        register_helper_5!(HelperKind::SetElem, otter_rt_set_elem);
        register_helper_4!(HelperKind::GetProp, otter_rt_get_prop);
        register_helper_5!(HelperKind::SetProp, otter_rt_set_prop);
        register_helper_3!(HelperKind::DeleteProp, otter_rt_delete_prop);
        register_helper_4!(HelperKind::DefineProperty, otter_rt_define_property);
        register_helper_2!(HelperKind::ThrowValue, otter_rt_throw_value);
        register_helper_4!(HelperKind::Construct, otter_rt_construct);
        register_helper_6!(HelperKind::CallMethod, otter_rt_call_method);
        register_helper_5!(HelperKind::CallWithReceiver, otter_rt_call_with_receiver);
        register_helper_6!(
            HelperKind::CallMethodComputed,
            otter_rt_call_method_computed
        );
        register_helper_2!(HelperKind::ToNumber, otter_rt_to_number);
        register_helper_2!(HelperKind::JsToString, otter_rt_to_string);
        register_helper_2!(HelperKind::RequireCoercible, otter_rt_require_coercible);
        register_helper_4!(HelperKind::InstanceOf, otter_rt_instanceof);
        register_helper_4!(HelperKind::InOp, otter_rt_in);
        register_helper_3!(HelperKind::DeclareGlobalVar, otter_rt_declare_global_var);
        register_helper_4!(HelperKind::DefineGetter, otter_rt_define_getter);
        register_helper_4!(HelperKind::DefineSetter, otter_rt_define_setter);
        register_helper_4!(HelperKind::DefineMethod, otter_rt_define_method);
        register_helper_3!(HelperKind::SpreadArray, otter_rt_spread_array);
        register_helper_2!(HelperKind::ClosureCreate, otter_rt_closure_create);
        register_helper_1!(HelperKind::CreateArguments, otter_rt_create_arguments);
        register_helper_2!(HelperKind::GetIterator, otter_rt_get_iterator);
        register_helper_2!(HelperKind::IteratorNext, otter_rt_iterator_next);
        register_helper_2!(HelperKind::IteratorClose, otter_rt_iterator_close);
        register_helper_5!(HelperKind::CallSpread, otter_rt_call_spread);
        register_helper_5!(HelperKind::ConstructSpread, otter_rt_construct_spread);
        register_helper_5!(
            HelperKind::CallMethodComputedSpread,
            otter_rt_call_method_computed_spread
        );
        register_helper_4!(HelperKind::TailCallHelper, otter_rt_tail_call);
        // Real implementations for class/iterator/eval opcodes
        register_helper_1!(HelperKind::GetSuper, otter_rt_get_super);
        register_helper_3!(HelperKind::SetHomeObject, otter_rt_set_home_object);
        register_helper_2!(HelperKind::GetSuperProp, otter_rt_get_super_prop);
        register_helper_4!(HelperKind::DefineClass, otter_rt_define_class);
        register_helper_3!(HelperKind::CallSuper, otter_rt_call_super);
        register_helper_2!(HelperKind::CallSuperSpread, otter_rt_call_super_spread);
        register_helper_2!(HelperKind::GetAsyncIterator, otter_rt_get_async_iterator);
        register_helper_2!(HelperKind::CallEval, otter_rt_call_eval);
        // Real implementations for try/catch, closure variants, and CallSuperForward
        register_helper_2!(HelperKind::TryStart, otter_rt_try_start);
        register_helper_1!(HelperKind::TryEnd, otter_rt_try_end);
        register_helper_1!(HelperKind::CatchOp, otter_rt_catch);
        register_helper_1!(HelperKind::CallSuperForward, otter_rt_call_super_forward);
        register_helper_2!(HelperKind::AsyncClosure, otter_rt_async_closure);
        register_helper_2!(HelperKind::GeneratorClosure, otter_rt_generator_closure);
        register_helper_2!(
            HelperKind::AsyncGeneratorClosure,
            otter_rt_async_generator_closure
        );
        register_helper_2!(HelperKind::ImportOp, otter_rt_import);
        register_helper_3!(HelperKind::ExportOp, otter_rt_export);
        register_helper_2!(HelperKind::ForInNext, otter_rt_for_in_next);

        // Generic arithmetic / comparison helpers (slow path for type guard failure)
        register_helper_3!(HelperKind::GenericAdd, otter_rt_generic_add);
        register_helper_3!(HelperKind::GenericSub, otter_rt_generic_sub);
        register_helper_3!(HelperKind::GenericMul, otter_rt_generic_mul);
        register_helper_3!(HelperKind::GenericDiv, otter_rt_generic_div);
        register_helper_3!(HelperKind::GenericMod, otter_rt_generic_mod);
        register_helper_2!(HelperKind::GenericNeg, otter_rt_generic_neg);
        register_helper_2!(HelperKind::GenericInc, otter_rt_generic_inc);
        register_helper_2!(HelperKind::GenericDec, otter_rt_generic_dec);
        register_helper_3!(HelperKind::GenericLt, otter_rt_generic_lt);
        register_helper_3!(HelperKind::GenericLe, otter_rt_generic_le);
        register_helper_3!(HelperKind::GenericGt, otter_rt_generic_gt);
        register_helper_3!(HelperKind::GenericGe, otter_rt_generic_ge);
        register_helper_3!(HelperKind::GenericEq, otter_rt_generic_eq);
        register_helper_3!(HelperKind::GenericNeq, otter_rt_generic_neq);
        register_helper_4!(HelperKind::GenericBitOp, otter_rt_generic_bitop);
        register_helper_2!(HelperKind::GenericBitNot, otter_rt_generic_bitnot);
        register_helper_2!(HelperKind::GenericNot, otter_rt_generic_not);
        register_helper_4!(HelperKind::ArithAdd, otter_rt_arith_add);
        register_helper_4!(HelperKind::ArithSub, otter_rt_arith_sub);
        register_helper_4!(HelperKind::ArithMul, otter_rt_arith_mul);
        register_helper_4!(HelperKind::ArithDiv, otter_rt_arith_div);
        register_helper_3!(HelperKind::GetPropMono, otter_rt_get_prop_mono_stub);
        register_helper_4!(HelperKind::SetPropMono, otter_rt_set_prop_mono_impl);
        register_helper_3!(HelperKind::GetElemDense, otter_rt_get_elem_dense_impl);
        register_helper_5!(HelperKind::CallFfi, otter_rt_call_ffi);

        register_helper_1!(HelperKind::CheckTierUp, otter_rt_check_tier_up);
        register_helper_2!(HelperKind::ArrayPush, otter_rt_array_push_impl);
        register_helper_1!(HelperKind::ArrayPop, otter_rt_array_pop_impl);

        // GC write barrier for inline property stores
        register_helper_1!(HelperKind::GcWriteBarrier, otter_rt_gc_write_barrier_jit);
        // Primitive toString() (no method resolution)
        register_helper_1!(HelperKind::PrimitiveToString, otter_rt_primitive_to_string);

        // Bail-out stubs: JIT can't suspend (Yield/Await)
        register_helper_1!(HelperKind::YieldOp, otter_rt_bailout_stub);
        register_helper_1!(HelperKind::AwaitOp, otter_rt_bailout_stub);
    }
    helpers
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm_bytecode::{ConstantIndex, Instruction, Module, Register};

    #[test]
    fn set_prop_const_helper_slow_path_survives_proto_epoch_mismatch() {
        let runtime = crate::runtime::VmRuntime::new();
        let _ctx_guard = runtime.create_context();

        let mut builder = Module::builder("jit-set-prop-const-helper.js");
        builder.constants_mut().add_string("x");

        let function = Function::builder()
            .name("main")
            .feedback_vector_size(1)
            .instruction(Instruction::SetPropConst {
                obj: Register(0),
                name: ConstantIndex(0),
                val: Register(1),
                ic_index: 0,
            })
            .instruction(Instruction::ReturnUndefined)
            .build();
        builder.add_function(function);
        let module = builder.build();
        let function = &module.functions[0];

        let obj = GcRef::new(JsObject::new(crate::value::Value::null()));
        obj.set(PropertyKey::string("x"), crate::value::Value::int32(1))
            .expect("object should accept property initialization");

        let mut ctx = JitContext {
            function_ptr: function as *const Function,
            proto_epoch: 1,
            interpreter: std::ptr::null(),
            vm_ctx: std::ptr::null_mut(),
            constants: &module.constants as *const _,
            upvalues_ptr: std::ptr::null(),
            upvalue_count: 0,
            this_raw: crate::value::Value::undefined().to_jit_bits(),
            callee_raw: crate::value::Value::undefined().to_jit_bits(),
            home_object_raw: crate::value::Value::undefined().to_jit_bits(),
            secondary_result: 0,
            bailout_reason: 0,
            bailout_pc: -1,
            deopt_locals_ptr: std::ptr::null_mut(),
            deopt_locals_count: 0,
            deopt_regs_ptr: std::ptr::null_mut(),
            deopt_regs_count: 0,
            osr_entry_pc: -1,
            tier_up_budget: otter_vm_jit::runtime_helpers::JIT_TIER_UP_BUDGET_DEFAULT,
            ic_probes_ptr: std::ptr::null(),
            ic_probes_count: 0,
            interrupt_flag_ptr: std::ptr::null(),
        };

        let result = otter_rt_set_prop_const(
            (&mut ctx as *mut JitContext) as i64,
            crate::value::Value::object(obj).to_jit_bits() as i64,
            0,
            crate::value::Value::int32(42).to_jit_bits() as i64,
            0,
        );

        assert_eq!(
            result, 0,
            "helper should refresh IC via slow path, not bail"
        );
        assert_eq!(
            obj.get(&PropertyKey::string("x")),
            Some(crate::value::Value::int32(42))
        );
        assert!(matches!(
            function.feedback_vector.read()[0].ic_state,
            InlineCacheState::Monomorphic { .. } | InlineCacheState::Polymorphic { .. }
        ));
    }
}
