//! Native forwarding through bounded and runtime-selected ordinary targets.
//!
//! # Contents
//! - Intrinsic-apply/live-argument probes before native target admission.
//! - Bounded exact identity selection and general current-generation dispatch.
//! - Source-materialization exits and committed value completion.
//!
//! # Invariants
//! - x8 retains the probed actual count across identity selection. The shared
//!   plain-call proof uses other scratch registers and saves the count before GC.
//! - An admitted source completes native misses through one pure-value boundary.
//!   A stack-owned source needing materialization exits before the call effect.
//! - Saturation or a bounded miss still permits general native target admission;
//!   both paths share entry cells, native frames, roots and completion contracts.
//!
//! # See also
//! - [`crate::arm64::emit_direct_call_with_access`] — shared call lifecycle.

use std::collections::BTreeMap;

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{JitCompileSnapshot, JitCompilerDiagnostic, native_abi as abi};

use super::{
    calls,
    values::{
        CellTest, emit_cell_test, emit_load_reg, emit_load_runtime_stub, emit_load_u64,
        emit_store_reg,
    },
};
use crate::{
    arm64::{
        DirectCallArguments, DirectCallForm, DirectCallSite, direct_call_artifact,
        emit_direct_call_with_access,
    },
    artifact::{CodeMapCapture, relocation::RelocationCapture},
    entry::{TransitionTable, Unsupported},
};

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_forward_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    view: &JitCompileSnapshot,
    mut events: Option<&mut BTreeMap<(u32, u32), JitCompilerDiagnostic>>,
    mut code_map: Option<&mut CodeMapCapture>,
    logical_pc: u32,
    [dst, method, callee, receiver]: [u16; 4],
    bail: DynamicLabel,
    threw: DynamicLabel,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    let byte_pc = view
        .instructions
        .get(logical_pc as usize)
        .ok_or(Unsupported::OperandShape("forward call instruction PC"))?
        .byte_pc;
    let form = DirectCallForm::CallWithThis {
        callable: callee,
        receiver,
    };
    let site = |target, target_index, target_count| DirectCallSite {
        target,
        target_index,
        target_count,
        caller_function_id: view.code_block.id,
        logical_pc,
        byte_pc,
        dst,
        form,
        arguments: DirectCallArguments::Forward { count: 8 },
    };
    let targets: Vec<_> = view
        .direct_callees
        .get(&byte_pc)
        .into_iter()
        .flatten()
        .collect();
    let cold = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    if targets
        .iter()
        .any(|target| direct_call_artifact(view, site(target, 0, 1)).is_ok())
    {
        emit_load_reg(ops, 1, method)?;
        dynasm!(ops ; .arch aarch64 ; mov x0, x20);
        emit_load_runtime_stub(
            ops,
            relocations,
            16,
            table.entry(abi::STUB_JIT_FORWARD_ARGUMENT_COUNT),
            abi::STUB_JIT_FORWARD_ARGUMENT_COUNT,
        );
        dynasm!(ops ; .arch aarch64 ; blr x16 ; tbnz x0, #63, =>cold ; mov x8, x0);
    }
    for (index, target) in targets.iter().enumerate() {
        if direct_call_artifact(view, site(target, index as u32, targets.len() as u32)).is_err() {
            if let Some(events) = events.as_deref_mut() {
                events.insert(
                    (byte_pc, index as u32),
                    calls::direct_call_lowering_event(
                        otter_vm::JitDirectCallKind::Plain,
                        logical_pc,
                        byte_pc,
                        target,
                        index as u32,
                        targets.len() as u32,
                        otter_vm::JitDirectCallLoweringOutcome::Rejected {
                            reason:
                                otter_vm::JitDirectCallLoweringRejectionReason::LayoutUnsupported,
                        },
                    ),
                );
            }
            continue;
        }
        let next = ops.new_dynamic_label();
        let matched = ops.new_dynamic_label();
        emit_load_reg(ops, 9, callee)?;
        emit_load_u64(
            ops,
            10,
            otter_vm::value::tag::box_function_id(target.plan.function_id),
        );
        dynasm!(ops ; .arch aarch64 ; cmp x9, x10 ; b.eq =>matched ; cbz x9, =>next);
        emit_cell_test(ops, 9, 10, CellTest::IsNotCell, next);
        dynasm!(ops
            ; .arch aarch64
            ; ldrb w10, [x9]
            ; cmp w10, otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG as u32
            ; b.ne =>next
            ; ldr w10, [x9, view.closure_call_layout.function_id_byte]
        );
        emit_load_u64(ops, 11, u64::from(target.plan.function_id));
        dynasm!(ops ; .arch aarch64 ; cmp w10, w11 ; b.ne =>next ; =>matched);
        emit_direct_call_with_access(
            ops,
            relocations,
            view,
            site(target, index as u32, targets.len() as u32),
            table.entry(abi::STUB_JIT_DEOPT_STACK_CALL),
            table.entry(abi::STUB_JIT_RESOLVE_DIRECT_ENTRY),
            0,
            0,
            0,
            0,
            table.entry(abi::STUB_JIT_INITIALIZE_UPVALUES),
            table.entry(abi::STUB_JIT_COPY_FORWARDED_ARGUMENTS),
            code_map.as_deref_mut(),
            cold,
            threw,
            throw_value,
            fatal,
            done,
            20,
            |ops, source, target, _| emit_load_reg(ops, target, source),
            |ops, destination, source, _| emit_store_reg(ops, source, destination),
            |_| Ok(()),
            |_, _, _| Err(Unsupported::OperandShape("forward call construct receiver")),
            |_, _| {
                Err(Unsupported::OperandShape(
                    "forward call post-reservation SSA access",
                ))
            },
            |ops, reg, _| emit_load_reg(ops, 14, reg),
        )?;
        if let Some(events) = events.as_deref_mut() {
            events.insert(
                (byte_pc, index as u32),
                calls::direct_call_lowering_event(
                    otter_vm::JitDirectCallKind::Plain,
                    logical_pc,
                    byte_pc,
                    target,
                    index as u32,
                    targets.len() as u32,
                    otter_vm::JitDirectCallLoweringOutcome::Generated {
                        code_object_id: target.plan.code_object_id,
                        target_tier: calls::direct_call_target_tier(target),
                        this_mode: target.plan.this_mode,
                    },
                ),
            );
        }
        dynasm!(ops ; .arch aarch64 ; =>next);
    }
    dynasm!(ops ; .arch aarch64 ; =>cold);
    let canonical = ops.new_dynamic_label();
    crate::arm64::emit_runtime_forward(
        ops,
        relocations,
        view,
        table,
        [dst, method, callee, receiver],
        logical_pc,
        byte_pc,
        code_map,
        canonical,
        threw,
        throw_value,
        fatal,
        done,
        20,
        |ops, source, target, _| emit_load_reg(ops, target, source),
        |ops, destination, source, _| emit_store_reg(ops, source, destination),
        |_| Ok(()),
        |ops, reg, _| emit_load_reg(ops, 14, reg),
    )?;
    dynasm!(ops ; .arch aarch64 ; =>canonical);
    emit_load_reg(ops, 1, method)?;
    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_FORWARD_SOURCE_READY),
        abi::STUB_JIT_FORWARD_SOURCE_READY,
    );
    dynasm!(ops ; .arch aarch64 ; blr x16 ; cbz x0, =>bail);
    let mut words = vec![method, callee, receiver];
    words.extend(
        view.code_block
            .forwarded_argument_bindings()
            .filter_map(|(_, storage)| match storage {
                otter_bytecode::ArgumentBindingStorage::Register { reg } => Some(reg),
                otter_bytecode::ArgumentBindingStorage::Upvalue { .. } => None,
            }),
    );
    if words.len() > 510 {
        // The canonical frame is reconstructed before the call has effects.
        dynasm!(ops ; .arch aarch64 ; b =>bail);
    } else {
        let words = words
            .into_iter()
            .map(super::value_packet::PacketWord::Register)
            .collect::<Vec<_>>();
        super::value_packet::emit_value_packet_transition(
            ops,
            relocations,
            table,
            abi::STUB_JIT_CALL_FORWARD_ARGUMENTS,
            &words,
            dst,
            throw_value,
            fatal,
        )?;
    }
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}
