//! Emitted intrinsic identity guards on both machine targets.
//!
//! # Contents
//! - One executable primitive-operation harness over a held identity word.
//!
//! # Invariants
//! - Misses preserve the destination and every evaluated operand.
//! - Updating the held word changes admission without recompiling machine code.
//! - The harness preserves platform callee-saved registers and retains its code.
//!
//! # See also
//! - The parent target's intrinsic emitter and `crate::intrinsic_calls`.

use super::*;
use crate::entry::{VALUE_HOLE, VALUE_TRUE};
use dynasmrt::DynasmApi;
use otter_vm::{IntrinsicCallKind, JitIntrinsicCallProof, Value};

fn primitive_program(
    identity: *const u64,
    form: IntrinsicCallForm,
    argument: Option<u16>,
) -> (dynasmrt::ExecutableBuffer, dynasmrt::AssemblyOffset) {
    let view = JitCompileSnapshot::without_feedback(0, 0, 4, Vec::new());
    let transitions = crate::entry::TransitionTable::resolve();
    let mut relocations = RelocationCapture::default();
    let mut ops = Assembler::new().expect("assembler");
    let entry = ops.offset();
    let miss = ops.new_dynamic_label();
    let identity_miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    #[cfg(target_arch = "aarch64")]
    dynasm!(ops ; .arch aarch64 ; stp x19, x30, [sp, #-16]! ; mov x19, x0);
    #[cfg(target_arch = "x86_64")]
    {
        dynasm!(ops ; .arch x64 ; push r13);
        #[cfg(not(target_os = "windows"))]
        dynasm!(ops ; .arch x64 ; mov r13, rdi);
        #[cfg(target_os = "windows")]
        dynasm!(ops ; .arch x64 ; mov r13, rcx);
    }
    let instruction = TemplateInstr {
        pc: 3,
        byte_pc: 21,
        op: TemplateOp::IntrinsicCall {
            dst: 0,
            callee: 1,
            argument,
            proof: JitIntrinsicCallProof {
                kind: if form == IntrinsicCallForm::NumberZero {
                    IntrinsicCallKind::Number
                } else {
                    IntrinsicCallKind::Boolean
                },
                identity_cell: identity as u64,
                realm_id: view.literal_allocations.realm_id,
            },
            form,
        },
    };
    let exits = OperationExits {
        type_mismatch_exit: miss,
        identity_guard_exit: identity_miss,
        allocation_miss_exit: miss,
        unsupported_exit: miss,
        runtime_transition_exit: miss,
        backedge_relink_exit: miss,
        #[cfg(target_arch = "aarch64")]
        bail: miss,
        returned: miss,
        committed_throw: miss,
        threw: miss,
        #[cfg(target_arch = "aarch64")]
        propagate_throw: miss,
        fatal: miss,
    };
    emit_intrinsic_call(
        &mut ops,
        &mut relocations,
        &transitions,
        &view,
        &instruction,
        AllocPacket {
            safepoint: abi::NO_SAFEPOINT,
            tagged_slots: 0,
        },
        exits,
    )
    .expect("primitive emitter");
    #[cfg(target_arch = "aarch64")]
    {
        dynasm!(ops ; .arch aarch64 ; ldr x0, [x19] ; b =>done ; =>miss);
        emit_load_u64(&mut ops, 0, Value::undefined().to_bits());
        dynasm!(ops ; .arch aarch64 ; b =>done ; =>identity_miss);
        emit_load_u64(&mut ops, 0, VALUE_HOLE);
        dynasm!(ops ; .arch aarch64 ; =>done ; ldp x19, x30, [sp], #16 ; ret);
    }
    #[cfg(target_arch = "x86_64")]
    {
        dynasm!(ops ; .arch x64 ; mov rax, [r13] ; jmp =>done ; =>miss);
        emit_load_u64(&mut ops, 0, Value::undefined().to_bits());
        dynasm!(ops ; .arch x64 ; jmp =>done ; =>identity_miss);
        emit_load_u64(&mut ops, 0, VALUE_HOLE);
        dynasm!(ops ; .arch x64 ; =>done ; pop r13 ; ret);
    }
    (ops.finalize().expect("finalize"), entry)
}

#[test]
fn held_identity_is_read_live_and_misses_preserve_evaluated_operands() {
    let old_identity = Value::number_i32(17).to_bits();
    let moved_identity = Value::number_i32(29).to_bits();
    let mut identity = Box::new(old_identity);
    let sentinel = Value::number_i32(-71).to_bits();
    let operand = Value::number_i32(9).to_bits();
    for (form, argument, expected) in [
        (
            IntrinsicCallForm::NumberZero,
            None,
            Value::number_i32(0).to_bits(),
        ),
        (IntrinsicCallForm::Boolean, None, VALUE_FALSE),
        (IntrinsicCallForm::Boolean, Some(2), VALUE_TRUE),
    ] {
        *identity = old_identity;
        let (code, entry) = primitive_program(&*identity, form, argument);
        // SAFETY: the emitted primitive harness matches this C signature,
        // preserves callee-saved registers, and owns no collecting boundary.
        // Code, the identity word and the window outlive every invocation.
        let run: extern "C" fn(*mut u64) -> u64 = unsafe { std::mem::transmute(code.ptr(entry)) };
        let mut window = [
            sentinel,
            old_identity,
            operand,
            Value::undefined().to_bits(),
        ];
        assert_eq!(run(window.as_mut_ptr()), expected);
        assert_eq!(
            &window[1..],
            &[old_identity, operand, Value::undefined().to_bits()]
        );
        *identity = moved_identity;
        window[0] = sentinel;
        assert_eq!(run(window.as_mut_ptr()), VALUE_HOLE);
        assert_eq!(
            window,
            [
                sentinel,
                old_identity,
                operand,
                Value::undefined().to_bits()
            ]
        );
        window[1] = moved_identity;
        assert_eq!(run(window.as_mut_ptr()), expected);
        assert_eq!(
            &window[1..],
            &[moved_identity, operand, Value::undefined().to_bits()]
        );
    }
}
