//! Shared x86-64 receiver proofs for intrinsic-prototype CacheIR loads.
//!
//! # Contents
//! - [`emit`] validates the cell type, declared body predicate and optional
//!   active-realm id, then loads the pinned realm prototype used by Template
//!   property programs.
//!
//! # Invariants
//! - Receiver checks precede every body read; misses have no effects.
//! - The result is a complete tagged cell pointer, never a compressed offset.
//! - Prototype shape, descriptor and slot checks remain subsequent CacheIR ops.
//! - No allocation, runtime call, safepoint or moving address retention occurs.
//!
//! # See also
//! - `otter_vm::jit::JitIntrinsicPrototype` for the shared receiver declaration.

use super::*;

/// Guard `receiver`, then leave the full pinned prototype pointer in `r8`.
/// Clobbers `r8..r11`; the receiver must not use these scratch registers.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    target: otter_vm::jit::JitIntrinsicPrototype,
    byte_pc: u32,
    receiver: u8,
    miss: DynamicLabel,
) {
    if !target.is_generated_receiver() {
        dynasm!(ops ; .arch x64 ; jmp =>miss);
        return;
    }
    emit_load_u64(ops, 10, NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; test Rq(receiver), r10
        ; jnz =>miss
        ; mov r8d, Rd(receiver)
        ; test r8d, r8d
        ; jz =>miss
    );
    emit_load_symbol_u64(
        ops,
        relocations,
        9,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch x64
        ; add r8, r9
        ; cmp BYTE [r8], target.type_tag as i8
        ; jne =>miss
    );
    if let Some(guard) = target.guard {
        emit_body_guard(ops, guard, miss);
    }
    if let Some(realm) = target.active_realm {
        // Both x86-64 tiers keep the native context in `r15`.
        dynasm!(ops
            ; .arch x64
            ; mov r10, [r15 + crate::entry::THREAD_OFFSET as i32]
            ; mov r10, [r10 + crate::entry::VM_THREAD_ACTIVE_REALM_CELL_OFFSET as i32]
            ; test r10, r10
            ; jz =>miss
            ; cmp DWORD [r10], realm as i32
            ; jne =>miss
        );
    }
    emit_load_symbol_u64(
        ops,
        relocations,
        8,
        u64::from(target.proto_offset),
        RelocationTarget::GuardedHeapReference {
            component: crate::artifact::relocation::GuardedHeapComponent::Prototype,
            byte_pc,
            runtime_stub_id: abi::STUB_JIT_LOAD_PROPERTY.id,
        },
    );
    dynasm!(ops ; .arch x64 ; add r8, r9);
}

/// Read one body word while preserving the body pointer for later guards.
/// Narrow compares use the exact u32 bit pattern; a wide compare materializes
/// that same unsigned value rather than sign-extending an imm32.
fn emit_body_guard(ops: &mut Assembler, guard: otter_vm::jit::JitBodyGuard, miss: DynamicLabel) {
    match guard.width {
        otter_vm::jit::JitGuardWidth::Byte => {
            dynasm!(ops ; .arch x64 ; movzx r10d, BYTE [r8 + guard.byte as i32]);
        }
        otter_vm::jit::JitGuardWidth::Word32 => {
            dynasm!(ops ; .arch x64 ; mov r10d, [r8 + guard.byte as i32]);
        }
        otter_vm::jit::JitGuardWidth::Word64 => {
            dynasm!(ops ; .arch x64 ; mov r10, [r8 + guard.byte as i32]);
        }
    }
    if guard.tested_bit().is_some() {
        debug_assert_eq!(guard.expect, 0);
        dynasm!(ops ; .arch x64 ; test r10d, guard.mask as i32);
    } else if guard.expect == 0 {
        dynasm!(ops ; .arch x64 ; test r10, r10);
    } else if guard.width == otter_vm::jit::JitGuardWidth::Word64 {
        dynasm!(ops ; .arch x64 ; mov r11d, guard.expect as i32 ; cmp r10, r11);
    } else {
        dynasm!(ops ; .arch x64 ; cmp r10d, guard.expect as i32);
    }
    dynasm!(ops ; .arch x64 ; jne =>miss);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_word_reads_preserve_unsigned_expectations_and_receiver() {
        use otter_vm::jit::{JitBodyGuard, JitGuardWidth};
        for (width, expected) in [
            (JitGuardWidth::Word32, 0),
            (JitGuardWidth::Word32, u32::MAX),
            (JitGuardWidth::Word64, u32::MAX),
        ] {
            let mut ops = Assembler::new().unwrap();
            let entry = ops.offset();
            let miss = ops.new_dynamic_label();
            let done = ops.new_dynamic_label();
            dynasm!(ops ; .arch x64 ; mov r8, rdi);
            emit_body_guard(
                &mut ops,
                JitBodyGuard {
                    byte: 8,
                    width,
                    expect: expected,
                    mask: JitBodyGuard::WHOLE,
                },
                miss,
            );
            dynasm!(ops ; .arch x64
                ; mov rax, r8 ; jmp =>done
                ; =>miss ; xor eax, eax ; =>done ; ret);
            let code = crate::CompiledCode::new(ops.finalize().unwrap(), entry);
            // SAFETY: the SysV fixture reads one initialized word within `body`.
            let call: extern "sysv64" fn(*const u64) -> usize =
                unsafe { std::mem::transmute(code.entry_ptr()) };
            let mut body = [u64::MAX, u64::from(expected), u64::MAX];
            assert_eq!(
                call(std::hint::black_box(&body).as_ptr()),
                body.as_ptr() as usize
            );
            body[1] ^= 1;
            assert_eq!(call(std::hint::black_box(&body).as_ptr()), 0);
            body[1] = u64::from(expected) | (1 << 32);
            assert_eq!(
                call(std::hint::black_box(&body).as_ptr()),
                if width == JitGuardWidth::Word64 {
                    0
                } else {
                    body.as_ptr() as usize
                },
                "narrow words ignore adjacent state; a complete wide word includes it"
            );
        }
    }

    #[test]
    fn whole_lookup_null_word_rejects_either_compressed_edge() {
        let mut ops = Assembler::new().unwrap();
        let entry = ops.offset();
        let miss = ops.new_dynamic_label();
        let done = ops.new_dynamic_label();
        dynasm!(ops ; .arch x64 ; mov r8, rdi);
        emit_body_guard(
            &mut ops,
            otter_vm::jit::JitBodyGuard::clear(8, otter_vm::jit::JitGuardWidth::Word64),
            miss,
        );
        dynasm!(ops ; .arch x64 ; mov rax, r8 ; jmp =>done
            ; =>miss ; xor eax, eax ; =>done ; ret);
        let code = crate::CompiledCode::new(ops.finalize().unwrap(), entry);
        // SAFETY: the SysV fixture reads one aligned initialized word in `body`.
        let call: extern "sysv64" fn(*const u64) -> usize =
            unsafe { std::mem::transmute(code.entry_ptr()) };
        let mut body = [u64::MAX, 0, u64::MAX];
        assert_eq!(
            call(std::hint::black_box(&body).as_ptr()),
            body.as_ptr() as usize
        );
        for edges in [1, 1 << 31, 1 << 32, 1 << 63, (1 << 32) | 1] {
            body[1] = edges;
            assert_eq!(
                call(std::hint::black_box(&body).as_ptr()),
                0,
                "either compressed edge must make the complete null word miss"
            );
        }
        body[1] = 0;
        assert_eq!(
            call(std::hint::black_box(&body).as_ptr()),
            body.as_ptr() as usize
        );
    }
}
