//! JitContext — the unified runtime context passed to all JIT-compiled code.
//!
//! This struct is `#[repr(C)]` so that compiled code can load fields at known
//! offsets. Fields are ordered hot-first: registers and constants are accessed
//! on nearly every instruction, while bailout fields are written only on deopt.

/// Opaque context pointer passed as the first argument to every JIT-compiled
/// function: `extern "C" fn(*mut JitContext) -> u64`.
///
/// Constructed by the JIT bridge before calling compiled code. Valid for the
/// duration of one JIT execution (not stored across calls).
#[repr(C)]
pub struct JitContext {
    // ---- Hot: accessed by nearly every compiled instruction ----
    /// Direct pointer into the shared register window (`Vec<Value>`).
    /// Points to `registers[register_base]`.
    pub registers_base: *mut u64,

    /// Number of local variable slots in this frame.
    pub local_count: u32,

    /// Total register count (locals + scratch).
    pub register_count: u32,

    /// Pointer to the module's constant pool.
    pub constants: *const (),

    /// NaN-boxed `this` value (already coerced: sloppy undefined -> globalThis).
    pub this_raw: u64,

    /// Pointer to the interrupt/GC flag. JIT code polls this at back-edges.
    /// Nonzero means bail out to interpreter.
    pub interrupt_flag: *const u8,

    // ---- Warm: needed by runtime helpers ----
    /// Pointer to the Interpreter (for VM reentry calls).
    pub interpreter: *const (),

    /// Pointer to VmContext (for VM reentry calls).
    pub vm_ctx: *mut (),

    /// Reserved function pointer slot in the shared JIT ABI.
    pub function_ptr: *const (),

    /// Pointer to the upvalue cells array (closure captures).
    pub upvalues_ptr: *const (),

    /// Number of upvalue cells.
    pub upvalue_count: u32,

    /// NaN-boxed callee value (for arguments.callee, super calls).
    pub callee_raw: u64,

    /// NaN-boxed home_object (for super property access in class methods).
    pub home_object_raw: u64,

    /// Cached prototype epoch (for IC invalidation guards).
    pub proto_epoch: u64,

    // ---- Cold: written only on bailout ----
    /// Bailout reason code (BailoutReason as u32).
    pub bailout_reason: u32,

    /// Bytecode PC at which the bailout occurred.
    pub bailout_pc: u32,

    /// Secondary return value for multi-result operations
    /// (e.g., IteratorNext done flag).
    pub secondary_result: u64,

    /// Pointer to the active module for specialized tier1 execution.
    pub module_ptr: *const (),

    /// Pointer to the active runtime state for specialized tier1 execution.
    pub runtime_ptr: *mut (),

    /// Pointer to the TypedHeap slots array base.
    pub heap_slots_base: *const (),
}

// Compile-time offset verification. These constants are used by codegen to
// emit loads/stores at known offsets from the JitContext pointer.
macro_rules! assert_offset {
    ($field:ident, $expected:expr) => {
        const _: () = {
            assert!(
                std::mem::offset_of!(JitContext, $field) == $expected,
                concat!("JitContext::", stringify!($field), " offset mismatch"),
            );
        };
    };
}

assert_offset!(registers_base, 0);
assert_offset!(local_count, 8);
assert_offset!(register_count, 12);
assert_offset!(constants, 16);
assert_offset!(this_raw, 24);
assert_offset!(interrupt_flag, 32);
assert_offset!(interpreter, 40);
assert_offset!(vm_ctx, 48);
assert_offset!(function_ptr, 56);
assert_offset!(upvalues_ptr, 64);
assert_offset!(upvalue_count, 72);
assert_offset!(callee_raw, 80);
assert_offset!(home_object_raw, 88);
assert_offset!(proto_epoch, 96);
assert_offset!(bailout_reason, 104);
assert_offset!(bailout_pc, 108);
assert_offset!(secondary_result, 112);
assert_offset!(module_ptr, 120);
assert_offset!(runtime_ptr, 128);
assert_offset!(heap_slots_base, 136);

/// Byte offset constants for use in Cranelift IR codegen.
pub mod offsets {
    pub const REGISTERS_BASE: i32 = 0;
    pub const LOCAL_COUNT: i32 = 8;
    pub const REGISTER_COUNT: i32 = 12;
    pub const CONSTANTS: i32 = 16;
    pub const THIS_RAW: i32 = 24;
    pub const INTERRUPT_FLAG: i32 = 32;
    pub const INTERPRETER: i32 = 40;
    pub const VM_CTX: i32 = 48;
    pub const FUNCTION_PTR: i32 = 56;
    pub const UPVALUES_PTR: i32 = 64;
    pub const UPVALUE_COUNT: i32 = 72;
    pub const CALLEE_RAW: i32 = 80;
    pub const HOME_OBJECT_RAW: i32 = 88;
    pub const PROTO_EPOCH: i32 = 96;
    pub const BAILOUT_REASON: i32 = 104;
    pub const BAILOUT_PC: i32 = 108;
    pub const SECONDARY_RESULT: i32 = 112;
    pub const MODULE_PTR: i32 = 120;
    pub const RUNTIME_PTR: i32 = 128;
    pub const HEAP_SLOTS_BASE: i32 = 136;

    /// Offsets for JsObject memory layout.
    pub mod js_object {
        pub const SHAPE_ID: i32 = 0;
        pub const VALUES_PTR: i32 = 48;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jit_context_size() {
        assert_eq!(std::mem::size_of::<JitContext>(), 144);
    }
}
