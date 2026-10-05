//! Native parked-pair lifetime and stack geometry for terminal leaves.
//!
//! # Contents
//! - Current target C entry receives the exact sole pair pointer.
//! - Pair bytes survive platform normalization, and caller SP restores exactly.
//!
//! # Invariants
//! This executable private-memory test never enters GC or JS. The actual
//! terminal entry is VM-owned and separately proves canonical ticket semantics.
//! A Microsoft leaf consumes its existing hidden pair result; no extra carrier
//! exists. Architecture wrappers preserve host nonvolatiles before returning.
//!
//! # See also
//! - `x86_64::call_abi` owns Microsoft shadow/result adaptation.
//! - `otter_vm::constructor_layout::completion` owns terminal sampling.

use crate::CompiledCode;
use dynasmrt::{DynasmApi, dynasm};
use otter_vm::{
    Value,
    native_abi::{CallRequest, JitCtx, NativeResultPair},
};

const STACK: i32 = std::mem::offset_of!(JitCtx, native_stack_limit) as i32;
const POINTER: i32 = std::mem::offset_of!(JitCtx, generated_depth_limit) as i32;
const BEFORE: i32 = std::mem::offset_of!(JitCtx, generated_feedback_clean) as i32;
const AFTER: i32 =
    (std::mem::offset_of!(JitCtx, pending_call) + std::mem::offset_of!(CallRequest, entry)) as i32;

#[test]
fn terminal_pair_pointer_remains_aligned_and_native_stack_restores_exactly() {
    for pair in [
        NativeResultPair::success(Value::number_i32(1234)),
        NativeResultPair::throw_value(Value::number_i32(-55)),
        NativeResultPair::fatal_internal(),
        NativeResultPair::side_exit(otter_vm::native_abi::SideExit::new(
            3,
            otter_vm::native_abi::ExitReason::AllocationMiss,
            otter_vm::native_abi::ExitAction::Resume,
        )),
    ] {
        execute(pair);
    }
}

#[cfg(target_arch = "x86_64")]
fn execute(pair: NativeResultPair) {
    use dynasmrt::x64::Assembler;
    use otter_vm::native_abi::STUB_JIT_CONSTRUCTOR_TERMINAL;
    let mut leaf = Assembler::new().unwrap();
    let entry = leaf.offset();
    if cfg!(target_os = "windows") {
        dynasm!(leaf ; .arch x64 ; mov r10, rdx ; mov r11, r8);
    } else {
        dynasm!(leaf ; .arch x64 ; mov r10, rdi ; mov r11, rsi);
    }
    dynasm!(leaf ; .arch x64
        ; mov rax, rsp ; and rax, 15 ; mov [r10 + STACK], rax
        ; mov rax, r11 ; and rax, 15 ; mov [r10 + POINTER], rax
    );
    if cfg!(target_os = "windows") {
        dynasm!(leaf ; .arch x64
            ; mov rax, [r11] ; mov [rcx], rax
            ; mov rax, [r11 + 8] ; mov [rcx + 8], rax
            ; mov rax, rcx ; ret
        );
    } else {
        dynasm!(leaf ; .arch x64 ; mov rax, [r11] ; mov rdx, [r11 + 8] ; ret);
    }
    let leaf = CompiledCode::new(leaf.finalize().unwrap(), entry);
    let mut wrapper = Assembler::new().unwrap();
    let entry = wrapper.offset();
    dynasm!(wrapper ; .arch x64
        ; push r15 ; mov r15, rdi ; mov r11, rsi
        ; mov [r15 + BEFORE], rsp ; sub rsp, 16
    );
    // SAFETY: the one repr(C) pair is two initialized complete machine words.
    let words: [u64; 2] = unsafe { std::mem::transmute(pair) };
    dynasm!(wrapper ; .arch x64
        ; mov rax, QWORD words[0] as i64 ; mov [rsp], rax
        ; mov rax, QWORD words[1] as i64 ; mov [rsp + 8], rax
        ; mov rdi, r15 ; mov rsi, rsp
    );
    crate::x86_64::call_abi::emit_runtime_call(&mut wrapper, STUB_JIT_CONSTRUCTOR_TERMINAL);
    dynasm!(wrapper ; .arch x64
        ; add rsp, 16 ; mov [r15 + AFTER], rsp ; pop r15 ; ret
    );
    let wrapper = CompiledCode::new(wrapper.finalize().unwrap(), entry);
    let mut context = empty_context();
    // SAFETY: this wrapper follows private System V arguments/results even
    // on Windows. Its one C crossing above follows the actual target platform.
    let call: extern "sysv64" fn(*mut JitCtx, usize) -> NativeResultPair =
        unsafe { std::mem::transmute(wrapper.entry_ptr()) };
    // SAFETY: the finalized leaf remains alive through this private C call.
    let leaf_entry = unsafe { leaf.entry_ptr() } as usize;
    assert_eq!(call(&mut context, leaf_entry), pair);
    assert_eq!(
        context.native_stack_limit, 8,
        "C entry RSP is return-address aligned"
    );
    assert_eq!(
        context.generated_depth_limit, 0,
        "parked pair is 16-byte aligned"
    );
    assert_eq!(
        context.generated_feedback_clean, context.pending_call.entry as u64,
        "exact caller SP restore"
    );
}

#[cfg(target_arch = "aarch64")]
fn execute(pair: NativeResultPair) {
    use dynasmrt::aarch64::Assembler;
    let mut leaf = Assembler::new().unwrap();
    let entry = leaf.offset();
    dynasm!(leaf ; .arch aarch64
        ; mov x10, x0 ; mov x11, x1
        ; mov x9, sp ; and x9, x9, 15 ; str x9, [x10, STACK as u32]
        ; and x9, x11, 15 ; str x9, [x10, POINTER as u32]
        ; ldp x0, x1, [x11] ; ret
    );
    let leaf = CompiledCode::new(leaf.finalize().unwrap(), entry);
    let mut wrapper = Assembler::new().unwrap();
    let entry = wrapper.offset();
    dynasm!(wrapper ; .arch aarch64
        ; stp x19, x30, [sp, -16]! ; mov x19, x0 ; mov x16, x1
        ; mov x9, sp ; str x9, [x19, BEFORE as u32]
        ; sub sp, sp, 16
    );
    // SAFETY: the one repr(C) pair is two initialized complete machine words.
    let words: [u64; 2] = unsafe { std::mem::transmute(pair) };
    crate::template::arm64::values::emit_load_u64(&mut wrapper, 9, words[0]);
    crate::template::arm64::values::emit_load_u64(&mut wrapper, 10, words[1]);
    dynasm!(wrapper ; .arch aarch64
        ; stp x9, x10, [sp] ; mov x1, sp ; mov x0, x19 ; blr x16
        ; add sp, sp, 16 ; mov x9, sp ; str x9, [x19, AFTER as u32]
        ; ldp x19, x30, [sp], 16 ; ret
    );
    let wrapper = CompiledCode::new(wrapper.finalize().unwrap(), entry);
    let mut context = empty_context();
    // SAFETY: this wrapper and leaf follow the target C convention and retain
    // these exact live private host records; neither can allocate or collect.
    let call: extern "C" fn(*mut JitCtx, usize) -> NativeResultPair =
        unsafe { std::mem::transmute(wrapper.entry_ptr()) };
    // SAFETY: the finalized leaf remains alive through this private C call.
    let leaf_entry = unsafe { leaf.entry_ptr() } as usize;
    assert_eq!(call(&mut context, leaf_entry), pair);
    assert_eq!(
        context.native_stack_limit, 0,
        "C entry SP remains 16-byte aligned"
    );
    assert_eq!(
        context.generated_depth_limit, 0,
        "parked pair is 16-byte aligned"
    );
    assert_eq!(
        context.generated_feedback_clean, context.pending_call.entry as u64,
        "exact caller SP restore"
    );
}

fn empty_context() -> JitCtx {
    JitCtx {
        thread: std::ptr::null_mut(),
        native_frame: std::ptr::null_mut(),
        error: std::ptr::null_mut(),
        generated_depth_limit: u64::MAX,
        global_this_offset: std::ptr::null(),
        native_stack_limit: usize::MAX,
        generated_feedback_clean: 1,
        alloc_window: otter_vm::jit::JitMachineAllocationWindow::disabled(),
        runtime_stats: std::ptr::null_mut(),
        pending_call: CallRequest::EMPTY,
        completion: NativeResultPair::success(Value::UNDEFINED),
        completion_destination: u32::MAX,
        completion_generation: 0,
    }
}
