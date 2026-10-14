//! Context-slot constants of a method body spliced against its exact closure.
//!
//! # Contents
//! - A proven method's body reads its closure's watched slots as constants,
//!   each use behind a slot proof, and its closure's context in one field.
//! - Slots the bake did not fix, at their own depth, stay loads.
//! - A slot store notes the write for the slot's watch.
//! - Heap-backed primitive constants retain sloppy receiver conversion.
//!
//! # Invariants
//! Synthetic snapshots describe compiler geometry only: the cell, validity
//! and watch identities are never dereferenced, and no code is emitted.
//!
//! # See also
//! - `otter_vm::code_dependencies` for watches and their firing.
//! - `crates/otter-difftest/corpus/context_slot_constants.js` for the runtime
//!   behaviour of every writer.

use std::rc::Rc;
use std::sync::Arc;

use otter_bytecode::{ContextCoord, Op, Operand};
use otter_vm::JitCompileSnapshot;
use otter_vm::jit::{
    JitConstantValue, JitContextConstant, JitDirectCallPlan, JitDirectCallThisMode,
    JitDirectCallee, JitDirectMethod, JitMethodGuard, JitPrototypeValidity, JitTestInstruction,
};
use otter_vm::native_abi::{FUNCTION_CALL_NO_RECEIVER_CONVERSION, NativeFrameKind};

use super::{Kind, build};
use crate::graph::{BaselineSupport, bytecode::Analysis};

const METHOD_FID: u32 = 8;

fn coord(depth: u16, slot: u16) -> Operand {
    Operand::Imm32(
        ContextCoord::new(depth, slot)
            .expect("addressable slot")
            .to_imm32(),
    )
}

/// `r1 = closure.slot1; closure.slot2 = r1; r3 = closure.parent.slot0;
/// r1 = closure.slot1; return r1`.
fn body() -> JitCompileSnapshot {
    let instructions = vec![
        JitTestInstruction::new(Op::LoadClosureContext, 0, 0, vec![Operand::Register(0)]),
        JitTestInstruction::new(
            Op::LoadContextSlot,
            1,
            8,
            vec![Operand::Register(1), Operand::Register(0), coord(0, 1)],
        ),
        JitTestInstruction::new(
            Op::StoreContextSlot,
            2,
            24,
            vec![Operand::Register(1), Operand::Register(0), coord(0, 2)],
        ),
        JitTestInstruction::new(
            Op::LoadContextSlot,
            3,
            40,
            vec![Operand::Register(3), Operand::Register(0), coord(1, 0)],
        ),
        JitTestInstruction::new(
            Op::LoadContextSlot,
            4,
            56,
            vec![Operand::Register(1), Operand::Register(0), coord(0, 1)],
        ),
        JitTestInstruction::new(Op::ReturnValue, 5, 72, vec![Operand::Register(1)]),
    ];
    let mut body = JitCompileSnapshot::without_feedback(METHOD_FID, 0, 4, instructions);
    body.closure_constants = vec![JitContextConstant {
        depth: 0,
        slot: 1,
        watch: 5,
        value: JitConstantValue::Int32(7),
    }];
    body
}

/// `r0 = r1.method()` whose one target's holder slot is proven.
fn caller(body: JitCompileSnapshot) -> JitCompileSnapshot {
    let mut view = JitCompileSnapshot::without_feedback(
        7,
        0,
        4,
        vec![
            JitTestInstruction::new(
                Op::CallMethodValue,
                0,
                0,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            ),
            JitTestInstruction::new(Op::ReturnValue, 1, 32, vec![Operand::Register(0)]),
        ],
    );
    view.cage_base = 0x1000;
    view.instructions[0].call_attempted = true;
    let method = JitDirectMethod {
        target_index: 0,
        target_count: 1,
        guard: JitMethodGuard {
            method_fid: METHOD_FID,
            recv_shape: 0x80,
            prototype_validity: Some(JitPrototypeValidity {
                address: 0x2000,
                identity: 0x90,
            }),
            holder_root: 0x90,
            method_field: otter_vm::object::FieldLocation::inline(0),
            slot_proven: true,
            method_cell: 0x3000,
            accessor_byte: None,
        },
        callee: JitDirectCallee {
            plan: JitDirectCallPlan {
                function_id: METHOD_FID,
                code_object_id: 91,
                entry_cell: 0x4000,
                tier: NativeFrameKind::Baseline,
                this_mode: JitDirectCallThisMode::StrictOrLexical,
                is_derived_constructor: false,
                call_flags: FUNCTION_CALL_NO_RECEIVER_CONVERSION,
                callee_cell: 0,
            },
            receiver_allocation: None,
        },
        body: Some(Arc::new(body)),
    };
    view.direct_methods.insert(0, vec![method]);
    view
}

fn placed(graph: &super::super::ir::Graph) -> Vec<&Kind> {
    graph
        .blocks
        .iter()
        .flat_map(|block| block.body.iter())
        .map(|&node| &graph.node(node).kind)
        .collect()
}

#[test]
fn a_proven_method_reads_its_closure_constants_behind_slot_proofs() {
    let view = caller(body());
    let analysis = Rc::new(Analysis::build(&view).unwrap());
    let built = build(
        &view,
        &analysis,
        &BaselineSupport {
            supported: vec![true; view.instructions.len()],
        },
        None,
    )
    .unwrap();
    assert_eq!(built.inline_views.len(), 1, "the method body is spliced");
    let kinds = placed(&built.graph);
    let slots = view.context_layout.slots_byte as i32;

    // Both reads of the fixed slot are its constant, each behind a proof.
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| ***kind == Kind::CheckContextSlot { watch: 5, slot: 1 })
            .count(),
        2
    );
    assert!(
        built
            .graph
            .nodes
            .iter()
            .any(|node| node.kind == Kind::ConstInt32(7))
    );
    assert!(!kinds.contains(&&Kind::LoadTaggedField(slots + 8)));
    // The proven closure is a heap closure: no tagged-function dispatch.
    assert!(!kinds.contains(&&Kind::LoadClosureContext));
    // The slot two scopes out was not fixed: a parent hop and a load.
    assert!(kinds.contains(&&Kind::LoadContextParent));
    assert!(kinds.contains(&&Kind::LoadTaggedField(slots)));
    // The store keeps its write and tells the slot's watch.
    assert!(kinds.contains(&&Kind::StoreTaggedField(slots + 16)));
    assert!(kinds.contains(&&Kind::NoteContextWrite(2)));
}

#[test]
fn an_unproven_method_body_ignores_closure_constants() {
    let mut view = caller(body());
    view.direct_methods.get_mut(&0).unwrap()[0]
        .guard
        .slot_proven = false;
    let analysis = Rc::new(Analysis::build(&view).unwrap());
    let built = build(
        &view,
        &analysis,
        &BaselineSupport {
            supported: vec![true; view.instructions.len()],
        },
        None,
    )
    .unwrap();
    let kinds = placed(&built.graph);
    let slots = view.context_layout.slots_byte as i32;
    assert!(
        !kinds
            .iter()
            .any(|kind| matches!(kind, Kind::CheckContextSlot { .. }))
    );
    assert!(kinds.contains(&&Kind::LoadClosureContext));
    assert!(kinds.contains(&&Kind::LoadTaggedField(slots + 8)));
}

#[test]
fn only_object_cells_bypass_sloppy_receiver_conversion() {
    use Operand::{ConstIndex as C, Register as R};
    for (constant, object) in [
        (
            JitConstantValue::Cell {
                cell: 0x5000,
                object: false,
                shape: 0,
            },
            false,
        ),
        (
            JitConstantValue::Cell {
                cell: 0x5000,
                object: true,
                shape: 0,
            },
            true,
        ),
        (
            JitConstantValue::Bits(otter_vm::Value::function_id(10).to_bits()),
            true,
        ),
    ] {
        let receiver = Arc::new(JitCompileSnapshot::without_feedback(
            9,
            0,
            1,
            vec![
                JitTestInstruction::new(Op::LoadThis, 0, 0, vec![R(0)]),
                JitTestInstruction::new(Op::ReturnValue, 1, 8, vec![R(0)]),
            ],
        ));
        let mut method = JitCompileSnapshot::without_feedback(
            METHOD_FID,
            1,
            3,
            vec![
                JitTestInstruction::new(Op::LoadClosureContext, 0, 0, vec![R(1)]),
                JitTestInstruction::new(Op::LoadContextSlot, 1, 8, vec![R(1), R(1), coord(0, 1)]),
                JitTestInstruction::new(Op::CallWithThis, 2, 24, vec![R(2), R(0), R(1), C(0)]),
                JitTestInstruction::new(Op::ReturnValue, 3, 48, vec![R(2)]),
            ],
        );
        method.closure_constants = vec![JitContextConstant {
            depth: 0,
            slot: 1,
            watch: 5,
            value: constant,
        }];
        method.instructions[2].call_attempted = true;
        method.direct_callees.insert(
            24,
            vec![JitDirectCallee {
                plan: JitDirectCallPlan {
                    function_id: 9,
                    code_object_id: 92,
                    entry_cell: 0x6000,
                    tier: NativeFrameKind::Baseline,
                    this_mode: JitDirectCallThisMode::SloppyGlobal,
                    is_derived_constructor: false,
                    call_flags: receiver.code_block.call_flags(),
                    callee_cell: 0,
                },
                receiver_allocation: None,
            }],
        );
        method
            .inline_callees
            .insert(24, otter_vm::JitInlineCallee { body: receiver });
        let view = caller(method);
        let analysis = Rc::new(Analysis::build(&view).unwrap());
        let built = build(
            &view,
            &analysis,
            &BaselineSupport {
                supported: vec![true; view.instructions.len()],
            },
            None,
        )
        .unwrap();
        assert_eq!(built.graph.inlined.len(), if object { 2 } else { 1 });
        if !object {
            assert!(placed(&built.graph).iter().any(|kind| matches!(
                kind,
                Kind::CallJs {
                    receiver_object: false,
                    ..
                }
            )));
        }
    }
}

#[test]
fn own_source_constants_use_a_dependency_without_a_context_identity_check() {
    let mut view = body();
    view.unique_closure_context = Some(0x6000);
    let analysis = Rc::new(Analysis::build(&view).unwrap());
    let built = build(
        &view,
        &analysis,
        &BaselineSupport {
            supported: vec![true; view.instructions.len()],
        },
        None,
    )
    .unwrap();
    let kinds = placed(&built.graph);
    assert!(kinds.contains(&&Kind::CheckClosureContext(METHOD_FID)));
    assert!(kinds.contains(&&Kind::LoadCellValue(0x6000)));
    assert!(
        built
            .graph
            .nodes
            .iter()
            .any(|node| node.kind == Kind::ConstInt32(7))
    );
    assert!(!kinds.contains(&&Kind::LoadClosureContext));
    assert!(
        !kinds
            .iter()
            .any(|kind| matches!(kind, Kind::CheckCellValue(_)))
    );
    assert!(!kinds.contains(&&Kind::LoadTaggedField(
        view.context_layout.slots_byte as i32 + 8
    )));
}
