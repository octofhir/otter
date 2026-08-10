//! Spread/call-family transition emission.
//!
//! # Contents
//! - Monomorphic spread calls and constructions through shared generated
//!   linkage.
//! - Reentrant completion for sites without a generated target.
//! - Uniform success, throw, and committed caller-handler resumption routing.
//!
//! # Invariants
//! - Every success represents a fully committed opcode and falls through once.
//! - `STATUS_BAILED` carries either the sole pre-effect activation miss or a
//!   caller handler PC already published by the VM; neither path replays an
//!   observable call.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_spread_call_op`

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::native_abi as abi;

use super::{
    calls,
    values::{emit_load_reg, emit_load_runtime_stub, emit_load_u64, emit_store_reg},
};
use crate::{
    arm64::{
        DirectCallArguments, DirectCallForm, DirectCallSite, direct_call_target_is_supported,
        emit_direct_call_with_access,
    },
    artifact::{CodeMapCapture, relocation::RelocationCapture},
    entry::{STATUS_BAILED, STATUS_THREW, Unsupported},
};

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_spread_call_op(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    view: &otter_vm::JitCompileSnapshot,
    direct_call_events: Option<
        &mut std::collections::BTreeMap<(u32, u32), otter_vm::JitCompilerDiagnostic>,
    >,
    code_map: Option<&mut CodeMapCapture>,
    opcode: u8,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    logical_pc: u32,
    byte_pc: u32,
    bail: DynamicLabel,
    threw: DynamicLabel,
) -> Result<(), Unsupported> {
    let lane = |packed: u64, index: usize| ((packed >> (index * 16)) & 0xffff) as u16;
    let direct = if opcode == otter_bytecode::Op::CallSpread as u8 {
        view.direct_callees.get(&byte_pc).map(|target| {
            (
                target,
                DirectCallForm::CallWithThis {
                    callable: lane(arg0, 1),
                    receiver: lane(arg0, 2),
                },
                lane(arg0, 0),
                lane(arg0, 3),
                otter_vm::JitDirectCallKind::Plain,
            )
        })
    } else if opcode == otter_bytecode::Op::NewSpread as u8
        || opcode == otter_bytecode::Op::SuperConstructSpread as u8
    {
        view.direct_constructs.get(&byte_pc).map(|target| {
            let super_construct = opcode == otter_bytecode::Op::SuperConstructSpread as u8;
            let kind = match (super_construct, target.plan.is_derived_constructor) {
                (false, false) => otter_vm::JitDirectCallKind::Construct,
                (false, true) => otter_vm::JitDirectCallKind::DerivedConstruct,
                (true, false) => otter_vm::JitDirectCallKind::SuperConstruct,
                (true, true) => otter_vm::JitDirectCallKind::DerivedSuperConstruct,
            };
            let form = match kind {
                otter_vm::JitDirectCallKind::Construct => DirectCallForm::Construct {
                    callable: arg1 as u16,
                    receiver: arg0 as u16,
                },
                otter_vm::JitDirectCallKind::DerivedConstruct => DirectCallForm::DerivedConstruct {
                    callable: arg1 as u16,
                },
                otter_vm::JitDirectCallKind::SuperConstruct => DirectCallForm::SuperConstruct {
                    callable: arg1 as u16,
                    receiver: arg0 as u16,
                },
                otter_vm::JitDirectCallKind::DerivedSuperConstruct => {
                    DirectCallForm::DerivedSuperConstruct {
                        callable: arg1 as u16,
                    }
                }
                _ => unreachable!("spread construct kind"),
            };
            (target, form, arg0 as u16, arg2 as u16, kind)
        })
    } else {
        None
    };
    if let Some((target, form, dst, arguments, kind)) =
        direct.filter(|(target, ..)| direct_call_target_is_supported(target))
    {
        let done = ops.new_dynamic_label();
        emit_direct_call_with_access(
            ops,
            relocations,
            view,
            DirectCallSite {
                target,
                caller_function_id: view.code_block.id,
                logical_pc,
                byte_pc,
                dst,
                form,
                arguments: DirectCallArguments::Spread(arguments),
            },
            transitions.entry(abi::STUB_JIT_DEOPT_STACK_CALL),
            transitions.entry(abi::STUB_JIT_RESOLVE_DIRECT_ENTRY),
            transitions.entry(abi::STUB_JIT_PREPARE_BASE_CONSTRUCT),
            transitions.entry(abi::STUB_JIT_BASE_CONSTRUCT_RESULT),
            transitions.entry(abi::STUB_JIT_DERIVED_CONSTRUCT_RESULT),
            transitions.entry(abi::STUB_JIT_COPY_SPREAD_ARGUMENTS),
            transitions.entry(abi::STUB_JIT_INITIALIZE_UPVALUES),
            code_map,
            bail,
            threw,
            done,
            20,
            |ops, source, target, _| emit_load_reg(ops, target, source),
            |ops, destination, source, _| emit_store_reg(ops, source, destination),
            |_| Ok(()),
            |ops, source, _| emit_store_reg(ops, source, dst),
            |_, _| Ok(()),
        )?;
        if let Some(events) = direct_call_events {
            events.insert(
                (byte_pc, 0),
                calls::direct_call_lowering_event(
                    kind,
                    logical_pc,
                    byte_pc,
                    target,
                    0,
                    1,
                    otter_vm::JitDirectCallLoweringOutcome::Generated {
                        code_object_id: target.plan.code_object_id,
                        target_tier: calls::direct_call_target_tier(target),
                        this_mode: target.plan.this_mode,
                    },
                ),
            );
        }
        dynasm!(ops ; .arch aarch64 ; =>done);
        return Ok(());
    }

    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
    emit_load_u64(ops, 1, u64::from(opcode));
    emit_load_u64(ops, 2, arg0);
    emit_load_u64(ops, 3, arg1);
    emit_load_u64(ops, 4, arg2);
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        transitions.variadic_entry(abi::STUB_JIT_SPREAD_CALL_OP),
        abi::STUB_JIT_SPREAD_CALL_OP,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbz x0, =>done
        ; cmp x0, STATUS_BAILED as u32
        ; b.eq =>bail
        ; cmp x0, STATUS_THREW as u32
        ; b.eq =>threw
        ; b =>threw
        ; =>done
    );
    Ok(())
}
