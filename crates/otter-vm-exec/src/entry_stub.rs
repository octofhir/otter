//! Low-level entry/deopt bridge for compiled JIT code.
//!
//! This is the runtime boundary between Rust JIT bookkeeping and compiled
//! machine code. The goal is to keep the hot entry path and bailout telemetry
//! handoff in one place so future entry/deopt/OSR trampolines do not sprawl
//! across `jit_runtime.rs`.

use otter_vm_jit::runtime_helpers::{JIT_CTX_BAILOUT_PC_OFFSET, JIT_CTX_BAILOUT_REASON_OFFSET};
use otter_vm_jit::{BAILOUT_SENTINEL, BailoutReason};

#[cfg(not(target_arch = "aarch64"))]
type JitEntry = extern "C" fn(*mut u8, *const i64, u32) -> i64;

const UNKNOWN_BAILOUT_PC_RAW: i64 = -1;

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

#[inline]
#[cfg(not(target_arch = "aarch64"))]
unsafe fn call_jit_entry_rust(
    code_ptr: usize,
    ctx_ptr: *mut u8,
    args_ptr: *const i64,
    argc: u32,
) -> JitEntryOutcome {
    let code: JitEntry = unsafe { std::mem::transmute(code_ptr) };
    let result = code(ctx_ptr, args_ptr, argc);
    if result != BAILOUT_SENTINEL || ctx_ptr.is_null() {
        return make_outcome(
            result,
            BailoutReason::Unknown.code(),
            UNKNOWN_BAILOUT_PC_RAW,
        );
    }

    let raw_reason = unsafe {
        ctx_ptr
            .add(JIT_CTX_BAILOUT_REASON_OFFSET as usize)
            .cast::<i64>()
            .read()
    };
    let raw_pc = unsafe {
        ctx_ptr
            .add(JIT_CTX_BAILOUT_PC_OFFSET as usize)
            .cast::<i64>()
            .read()
    };
    make_outcome(result, raw_reason, raw_pc)
}

#[cfg(target_arch = "aarch64")]
#[inline]
unsafe fn call_jit_entry_asm(
    code_ptr: usize,
    ctx_ptr: *mut u8,
    args_ptr: *const i64,
    argc: u32,
) -> JitEntryOutcome {
    let mut raw_reason = BailoutReason::Unknown.code();
    let mut raw_pc = UNKNOWN_BAILOUT_PC_RAW;
    let mut result: i64;
    unsafe {
        // Use fixed registers so the entry pointer, context, and telemetry
        // spill slots cannot alias with the call ABI registers we rewrite.
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
            in("x10") ctx_ptr,
            in("x1") args_ptr,
            in("w2") argc,
            in("x12") (&mut raw_reason as *mut i64),
            in("x13") (&mut raw_pc as *mut i64),
            in("x14") BAILOUT_SENTINEL,
            lateout("x0") result,
            reason_off = const JIT_CTX_BAILOUT_REASON_OFFSET as usize,
            pc_off = const JIT_CTX_BAILOUT_PC_OFFSET as usize,
            clobber_abi("C"),
        );
    }
    make_outcome(result, raw_reason, raw_pc)
}

#[inline]
pub(crate) unsafe fn call_jit_entry(
    code_ptr: usize,
    ctx_ptr: *mut u8,
    args_ptr: *const i64,
    argc: u32,
) -> JitEntryOutcome {
    #[cfg(target_arch = "aarch64")]
    {
        unsafe { call_jit_entry_asm(code_ptr, ctx_ptr, args_ptr, argc) }
    }

    #[cfg(not(target_arch = "aarch64"))]
    {
        unsafe { call_jit_entry_rust(code_ptr, ctx_ptr, args_ptr, argc) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" fn return_constant(_ctx: *mut u8, args: *const i64, argc: u32) -> i64 {
        let args = unsafe { std::slice::from_raw_parts(args, argc as usize) };
        77 + args.iter().sum::<i64>()
    }

    extern "C" fn bailout_with_telemetry(ctx: *mut u8, _args: *const i64, _argc: u32) -> i64 {
        unsafe {
            ctx.add(JIT_CTX_BAILOUT_REASON_OFFSET as usize)
                .cast::<i64>()
                .write(BailoutReason::TypeGuardFailure.code());
            ctx.add(JIT_CTX_BAILOUT_PC_OFFSET as usize)
                .cast::<i64>()
                .write(42);
        }
        BAILOUT_SENTINEL
    }

    #[test]
    fn call_jit_entry_returns_value_without_bailout_metadata() {
        let args = [3_i64, 4_i64];
        let outcome = unsafe {
            call_jit_entry(
                return_constant as *const () as usize,
                std::ptr::null_mut(),
                args.as_ptr(),
                2,
            )
        };
        assert_eq!(outcome.result, 84);
        assert_eq!(outcome.bailout_reason, BailoutReason::Unknown);
        assert_eq!(outcome.bailout_pc, None);
    }

    #[test]
    fn call_jit_entry_reads_bailout_reason_and_pc() {
        let mut raw = [0_u8; 128];
        let outcome = unsafe {
            call_jit_entry(
                bailout_with_telemetry as *const () as usize,
                raw.as_mut_ptr(),
                std::ptr::null(),
                0,
            )
        };
        assert_eq!(outcome.result, BAILOUT_SENTINEL);
        assert_eq!(outcome.bailout_reason, BailoutReason::TypeGuardFailure);
        assert_eq!(outcome.bailout_pc, Some(42));
    }
}
