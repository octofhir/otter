//! Low-level JIT trampolines and stub glue.
//!
//! This module is the boundary where we start moving hot ABI-sensitive paths
//! out of general Rust helper code and into dedicated stubs. Cranelift still
//! generates whole-function machine code, but entry/call glue can use
//! hand-written assembly where it materially reduces state-shuffle overhead.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::jit_helpers::JitContext;
use crate::value::UpvalueCell;
use otter_vm_bytecode::{ConstantPool, Function};
use otter_vm_jit::{BAILOUT_SENTINEL, BailoutReason};

type JitEntry = extern "C" fn(*mut u8, *const i64, u32) -> i64;
type CallMonoHelper = extern "C" fn(i64, i64, i64, i64, i64) -> i64;
type GetPropMonoHelper = extern "C" fn(i64, i64, i64) -> i64;

const FUNCTION_PTR_OFFSET: usize = std::mem::offset_of!(JitContext, function_ptr);
const CONSTANTS_OFFSET: usize = std::mem::offset_of!(JitContext, constants);
const UPVALUES_PTR_OFFSET: usize = std::mem::offset_of!(JitContext, upvalues_ptr);
const UPVALUE_COUNT_OFFSET: usize = std::mem::offset_of!(JitContext, upvalue_count);
const THIS_RAW_OFFSET: usize = std::mem::offset_of!(JitContext, this_raw);
const CALLEE_RAW_OFFSET: usize = std::mem::offset_of!(JitContext, callee_raw);
const HOME_OBJECT_RAW_OFFSET: usize = std::mem::offset_of!(JitContext, home_object_raw);
const BAILOUT_REASON_OFFSET: usize = std::mem::offset_of!(JitContext, bailout_reason);
const BAILOUT_PC_OFFSET: usize = std::mem::offset_of!(JitContext, bailout_pc);

static CALL_MONO_IC_TARGET: AtomicUsize = AtomicUsize::new(0);
static GET_PROP_MONO_IC_TARGET: AtomicUsize = AtomicUsize::new(0);

#[inline]
fn default_call_mono_ic_target() -> usize {
    crate::jit_helpers::otter_rt_call_mono_impl as *const () as usize
}

#[inline]
fn default_get_prop_mono_ic_target() -> usize {
    crate::jit_helpers::otter_rt_get_prop_mono_impl as *const () as usize
}

#[inline]
fn load_patchable_target(slot: &AtomicUsize, default_target: usize) -> usize {
    let target = slot.load(Ordering::Acquire);
    if target != 0 {
        return target;
    }

    let _ = slot.compare_exchange(0, default_target, Ordering::AcqRel, Ordering::Acquire);
    slot.load(Ordering::Acquire)
}

#[inline]
fn load_call_mono_ic_target() -> usize {
    load_patchable_target(&CALL_MONO_IC_TARGET, default_call_mono_ic_target())
}

#[inline]
fn load_get_prop_mono_ic_target() -> usize {
    load_patchable_target(&GET_PROP_MONO_IC_TARGET, default_get_prop_mono_ic_target())
}

#[cfg(test)]
fn swap_call_mono_ic_target_for_tests(new_target: usize) -> usize {
    let old_target = load_call_mono_ic_target();
    CALL_MONO_IC_TARGET.store(new_target, Ordering::Release);
    old_target
}

#[cfg(test)]
fn swap_get_prop_mono_ic_target_for_tests(new_target: usize) -> usize {
    let old_target = load_get_prop_mono_ic_target();
    GET_PROP_MONO_IC_TARGET.store(new_target, Ordering::Release);
    old_target
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JitEntryOutcome {
    pub(crate) result: i64,
    pub(crate) bailout_reason: BailoutReason,
    pub(crate) bailout_pc: Option<u32>,
}

#[inline]
fn decode_bailout_pc(raw: i64) -> Option<u32> {
    if raw >= 0 && raw <= u32::MAX as i64 {
        Some(raw as u32)
    } else {
        None
    }
}

#[inline]
fn make_outcome(result: i64, raw_reason: i64, raw_pc: i64) -> JitEntryOutcome {
    JitEntryOutcome {
        result,
        bailout_reason: BailoutReason::from_code(raw_reason),
        bailout_pc: decode_bailout_pc(raw_pc),
    }
}

/// Minimal mutable call-frame state that must be swapped when one JIT frame
/// directly tail-calls another JIT frame through the shared `JitContext`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct JitCallReentryState {
    pub function_ptr: *const Function,
    pub constants: *const ConstantPool,
    pub upvalues_ptr: *const UpvalueCell,
    /// Lower 32 bits hold `upvalue_count`; upper 32 bits stay zero.
    pub upvalue_count_and_padding: u64,
    pub this_raw: i64,
    pub callee_raw: i64,
    pub home_object_raw: i64,
}

impl JitCallReentryState {
    pub(crate) fn new(
        function_ptr: *const Function,
        constants: *const ConstantPool,
        upvalues_ptr: *const UpvalueCell,
        upvalue_count: u32,
        this_raw: i64,
        callee_raw: i64,
        home_object_raw: i64,
    ) -> Self {
        Self {
            function_ptr,
            constants,
            upvalues_ptr,
            upvalue_count_and_padding: upvalue_count as u64,
            this_raw,
            callee_raw,
            home_object_raw,
        }
    }
}

#[inline]
unsafe fn call_jit_entry_rust(
    ctx: *mut JitContext,
    args_ptr: *const i64,
    argc: u32,
    code_ptr: usize,
) -> JitEntryOutcome {
    let code: JitEntry = unsafe { std::mem::transmute(code_ptr) };
    let result = code(ctx.cast::<u8>(), args_ptr, argc);
    if result != BAILOUT_SENTINEL || ctx.is_null() {
        return make_outcome(result, BailoutReason::Unknown.code(), -1);
    }

    let raw_reason = unsafe {
        ctx.cast::<u8>()
            .add(BAILOUT_REASON_OFFSET)
            .cast::<i64>()
            .read()
    };
    let raw_pc = unsafe { ctx.cast::<u8>().add(BAILOUT_PC_OFFSET).cast::<i64>().read() };
    make_outcome(result, raw_reason, raw_pc)
}

#[cfg(target_arch = "aarch64")]
#[inline]
unsafe fn call_jit_entry_asm(
    ctx: *mut JitContext,
    args_ptr: *const i64,
    argc: u32,
    code_ptr: usize,
) -> JitEntryOutcome {
    let mut raw_reason = BailoutReason::Unknown.code();
    let mut raw_pc = -1_i64;
    let mut result: i64;
    unsafe {
        std::arch::asm!(
            "sub sp, sp, #32",
            "str x10, [sp, #0]",
            "str x12, [sp, #8]",
            "str x13, [sp, #16]",
            "str x14, [sp, #24]",
            "mov x0, x10",
            "blr x9",
            "ldr x10, [sp, #0]",
            "ldr x12, [sp, #8]",
            "ldr x13, [sp, #16]",
            "ldr x14, [sp, #24]",
            "cmp x0, x14",
            "b.ne 2f",
            "cbz x10, 1f",
            "ldr x11, [x10, #{reason_off}]",
            "str x11, [x12]",
            "ldr x11, [x10, #{pc_off}]",
            "str x11, [x13]",
            "b 2f",
            "1:",
            "mov x11, xzr",
            "str x11, [x12]",
            "mov x11, #-1",
            "str x11, [x13]",
            "2:",
            "add sp, sp, #32",
            in("x9") code_ptr,
            in("x10") ctx,
            in("x1") args_ptr,
            in("w2") argc,
            in("x12") (&mut raw_reason as *mut i64),
            in("x13") (&mut raw_pc as *mut i64),
            in("x14") BAILOUT_SENTINEL,
            lateout("x0") result,
            reason_off = const BAILOUT_REASON_OFFSET,
            pc_off = const BAILOUT_PC_OFFSET,
            clobber_abi("C"),
        );
    }
    make_outcome(result, raw_reason, raw_pc)
}

#[inline]
pub(crate) unsafe fn call_jit_entry(
    ctx: *mut JitContext,
    args_ptr: *const i64,
    argc: u32,
    code_ptr: usize,
) -> JitEntryOutcome {
    #[cfg(target_arch = "aarch64")]
    {
        unsafe { call_jit_entry_asm(ctx, args_ptr, argc, code_ptr) }
    }

    #[cfg(not(target_arch = "aarch64"))]
    {
        unsafe { call_jit_entry_rust(ctx, args_ptr, argc, code_ptr) }
    }
}

#[inline]
unsafe fn dispatch_call_mono_rust(
    target: usize,
    ctx_raw: i64,
    callee_raw: i64,
    argc_raw: i64,
    argv_ptr_raw: i64,
    expected_func_index_raw: i64,
) -> i64 {
    let helper: CallMonoHelper = unsafe { std::mem::transmute(target) };
    helper(
        ctx_raw,
        callee_raw,
        argc_raw,
        argv_ptr_raw,
        expected_func_index_raw,
    )
}

#[cfg(target_arch = "aarch64")]
#[inline]
unsafe fn dispatch_call_mono_asm(
    target: usize,
    ctx_raw: i64,
    callee_raw: i64,
    argc_raw: i64,
    argv_ptr_raw: i64,
    expected_func_index_raw: i64,
) -> i64 {
    let mut result: i64;
    unsafe {
        std::arch::asm!(
            "blr x9",
            in("x0") ctx_raw,
            in("x1") callee_raw,
            in("x2") argc_raw,
            in("x3") argv_ptr_raw,
            in("x4") expected_func_index_raw,
            in("x9") target,
            lateout("x0") result,
            clobber_abi("C"),
        );
    }
    result
}

#[inline]
unsafe fn dispatch_get_prop_mono_rust(
    target: usize,
    obj_raw: i64,
    expected_shape: i64,
    offset: i64,
) -> i64 {
    let helper: GetPropMonoHelper = unsafe { std::mem::transmute(target) };
    helper(obj_raw, expected_shape, offset)
}

#[cfg(target_arch = "aarch64")]
#[inline]
unsafe fn dispatch_get_prop_mono_asm(
    target: usize,
    obj_raw: i64,
    expected_shape: i64,
    offset: i64,
) -> i64 {
    let mut result: i64;
    unsafe {
        std::arch::asm!(
            "blr x9",
            in("x0") obj_raw,
            in("x1") expected_shape,
            in("x2") offset,
            in("x9") target,
            lateout("x0") result,
            clobber_abi("C"),
        );
    }
    result
}

/// Patchable IC stub entry for monomorphic calls.
#[allow(unsafe_code)]
pub(crate) extern "C" fn otter_rt_call_mono_stub(
    ctx_raw: i64,
    callee_raw: i64,
    argc_raw: i64,
    argv_ptr_raw: i64,
    expected_func_index_raw: i64,
) -> i64 {
    let target = load_call_mono_ic_target();

    #[cfg(target_arch = "aarch64")]
    unsafe {
        return dispatch_call_mono_asm(
            target,
            ctx_raw,
            callee_raw,
            argc_raw,
            argv_ptr_raw,
            expected_func_index_raw,
        );
    }

    #[cfg(not(target_arch = "aarch64"))]
    unsafe {
        dispatch_call_mono_rust(
            target,
            ctx_raw,
            callee_raw,
            argc_raw,
            argv_ptr_raw,
            expected_func_index_raw,
        )
    }
}

/// Patchable IC stub entry for monomorphic property reads.
#[allow(unsafe_code)]
pub(crate) extern "C" fn otter_rt_get_prop_mono_stub(
    obj_raw: i64,
    expected_shape: i64,
    offset: i64,
) -> i64 {
    let target = load_get_prop_mono_ic_target();

    #[cfg(target_arch = "aarch64")]
    unsafe {
        return dispatch_get_prop_mono_asm(target, obj_raw, expected_shape, offset);
    }

    #[cfg(not(target_arch = "aarch64"))]
    unsafe {
        dispatch_get_prop_mono_rust(target, obj_raw, expected_shape, offset)
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SavedJitCallFrame {
    function_ptr: *const Function,
    constants: *const ConstantPool,
    upvalues_ptr: *const UpvalueCell,
    upvalue_count_and_padding: u64,
    this_raw: i64,
    callee_raw: i64,
    home_object_raw: i64,
}

#[inline]
unsafe fn call_reentry_stub_rust(
    ctx: *mut JitContext,
    args_ptr: *const i64,
    argc: u32,
    code_ptr: usize,
    state: *const JitCallReentryState,
) -> i64 {
    let ctx = unsafe { &mut *ctx };
    let state = unsafe { &*state };
    let saved = SavedJitCallFrame {
        function_ptr: ctx.function_ptr,
        constants: ctx.constants,
        upvalues_ptr: ctx.upvalues_ptr,
        upvalue_count_and_padding: unsafe {
            (ctx as *mut JitContext)
                .cast::<u8>()
                .add(UPVALUE_COUNT_OFFSET)
                .cast::<u64>()
                .read_unaligned()
        },
        this_raw: ctx.this_raw,
        callee_raw: ctx.callee_raw,
        home_object_raw: ctx.home_object_raw,
    };

    ctx.function_ptr = state.function_ptr;
    ctx.constants = state.constants;
    ctx.upvalues_ptr = state.upvalues_ptr;
    unsafe {
        (ctx as *mut JitContext)
            .cast::<u8>()
            .add(UPVALUE_COUNT_OFFSET)
            .cast::<u64>()
            .write_unaligned(state.upvalue_count_and_padding);
    }
    ctx.this_raw = state.this_raw;
    ctx.callee_raw = state.callee_raw;
    ctx.home_object_raw = state.home_object_raw;

    let code: JitEntry = unsafe { std::mem::transmute(code_ptr) };
    let result = code(ctx as *mut JitContext as *mut u8, args_ptr, argc);

    ctx.function_ptr = saved.function_ptr;
    ctx.constants = saved.constants;
    ctx.upvalues_ptr = saved.upvalues_ptr;
    unsafe {
        (ctx as *mut JitContext)
            .cast::<u8>()
            .add(UPVALUE_COUNT_OFFSET)
            .cast::<u64>()
            .write_unaligned(saved.upvalue_count_and_padding);
    }
    ctx.this_raw = saved.this_raw;
    ctx.callee_raw = saved.callee_raw;
    ctx.home_object_raw = saved.home_object_raw;

    result
}

#[cfg(target_arch = "aarch64")]
#[inline]
unsafe fn call_reentry_stub_asm(
    ctx: *mut JitContext,
    args_ptr: *const i64,
    argc: u32,
    code_ptr: usize,
    state: *const JitCallReentryState,
) -> i64 {
    let mut result: i64;
    unsafe {
        // Keep operands in fixed registers so the compiler cannot co-allocate
        // them with x0/x1/x2 or the branch target and have us overwrite inputs
        // while setting up the callee ABI before `blr`.
        std::arch::asm!(
            "sub sp, sp, #64",
            "str x10, [sp, #56]",
            "ldr x9, [x10, #{function_off}]",
            "str x9, [sp, #0]",
            "ldr x9, [x10, #{constants_off}]",
            "str x9, [sp, #8]",
            "ldr x9, [x10, #{upvalues_ptr_off}]",
            "str x9, [sp, #16]",
            "ldr x9, [x10, #{upvalue_count_off}]",
            "str x9, [sp, #24]",
            "ldr x9, [x10, #{this_off}]",
            "str x9, [sp, #32]",
            "ldr x9, [x10, #{callee_off}]",
            "str x9, [sp, #40]",
            "ldr x9, [x10, #{home_off}]",
            "str x9, [sp, #48]",
            "ldr x9, [x12, #0]",
            "str x9, [x10, #{function_off}]",
            "ldr x9, [x12, #8]",
            "str x9, [x10, #{constants_off}]",
            "ldr x9, [x12, #16]",
            "str x9, [x10, #{upvalues_ptr_off}]",
            "ldr x9, [x12, #24]",
            "str x9, [x10, #{upvalue_count_off}]",
            "ldr x9, [x12, #32]",
            "str x9, [x10, #{this_off}]",
            "ldr x9, [x12, #40]",
            "str x9, [x10, #{callee_off}]",
            "ldr x9, [x12, #48]",
            "str x9, [x10, #{home_off}]",
            "mov x0, x10",
            "blr x11",
            "ldr x10, [sp, #56]",
            "ldr x9, [sp, #0]",
            "str x9, [x10, #{function_off}]",
            "ldr x9, [sp, #8]",
            "str x9, [x10, #{constants_off}]",
            "ldr x9, [sp, #16]",
            "str x9, [x10, #{upvalues_ptr_off}]",
            "ldr x9, [sp, #24]",
            "str x9, [x10, #{upvalue_count_off}]",
            "ldr x9, [sp, #32]",
            "str x9, [x10, #{this_off}]",
            "ldr x9, [sp, #40]",
            "str x9, [x10, #{callee_off}]",
            "ldr x9, [sp, #48]",
            "str x9, [x10, #{home_off}]",
            "add sp, sp, #64",
            in("x10") ctx,
            in("x1") args_ptr,
            in("w2") argc,
            in("x11") code_ptr,
            in("x12") state,
            lateout("x0") result,
            function_off = const FUNCTION_PTR_OFFSET,
            constants_off = const CONSTANTS_OFFSET,
            upvalues_ptr_off = const UPVALUES_PTR_OFFSET,
            upvalue_count_off = const UPVALUE_COUNT_OFFSET,
            this_off = const THIS_RAW_OFFSET,
            callee_off = const CALLEE_RAW_OFFSET,
            home_off = const HOME_OBJECT_RAW_OFFSET,
            clobber_abi("C"),
        );
    }
    result
}

/// Call a compiled JIT entry while temporarily swapping selected `JitContext`
/// fields to the callee frame's state.
#[inline]
pub(crate) unsafe fn call_with_reentry_state(
    ctx: *mut JitContext,
    args_ptr: *const i64,
    argc: u32,
    code_ptr: usize,
    state: &JitCallReentryState,
) -> i64 {
    #[cfg(target_arch = "aarch64")]
    {
        unsafe { call_reentry_stub_asm(ctx, args_ptr, argc, code_ptr, state) }
    }

    #[cfg(not(target_arch = "aarch64"))]
    {
        unsafe { call_reentry_stub_rust(ctx, args_ptr, argc, code_ptr, state) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static IC_STUB_TEST_LOCK: Mutex<()> = Mutex::new(());

    extern "C" fn inspect_reentry_context(ctx_raw: *mut u8, _args: *const i64, _argc: u32) -> i64 {
        let ctx = unsafe { &*(ctx_raw as *const JitContext) };
        ctx.this_raw ^ ctx.callee_raw ^ ctx.home_object_raw
    }

    extern "C" fn inspect_direct_entry(args_ctx: *mut u8, args: *const i64, argc: u32) -> i64 {
        let ctx = unsafe { &*(args_ctx as *const JitContext) };
        let args = unsafe { std::slice::from_raw_parts(args, argc as usize) };
        ctx.this_raw + args.iter().sum::<i64>()
    }

    extern "C" fn trigger_direct_bailout(ctx_raw: *mut u8, _args: *const i64, _argc: u32) -> i64 {
        let ctx = unsafe { &mut *(ctx_raw as *mut JitContext) };
        ctx.bailout_reason = BailoutReason::TypeGuardFailure.code();
        ctx.bailout_pc = 27;
        BAILOUT_SENTINEL
    }

    extern "C" fn inspect_reentry_state_and_args(
        ctx_raw: *mut u8,
        args: *const i64,
        argc: u32,
    ) -> i64 {
        let ctx = unsafe { &*(ctx_raw as *const JitContext) };
        let args = unsafe { std::slice::from_raw_parts(args, argc as usize) };
        (argc as i64 * 1_000)
            + args.iter().sum::<i64>()
            + ctx.upvalue_count as i64
            + ctx.this_raw
            + ctx.callee_raw
            + ctx.home_object_raw
    }

    extern "C" fn patched_call_mono_target(
        ctx_raw: i64,
        callee_raw: i64,
        argc_raw: i64,
        argv_ptr_raw: i64,
        expected_func_index_raw: i64,
    ) -> i64 {
        let args =
            unsafe { std::slice::from_raw_parts(argv_ptr_raw as *const i64, argc_raw as usize) };
        ctx_raw + callee_raw + expected_func_index_raw + args.iter().sum::<i64>()
    }

    extern "C" fn patched_get_prop_mono_target(
        obj_raw: i64,
        expected_shape: i64,
        offset: i64,
    ) -> i64 {
        obj_raw ^ expected_shape ^ offset
    }

    #[test]
    fn call_with_reentry_state_restores_caller_context() {
        let mut ctx = JitContext {
            function_ptr: 0x1111usize as *const Function,
            proto_epoch: 0,
            interpreter: std::ptr::null(),
            vm_ctx: std::ptr::null_mut(),
            constants: 0x2222usize as *const ConstantPool,
            upvalues_ptr: 0x3333usize as *const UpvalueCell,
            upvalue_count: 7,
            this_raw: 10,
            callee_raw: 20,
            home_object_raw: 30,
            secondary_result: 0,
            bailout_reason: 0,
            bailout_pc: 0,
            deopt_locals_ptr: std::ptr::null_mut(),
            deopt_locals_count: 0,
            deopt_regs_ptr: std::ptr::null_mut(),
            deopt_regs_count: 0,
            osr_entry_pc: -1,
        };

        let state = JitCallReentryState::new(
            0xAAAAusize as *const Function,
            0xBBBBusize as *const ConstantPool,
            0xCCCCusize as *const UpvalueCell,
            3,
            100,
            200,
            300,
        );

        let result = unsafe {
            call_with_reentry_state(
                &mut ctx,
                std::ptr::null(),
                0,
                inspect_reentry_context as usize,
                &state,
            )
        };

        assert_eq!(result, 100 ^ 200 ^ 300);
        assert_eq!(ctx.function_ptr, 0x1111usize as *const Function);
        assert_eq!(ctx.constants, 0x2222usize as *const ConstantPool);
        assert_eq!(ctx.upvalues_ptr, 0x3333usize as *const UpvalueCell);
        assert_eq!(ctx.upvalue_count, 7);
        assert_eq!(ctx.this_raw, 10);
        assert_eq!(ctx.callee_raw, 20);
        assert_eq!(ctx.home_object_raw, 30);
    }

    #[test]
    fn call_with_reentry_state_passes_args_and_installs_full_state() {
        let mut ctx = JitContext {
            function_ptr: std::ptr::null(),
            proto_epoch: 0,
            interpreter: std::ptr::null(),
            vm_ctx: std::ptr::null_mut(),
            constants: std::ptr::null(),
            upvalues_ptr: std::ptr::null(),
            upvalue_count: 0,
            this_raw: 1,
            callee_raw: 2,
            home_object_raw: 3,
            secondary_result: 0,
            bailout_reason: 0,
            bailout_pc: 0,
            deopt_locals_ptr: std::ptr::null_mut(),
            deopt_locals_count: 0,
            deopt_regs_ptr: std::ptr::null_mut(),
            deopt_regs_count: 0,
            osr_entry_pc: -1,
        };

        let state = JitCallReentryState::new(
            0xAAAAusize as *const Function,
            0xBBBBusize as *const ConstantPool,
            0xCCCCusize as *const UpvalueCell,
            9,
            100,
            200,
            300,
        );
        let args = [7_i64, 11_i64, 13_i64];

        let result = unsafe {
            call_with_reentry_state(
                &mut ctx,
                args.as_ptr(),
                args.len() as u32,
                inspect_reentry_state_and_args as usize,
                &state,
            )
        };

        assert_eq!(result, 3_000 + 31 + 9 + 100 + 200 + 300);
        assert_eq!(ctx.upvalue_count, 0);
        assert_eq!(ctx.this_raw, 1);
        assert_eq!(ctx.callee_raw, 2);
        assert_eq!(ctx.home_object_raw, 3);
    }

    #[test]
    fn call_jit_entry_returns_value_without_bailout_metadata() {
        let mut ctx = JitContext {
            function_ptr: std::ptr::null(),
            proto_epoch: 0,
            interpreter: std::ptr::null(),
            vm_ctx: std::ptr::null_mut(),
            constants: std::ptr::null(),
            upvalues_ptr: std::ptr::null(),
            upvalue_count: 0,
            this_raw: 9,
            callee_raw: 0,
            home_object_raw: 0,
            secondary_result: 0,
            bailout_reason: BailoutReason::Unknown.code(),
            bailout_pc: -1,
            deopt_locals_ptr: std::ptr::null_mut(),
            deopt_locals_count: 0,
            deopt_regs_ptr: std::ptr::null_mut(),
            deopt_regs_count: 0,
            osr_entry_pc: -1,
        };
        let args = [5_i64, 6_i64];

        let outcome = unsafe {
            call_jit_entry(
                &mut ctx,
                args.as_ptr(),
                args.len() as u32,
                inspect_direct_entry as *const () as usize,
            )
        };

        assert_eq!(outcome.result, 20);
        assert_eq!(outcome.bailout_reason, BailoutReason::Unknown);
        assert_eq!(outcome.bailout_pc, None);
    }

    #[test]
    fn call_jit_entry_reads_bailout_reason_and_pc() {
        let mut ctx = JitContext {
            function_ptr: std::ptr::null(),
            proto_epoch: 0,
            interpreter: std::ptr::null(),
            vm_ctx: std::ptr::null_mut(),
            constants: std::ptr::null(),
            upvalues_ptr: std::ptr::null(),
            upvalue_count: 0,
            this_raw: 0,
            callee_raw: 0,
            home_object_raw: 0,
            secondary_result: 0,
            bailout_reason: BailoutReason::Unknown.code(),
            bailout_pc: -1,
            deopt_locals_ptr: std::ptr::null_mut(),
            deopt_locals_count: 0,
            deopt_regs_ptr: std::ptr::null_mut(),
            deopt_regs_count: 0,
            osr_entry_pc: -1,
        };

        let outcome = unsafe {
            call_jit_entry(
                &mut ctx,
                std::ptr::null(),
                0,
                trigger_direct_bailout as *const () as usize,
            )
        };

        assert_eq!(outcome.result, BAILOUT_SENTINEL);
        assert_eq!(outcome.bailout_reason, BailoutReason::TypeGuardFailure);
        assert_eq!(outcome.bailout_pc, Some(27));
    }

    #[test]
    fn call_mono_ic_stub_dispatches_via_patchable_target() {
        let _guard = IC_STUB_TEST_LOCK.lock().unwrap();
        let patched = patched_call_mono_target as *const () as usize;
        let original = swap_call_mono_ic_target_for_tests(patched);
        let args = [5_i64, 7_i64];
        let result = otter_rt_call_mono_stub(11, 13, 2, args.as_ptr() as i64, 17);
        CALL_MONO_IC_TARGET.store(original, Ordering::Release);

        assert_eq!(result, 11 + 13 + 17 + 5 + 7);
    }

    #[test]
    fn get_prop_mono_ic_stub_dispatches_via_patchable_target() {
        let _guard = IC_STUB_TEST_LOCK.lock().unwrap();
        let patched = patched_get_prop_mono_target as *const () as usize;
        let original = swap_get_prop_mono_ic_target_for_tests(patched);
        let result = otter_rt_get_prop_mono_stub(0x10, 0x22, 0x34);
        GET_PROP_MONO_IC_TARGET.store(original, Ordering::Release);

        assert_eq!(result, 0x10 ^ 0x22 ^ 0x34);
    }
}
