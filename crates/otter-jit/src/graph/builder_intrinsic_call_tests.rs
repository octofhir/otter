//! Intrinsic identity, call domains and source reconstruction in the SSA builder.
//!
//! # Contents
//! - Fixed intrinsic forms remove call/window nodes after an identity guard.
//! - Unsupported forms and refused proofs retain native-kind linkage.
//! - Ignored actuals and nested inline sources survive eager reconstruction.
//! - Admitted Array births prove length receivers without feedback.
//!
//! # Invariants
//! Synthetic cell addresses describe graph geometry and are never dereferenced.
//! Executable identity, moving roots and semantic effects are runtime proofs.
//!
//! # See also
//! - `crate::intrinsic_calls` owns shared form admission.
//! - `crate::graph::builder` owns eager source states and body splices.

use std::{collections::BTreeSet, rc::Rc, sync::Arc};

use otter_bytecode::{Op, Operand};
use otter_vm::{
    IntrinsicCallKind, JitCompileSnapshot, JitInlineCallee, JitIntrinsicCallProof, JitNativeCall,
    jit::{JitDirectCallPlan, JitDirectCallThisMode, JitDirectCallee, JitTestInstruction},
    native_abi::{ExitReason, NativeFrameKind},
};

use super::super::{Built, Kind, build};
use crate::{
    call_linkage::CallPlan,
    graph::{BaselineSupport, bytecode::Analysis},
};

fn snapshot(
    fid: u32,
    params: u16,
    registers: u16,
    code: Vec<(Op, Vec<Operand>)>,
) -> JitCompileSnapshot {
    JitCompileSnapshot::without_feedback(
        fid,
        params,
        registers,
        code.into_iter()
            .enumerate()
            .map(|(pc, (op, operands))| {
                JitTestInstruction::new(op, pc as u32, pc as u32 * 4, operands)
            })
            .collect(),
    )
}

fn call(kind: IntrinsicCallKind, construct: bool, argc: u16) -> JitCompileSnapshot {
    let dst = argc + 1;
    let mut operands = vec![
        Operand::Register(dst),
        Operand::Register(0),
        Operand::ConstIndex(u32::from(argc)),
    ];
    operands.extend((1..=argc).map(Operand::Register));
    let mut view = snapshot(
        90,
        dst,
        dst + 1,
        vec![
            (if construct { Op::New } else { Op::Call }, operands),
            (Op::ReturnValue, vec![Operand::Register(dst)]),
        ],
    );
    prove(&mut view, kind);
    view
}

fn prove(view: &mut JitCompileSnapshot, kind: IntrinsicCallKind) {
    view.literal_allocations.realm_id = 7;
    view.instructions[0].call_attempted = true;
    view.native_calls.insert(
        0,
        JitNativeCall::Intrinsic(JitIntrinsicCallProof {
            kind,
            identity_cell: 32,
            realm_id: 7,
        }),
    );
}

fn graph(view: &JitCompileSnapshot) -> Built {
    let analysis = Rc::new(Analysis::build(view).expect("intrinsic call analysis"));
    build(
        view,
        &analysis,
        &BaselineSupport {
            supported: vec![true; view.instructions.len()],
        },
        None,
    )
    .expect("intrinsic call graph")
}

#[test]
fn admitted_forms_remove_activation_and_window_operations() {
    for (kind, construct, argc, result) in [
        (IntrinsicCallKind::Array, false, 0, Kind::NewArrayEmpty),
        (IntrinsicCallKind::Array, true, 0, Kind::NewArrayEmpty),
        (IntrinsicCallKind::Array, false, 1, Kind::NewArrayWithLength),
        (IntrinsicCallKind::Array, true, 1, Kind::NewArrayWithLength),
        (IntrinsicCallKind::Object, false, 0, Kind::NewObject),
        (IntrinsicCallKind::Object, true, 0, Kind::NewObject),
        (IntrinsicCallKind::Boolean, false, 1, Kind::ToBoolean),
        (IntrinsicCallKind::Number, false, 0, Kind::ConstInt32(0)),
    ] {
        let view = call(kind, construct, argc);
        let built = graph(&view);
        let guard = built
            .graph
            .nodes
            .iter()
            .find(|node| node.kind == Kind::CheckCellValue(32))
            .expect("evaluated callee identity");
        let state = built
            .graph
            .frame_state(guard.eager.expect("pre-effect call guard"));
        assert_eq!((state.function_id, state.pc, state.byte_pc), (90, 0, 0));
        assert!(state.registers.contains(&(0, guard.inputs[0])));
        assert!(built.graph.nodes.iter().any(|node| node.kind == result));
        assert!(!built.graph.nodes.iter().any(|node| matches!(
            node.kind,
            Kind::CallJs { .. } | Kind::Generic { .. } | Kind::LoadWindow(_)
        )));
        for node in built.graph.nodes.iter().filter(|node| {
            matches!(
                node.kind,
                Kind::NewArrayEmpty | Kind::NewArrayWithLength | Kind::NewObject
            )
        }) {
            assert_eq!(node.eager, guard.eager);
            assert!(node.lazy.is_none());
        }
    }
    let built = graph(&call(IntrinsicCallKind::Boolean, false, 0));
    assert!(
        built
            .graph
            .nodes
            .iter()
            .any(|node| node.kind == Kind::ConstTagged(otter_vm::Value::boolean(false).to_bits()))
    );
    assert!(
        built
            .graph
            .nodes
            .iter()
            .any(|node| node.kind == Kind::CheckCellValue(32))
    );
}

#[test]
fn intrinsic_array_births_remove_the_first_length_receiver_guard() {
    for (construct, argc) in [(false, 0), (true, 0), (false, 1), (true, 1)] {
        let mut operands = vec![
            Operand::Register(2),
            Operand::Register(0),
            Operand::ConstIndex(argc),
        ];
        if argc == 1 {
            operands.push(Operand::Register(1));
        }
        let mut view = snapshot(
            90,
            2,
            4,
            vec![
                (if construct { Op::New } else { Op::Call }, operands),
                (
                    Op::LoadProperty,
                    vec![
                        Operand::Register(3),
                        Operand::Register(2),
                        Operand::ConstIndex(0),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
        );
        prove(&mut view, IntrinsicCallKind::Array);
        view.instructions[1].load_array_length = true;
        let built = graph(&view);
        let length = built
            .graph
            .nodes
            .iter()
            .find(|node| matches!(node.kind, Kind::LoadArrayLength { .. }))
            .unwrap();
        assert_eq!(length.kind, Kind::LoadArrayLength { array_proved: true });
        let state = built.graph.frame_state(length.eager.unwrap());
        assert_eq!((state.function_id, state.pc), (90, 1));
    }
}

#[test]
fn ignored_boolean_actuals_remain_in_the_before_call_state() {
    let built = graph(&call(IntrinsicCallKind::Boolean, false, 3));
    let guard = built
        .graph
        .nodes
        .iter()
        .find(|node| node.kind == Kind::CheckCellValue(32))
        .unwrap();
    let state = built.graph.frame_state(guard.eager.unwrap());
    assert_eq!(
        state
            .registers
            .iter()
            .map(|(reg, _)| *reg)
            .collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
    let converted = built
        .graph
        .nodes
        .iter()
        .find(|node| node.kind == Kind::ToBoolean)
        .unwrap();
    let first_actual = state.registers.iter().find(|(reg, _)| *reg == 1).unwrap().1;
    assert_eq!(converted.inputs.as_slice(), [first_actual]);
}

#[test]
fn unsupported_intrinsic_forms_use_the_native_kind_call_contract() {
    for (kind, construct, argc) in [
        (IntrinsicCallKind::Array, false, 2),
        (IntrinsicCallKind::Array, true, 2),
        (IntrinsicCallKind::Boolean, true, 1),
        (IntrinsicCallKind::Number, false, 1),
        (IntrinsicCallKind::Number, true, 0),
        (IntrinsicCallKind::Object, false, 1),
        (IntrinsicCallKind::Object, true, 1),
    ] {
        let built = graph(&call(kind, construct, argc));
        assert!(built.graph.nodes.iter().any(|node| matches!(
            node.kind, Kind::CallJs { plan: CallPlan::Native, construct: actual, .. } if actual == construct
        )));
        assert!(!built.graph.nodes.iter().any(|node| matches!(
            node.kind,
            Kind::CheckCellValue(_) | Kind::Generic { .. } | Kind::LoadWindow(_)
        )));
    }
}

#[test]
fn invalid_or_retired_identity_proofs_do_not_respeculate() {
    for refusal in 0..3 {
        let mut view = call(IntrinsicCallKind::Array, true, 0);
        match refusal {
            0 => view.literal_allocations.realm_id = 8,
            1 => {
                let JitNativeCall::Intrinsic(proof) = view.native_calls.get_mut(&0).unwrap() else {
                    unreachable!()
                };
                proof.identity_cell = 0;
            }
            _ => {
                view.optimized_exit_reasons
                    .insert(0, BTreeSet::from([ExitReason::IdentityGuard]));
            }
        }
        let built = graph(&view);
        assert!(built.graph.nodes.iter().any(|node| matches!(
            node.kind,
            Kind::CallJs {
                plan: CallPlan::Native,
                construct: true,
                ..
            }
        )));
        assert!(
            !built
                .graph
                .nodes
                .iter()
                .any(|node| matches!(node.kind, Kind::CheckCellValue(_) | Kind::NewArrayEmpty))
        );
    }
}

#[test]
fn an_inlined_intrinsic_keeps_its_source_identity_and_evaluated_arguments() {
    use Operand::{ConstIndex as C, Register as R};
    let mut callee = snapshot(
        91,
        2,
        3,
        vec![
            (Op::New, vec![R(2), R(0), C(1), R(1)]),
            (Op::ReturnValue, vec![R(2)]),
        ],
    );
    prove(&mut callee, IntrinsicCallKind::Array);
    let callee = Arc::new(callee);
    let mut caller = snapshot(
        90,
        3,
        4,
        vec![
            (Op::Call, vec![R(3), R(0), C(2), R(1), R(2)]),
            (Op::ReturnValue, vec![R(3)]),
        ],
    );
    caller.literal_allocations.realm_id = 0;
    caller.instructions[0].call_attempted = true;
    caller.direct_callees.insert(
        0,
        vec![JitDirectCallee {
            plan: JitDirectCallPlan {
                function_id: 91,
                code_object_id: 1,
                entry_cell: 0,
                tier: NativeFrameKind::Baseline,
                this_mode: JitDirectCallThisMode::StrictOrLexical,
                is_derived_constructor: false,
                call_flags: callee.code_block.call_flags(),
                callee_cell: 0,
            },
            receiver_allocation: None,
        }],
    );
    caller
        .inline_callees
        .insert(0, JitInlineCallee { body: callee });
    let built = graph(&caller);
    assert_eq!(built.graph.inlined.len(), 1);
    let guard = built
        .graph
        .nodes
        .iter()
        .find(|node| node.kind == Kind::CheckCellValue(32))
        .unwrap();
    let allocation = built
        .graph
        .nodes
        .iter()
        .find(|node| node.kind == Kind::NewArrayWithLength)
        .unwrap();
    assert_eq!(guard.origin, 1);
    let state = built.graph.frame_state(guard.eager.unwrap());
    assert_eq!((state.function_id, state.pc, state.byte_pc), (91, 0, 0));
    assert_eq!(allocation.eager, guard.eager);
    assert!(state.registers.contains(&(0, guard.inputs[0])));
    assert!(state.registers.contains(&(1, allocation.inputs[0])));
    let caller_state = built
        .graph
        .frame_state(state.caller.as_ref().unwrap().state);
    assert_eq!(caller_state.function_id, 90);
    assert!(!built.graph.nodes.iter().any(|node| matches!(
        node.kind,
        Kind::CallJs { .. } | Kind::Generic { .. } | Kind::LoadWindow(_)
    )));
}

/// `r2 = r0.exec(r1)` (or `.test`) whose method load has one own-data
/// program and whose callee feedback proved the realm's original method.
fn regexp_method_call(kind: IntrinsicCallKind, realm: u32) -> JitCompileSnapshot {
    use otter_vm::{JitCacheIrOp, JitCacheIrProgram, object::FieldLocation};
    let mut view = snapshot(
        90,
        2,
        3,
        vec![
            (
                Op::CallMethodValue,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(1),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
    );
    prove(&mut view, kind);
    view.native_calls.insert(
        0,
        JitNativeCall::Intrinsic(JitIntrinsicCallProof {
            kind,
            identity_cell: 32,
            realm_id: realm,
        }),
    );
    let field = FieldLocation::inline(0);
    view.property_programs.insert(
        0,
        vec![JitCacheIrProgram {
            ops: vec![
                JitCacheIrOp::GuardShape {
                    object: 0,
                    shape: 128,
                },
                JitCacheIrOp::GuardAtomSlot {
                    object: 0,
                    atom: 1,
                    field,
                    writable: false,
                },
                JitCacheIrOp::LoadField { object: 0, field },
            ]
            .into_boxed_slice(),
        }],
    );
    view
}

#[test]
fn proved_builtin_methods_run_without_a_call_activation() {
    use crate::intrinsic_calls::IntrinsicMethodForm as Form;
    for (kind, form) in [
        (IntrinsicCallKind::RegExpExec, Form::RegExpExec),
        (IntrinsicCallKind::RegExpTest, Form::RegExpTest),
        (IntrinsicCallKind::StringReplace, Form::StringReplace),
        (IntrinsicCallKind::StringSearch, Form::StringSearch),
    ] {
        let built = graph(&regexp_method_call(kind, 7));
        let kinds: Vec<&Kind> = built
            .graph
            .blocks
            .iter()
            .flat_map(|block| block.body.iter())
            .map(|&node| &built.graph.node(node).kind)
            .collect();
        assert!(kinds.contains(&&Kind::CheckCellValue(32)));
        let method = built
            .graph
            .nodes
            .iter()
            .find(|node| node.kind == Kind::BuiltinMethod(form))
            .expect("the proved method runs as its builtin");
        // The receiver, then one input per argument the method reads: an
        // argument the call does not pass is `undefined`.
        assert_eq!(method.inputs.len(), 1 + form.arity());
        if form.arity() == 2 {
            assert_eq!(
                built.graph.node(method.inputs[2]).kind,
                Kind::ConstTagged(otter_vm::value::tag::VALUE_UNDEFINED)
            );
        }
        assert!(!kinds.iter().any(|kind| matches!(kind, Kind::CallJs { .. })));
        // A proof from another realm keeps the ordinary native call.
        let foreign = graph(&regexp_method_call(kind, 8));
        assert!(
            foreign
                .graph
                .nodes
                .iter()
                .all(|node| { !matches!(node.kind, Kind::BuiltinMethod(_)) })
        );
        assert!(
            foreign
                .graph
                .nodes
                .iter()
                .any(|node| { matches!(node.kind, Kind::CallJs { .. }) })
        );
    }
}
