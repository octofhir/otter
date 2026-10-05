//! Actual operation-encoder proof for omitted whole-function cold targets.
//!
//! # Contents
//! - Every narrowed control/pure operation finalizes with only its declared
//!   external exit labels bound.
//!
//! # Invariants
//! - Omitted exits remain unresolved: any emitted reference makes assembler
//!   finalization fail. This checks production emission, not a mirror mask.
//! - The mapping is never executed; semantic, root and pair behavior remains
//!   covered by the native Template and frame suites.

use super::*;
use crate::template::{operation::OperationExits, plan::TemplateInstr};
use dynasmrt::{DynasmLabelApi, dynasm, x64::Assembler};
use otter_bytecode::{Op, Operand};
use otter_vm::{JitCompileSnapshot, jit::JitTestInstruction, native_abi as abi};
use std::collections::BTreeMap;

#[test]
fn narrowed_operations_emit_no_reference_to_an_omitted_cold_exit() {
    let view = JitCompileSnapshot::without_feedback(
        7,
        1,
        3,
        vec![JitTestInstruction::new(
            Op::ReturnValue,
            0,
            0,
            vec![Operand::Register(0)],
        )],
    );
    let plan = crate::template::TemplatePlan::build(&view).unwrap();
    let transitions = crate::entry::TransitionTable::resolve();
    let mut cases = vec![
        TemplateOp::LoadImmediate {
            dst: 1,
            bits: otter_vm::Value::undefined().to_bits(),
        },
        TemplateOp::Move { dst: 1, src: 0 },
        TemplateOp::LoadSelfClosure { dst: 1 },
        TemplateOp::LoadClosureContext { dst: 1 },
        TemplateOp::LoadContextSlot {
            dst: 1,
            context: 0,
            depth: 0,
            slot: 0,
        },
        TemplateOp::StoreContextSlot {
            src: 1,
            context: 0,
            depth: 0,
            slot: 0,
        },
        TemplateOp::FusedNumericChain {
            steps: crate::template::plan::TemplateTail { start: 0, len: 0 },
            leaves: crate::template::plan::TemplateTail { start: 0, len: 0 },
            jump_target: 0,
        },
        TemplateOp::Return { src: 0 },
        TemplateOp::ReturnUndefined,
        TemplateOp::ReturnDerived {
            value: 1,
            context: 0,
            depth: 0,
            slot: 0,
        },
    ];
    for back_edge in [false, true] {
        cases.push(TemplateOp::Jump {
            target: 0,
            back_edge,
        });
        cases.push(TemplateOp::BranchNullish {
            condition: 0,
            target: 0,
            back_edge,
        });
        for when_truthy in [false, true] {
            cases.push(TemplateOp::Branch {
                condition: 0,
                target: 0,
                when_truthy,
                back_edge,
            });
        }
    }
    for op in cases {
        let mut ops = Assembler::new().unwrap();
        let exits = std::array::from_fn::<_, 9, _>(|_| ops.new_dynamic_label());
        let destination = ops.new_dynamic_label();
        let labels = BTreeMap::from([(0, destination)]);
        let mut relocations = crate::artifact::relocation::RelocationCapture::default();
        let mut return_sites = Vec::new();
        let mut shared_property = super::super::shared_property::SharedPropertyProbes::default();
        let mut events = None;
        let mut code_map = None;
        super::super::operation::emit_operation(
            super::super::operation::OperationContext {
                ops: &mut ops,
                relocations: &mut relocations,
                return_sites: &mut return_sites,
                call_safepoint: 0,
                transitions: &transitions,
                view: &view,
                plan: &plan,
                labels: &labels,
                exits: OperationExits {
                    type_mismatch_exit: exits[0],
                    unsupported_exit: exits[1],
                    runtime_transition_exit: exits[2],
                    allocation_miss_exit: exits[3],
                    backedge_relink_exit: exits[4],
                    returned: exits[5],
                    committed_throw: exits[6],
                    threw: exits[7],
                    fatal: exits[8],
                },
                frame_kind: abi::NativeFrameKind::Baseline,
                shared_property: &mut shared_property,
                direct_call_events: &mut events,
                code_map: &mut code_map,
            },
            &TemplateInstr {
                pc: 0,
                byte_pc: 0,
                op,
            },
        )
        .unwrap();
        dynasm!(ops ; .arch x64 ; =>destination ; ret);
        for (index, exit) in exits.into_iter().enumerate() {
            if required(op) & (1 << index) != 0 {
                dynasm!(ops ; .arch x64 ; =>exit ; ret);
            }
        }
        ops.finalize()
            .unwrap_or_else(|error| panic!("{op:?} references an omitted cold exit: {error:?}"));
    }
}
