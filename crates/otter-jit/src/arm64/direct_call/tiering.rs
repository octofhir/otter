//! Entry accounting and non-overwriting generated tier-up requests.
//!
//! # Contents
//! - Shared entry-counter increment and single pending-request publication.
//!
//! # Invariants
//! - A nested call cannot overwrite an already pending caller request.
//! - Optimizing generations and cached failed compilations do not resubmit.
//! - Cold VM policy controls eligibility; generated code owns no second policy.
//! - x25 selects the entered generation; x16 retains its native entry address.

use super::*;

const ENABLED_OFFSET: u32 =
    std::mem::offset_of!(abi::CodeEntryCell, generated_tiering_enabled) as u32;

pub(super) fn emit_entry(ops: &mut Assembler, context: u8) {
    emit_increment_feedback_u64(ops, CODE_ENTRY_GENERATED_ENTRIES_OFFSET);
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; cmp x14, otter_vm::tier_policy::OPTIMIZING_HOTNESS_THRESHOLD
        ; b.lo =>done
        ; ldr w15, [x25, ENABLED_OFFSET]
        ; cbz w15, =>done
        ; ldr x14, [X(context), THREAD_OFFSET]
        ; ldr x14, [x14, VM_THREAD_CODE_REGISTRY_OFFSET]
        ; ldr x15, [x14, CODE_REGISTRY_VIEW_HOT_FUNCTION_OFFSET]
        ; cbnz x15, =>done
        ; ldr w15, [sp, std::mem::offset_of!(abi::VmFrameHeader, function_id) as u32]
        ; add x15, x15, #1
        ; str x15, [x14, CODE_REGISTRY_VIEW_HOT_FUNCTION_OFFSET]
        ; =>done
    );
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    use super::*;
    use dynasmrt::AssemblyOffset;

    #[test]
    fn pending_caller_survives_nested_hot_entries_and_suppression() {
        let mut ops = Assembler::new().unwrap();
        dynasm!(ops
            ; .arch aarch64
            ; stp x19, x25, [sp, #-16]!
            ; sub sp, sp, #16
            ; str w2, [sp]
            ; mov x25, x0
            ; mov x19, x1
            ; mov x16, #123
        );
        emit_entry(&mut ops, 19);
        dynasm!(ops
            ; .arch aarch64
            ; mov x0, x16
            ; add sp, sp, #16
            ; ldp x19, x25, [sp], #16
            ; ret
        );
        let code = ops.finalize().unwrap();
        // SAFETY: this leaf uses only the initialized frame prefix and the
        // supplied cell/thread fields, preserves callee-saved registers and SP,
        // and retains no pointer after returning.
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
        let cell = abi::CodeEntryCell::new(1, 1, 7, 0, 0, 0, 16);
        cell.generated_entries
            .set(u64::from(otter_vm::tier_policy::OPTIMIZING_HOTNESS_THRESHOLD) - 2);
        assert_eq!(call(&cell, context.as_ptr(), 7), 123);
        assert_eq!(registry.hot_function, 0);
        assert_eq!(call(&cell, context.as_ptr(), 7), 123);
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
        assert_eq!(
            cell.generated_entries.get(),
            u64::from(otter_vm::tier_policy::OPTIMIZING_HOTNESS_THRESHOLD) + 2
        );
    }
}
