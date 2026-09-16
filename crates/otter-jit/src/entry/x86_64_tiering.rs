//! System V x86-64 generated-entry accounting and tier-up publication.
//!
//! # Contents
//! - One straight-line generated-entry counter update.
//! - Non-overwriting publication of the first hot callee to the registry.
//!
//! # Invariants
//! - `r12` identifies the entered code generation and `r15` retains `JitCtx`.
//! - The stack pointer addresses the initialized callee `NativeFrame`.
//! - Nested calls never overwrite an already pending outer tier-up request.
//!
//! # See also
//! - `crate::arm64::direct_call::tiering` for the peer target encoding.

use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::native_abi as abi;

use super::{
    CODE_ENTRY_GENERATED_ENTRIES_OFFSET, CODE_REGISTRY_VIEW_HOT_FUNCTION_OFFSET, THREAD_OFFSET,
    VM_THREAD_CODE_REGISTRY_OFFSET,
};

const ENABLED_OFFSET: u32 =
    std::mem::offset_of!(abi::CodeEntryCell, generated_tiering_enabled) as u32;
const BREAK_EVEN_OFFSET: u32 =
    std::mem::offset_of!(abi::CodeEntryCell, generated_tiering_break_even) as u32;

pub(crate) fn emit_entry(ops: &mut Assembler) {
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; mov r10, [r12 + CODE_ENTRY_GENERATED_ENTRIES_OFFSET as i32]
        ; add r10, 1
        ; mov [r12 + CODE_ENTRY_GENERATED_ENTRIES_OFFSET as i32], r10
        ; cmp r10, [r12 + BREAK_EVEN_OFFSET as i32]
        ; jb =>done
        ; cmp DWORD [r12 + ENABLED_OFFSET as i32], 0
        ; je =>done
        ; mov r10, [r15 + THREAD_OFFSET as i32]
        ; mov r10, [r10 + VM_THREAD_CODE_REGISTRY_OFFSET as i32]
        ; cmp QWORD [r10 + CODE_REGISTRY_VIEW_HOT_FUNCTION_OFFSET as i32], 0
        ; jne =>done
        ; mov r11d, [rsp + std::mem::offset_of!(abi::VmFrameHeader, function_id) as i32]
        ; add r11, 1
        ; mov [r10 + CODE_REGISTRY_VIEW_HOT_FUNCTION_OFFSET as i32], r11
        ; =>done
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use dynasmrt::AssemblyOffset;

    #[test]
    fn pending_caller_survives_nested_hot_entries_and_suppression() {
        let mut ops = Assembler::new().unwrap();
        dynasm!(ops
            ; .arch x64
            ; push rbp
            ; mov rbp, rsp
            ; push r12
            ; push r15
            ; sub rsp, 16
            ; mov r12, rdi
            ; mov r15, rsi
            ; mov [rsp + std::mem::offset_of!(abi::VmFrameHeader, function_id) as i32], edx
        );
        emit_entry(&mut ops);
        dynasm!(ops
            ; .arch x64
            ; mov rax, [r12 + CODE_ENTRY_GENERATED_ENTRIES_OFFSET as i32]
            ; add rsp, 16
            ; pop r15
            ; pop r12
            ; pop rbp
            ; ret
        );
        let code = ops.finalize().unwrap();
        // SAFETY: the generated leaf preserves every callee-saved register,
        // reads only the supplied fixed-layout records, and retains no pointer.
        let call: extern "C" fn(*const abi::CodeEntryCell, *const u64, u32) -> u64 =
            unsafe { std::mem::transmute(code.ptr(AssemblyOffset(0))) };
        let mut registry = abi::CodeRegistryView {
            context: 0,
            resolve_safepoint: 0,
            hot_function: 0,
        };
        let mut thread = abi::VmThread::empty();
        thread.code_registry = std::ptr::from_mut(&mut registry) as u64;
        let mut context = vec![0_u64; THREAD_OFFSET as usize / 8 + 1];
        context[THREAD_OFFSET as usize / 8] = std::ptr::from_ref(&thread) as u64;
        let cell = abi::CodeEntryCell::new(1, 1, 7, 0, 0, 0, 16, 5);
        cell.generated_entries.set(3);
        assert_eq!(call(&cell, context.as_ptr(), 7), 4);
        assert_eq!(registry.hot_function, 0);
        assert_eq!(call(&cell, context.as_ptr(), 7), 5);
        assert_eq!(registry.hot_function, 8);
        call(&cell, context.as_ptr(), 9);
        assert_eq!(
            registry.hot_function, 8,
            "nested callee must not overwrite caller"
        );
        registry.hot_function = 0;
        cell.generated_tiering_enabled.set(0);
        call(&cell, context.as_ptr(), 7);
        assert_eq!(registry.hot_function, 0, "cached decline must not resubmit");
        assert_eq!(cell.generated_entries.get(), 7);
    }
}
