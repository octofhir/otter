//! Exact-source lexical allocation and emitted collector ownership proofs.
//!
//! # Contents
//! - Context/copy/function nodes with late results and canonical input homes.
//! - Inlined factories capturing their receiver and undefined `new.target`.
//! - Prepared source byte PCs, direct typed cold entries and inline safepoints.
//!
//! # Invariants
//! - Synthetic plans are compiler geometry, never executable moving cells.
//! - Actual machine code is emitted by both backends; runtime GC is separate.
//! - No inline lexical binding is read from the physical caller frame.
//!
//! # See also
//! - `allocation::lexical_tests` executes the shared native LAB encoders.
//! - `otter-runtime/tests/jit_native_context_closure.rs` owns real collection.

use std::sync::Arc;

use otter_bytecode::{FunctionCodeBuilder, Op, Operand, encoding::measure_wordcode_function};
use otter_vm::jit::{
    JIT_YOUNG_CONTEXT_HEADER_WORD, JitClosureAllocationPlan, JitContextAllocationPlan,
    JitDirectCallPlan, JitDirectCallThisMode, JitDirectCallee, JitTestInstruction,
};
use otter_vm::native_abi::{self as abi, NativeFrameKind};
use otter_vm::{
    JitArtifactFileName, JitArtifactIdentity, JitCompileSnapshot, JitDebugTarget, JitDebugTier,
    JitInlineCallee,
};

use super::graph;
use crate::graph::{
    ir::{InputPolicy, Kind, NodeId, Repr, ResultPolicy},
    registers,
};

fn snapshot(
    fid: u32,
    params: u16,
    registers: u16,
    code: Vec<(Op, Vec<Operand>)>,
) -> JitCompileSnapshot {
    let mut builder = FunctionCodeBuilder::new();
    for (op, operands) in &code {
        builder.push(*op, operands);
    }
    let positions = measure_wordcode_function(&builder.finish())
        .unwrap()
        .instr_to_byte_pc;
    JitCompileSnapshot::without_feedback(
        fid,
        params,
        registers,
        code.into_iter()
            .zip(positions)
            .enumerate()
            .map(|(pc, ((op, operands), byte_pc))| {
                JitTestInstruction::new(op, pc as u32, byte_pc, operands)
            })
            .collect(),
    )
}

fn prepare(view: &mut JitCompileSnapshot) {
    let bytes = view.context_layout.slots_byte;
    view.context_allocations.insert(
        (view.code_block.id, 0),
        JitContextAllocationPlan {
            cell_bytes: bytes,
            header_word: JIT_YOUNG_CONTEXT_HEADER_WORD | (u64::from(bytes) << 32),
            body_word: u64::from(view.code_block.id),
            derived_this_slot: None,
            initial_words: Box::default(),
        },
    );
    for instruction in &view.instructions {
        if matches!(
            instruction.op(&view.code_block),
            Op::MakeClosure | Op::MakeFunction
        ) {
            let arrow = instruction.op(&view.code_block) == Op::MakeClosure;
            // Fixture-only call-word encoding: the public lookup summary
            // occupies the flags word's top byte; production words are baked
            // by the VM producer, not derived by this graph fixture.
            view.closure_allocations.insert(
                instruction.byte_pc,
                JitClosureAllocationPlan {
                    call_word: 902
                        | (u64::from(
                            (u32::from(otter_vm::closure::CLOSURE_LOOKUP_ORDINARY) << 24)
                                | if arrow {
                                    otter_vm::closure::CLOSURE_CALL_FLAG_BOUND_THIS
                                } else {
                                    0
                                },
                        ) << 32),
                    arrow,
                },
            );
        }
    }
}

#[test]
fn lexical_allocations_have_declared_homes_late_results_and_eager_refusal() {
    use Operand::{ConstIndex, Imm32, Register};
    let mut view = snapshot(
        901,
        1,
        5,
        vec![
            (Op::CreateContext, vec![Register(1), Register(4), Imm32(0)]),
            (Op::CopyContext, vec![Register(2), Register(1)]),
            (
                Op::MakeClosure,
                vec![Register(3), ConstIndex(0), Register(2)],
            ),
            (Op::MakeFunction, vec![Register(4), ConstIndex(0)]),
            (Op::ReturnValue, vec![Register(3)]),
        ],
    );
    prepare(&mut view);
    let built = graph(&view);
    let allocations: Vec<_> = built
        .graph
        .nodes
        .iter()
        .filter(|node| {
            matches!(
                node.kind,
                Kind::NativeNewContext(_) | Kind::CopyContext | Kind::NewClosure
            )
        })
        .collect();
    assert_eq!(allocations.len(), 4);
    assert!(
        !built
            .graph
            .nodes
            .iter()
            .any(|node| matches!(node.kind, Kind::Generic { .. }))
    );
    for node in allocations {
        assert_eq!(node.repr, Repr::Tagged);
        assert!(node.eager.is_some() && node.lazy.is_none());
        let properties = node.kind.properties();
        assert!(
            properties.effectful
                && properties.writes
                && properties.may_collect
                && properties.eager_deopt
        );
        assert!(!properties.call && !properties.can_throw);
        for target in [&registers::AARCH64, &registers::X86_64] {
            let constraints = node.kind.constraints(node.inputs.len(), target);
            assert!(
                constraints
                    .inputs
                    .iter()
                    .all(|policy| *policy == InputPolicy::Home)
            );
            assert_eq!(constraints.gp_temps, 4);
            assert_eq!(constraints.fp_temps, 0);
            assert_eq!(constraints.result, ResultPolicy::Register);
        }
    }
    let closures: Vec<_> = built
        .graph
        .nodes
        .iter()
        .filter(|node| node.kind == Kind::NewClosure)
        .collect();
    assert_eq!(closures[0].inputs.len(), 3);
    assert_eq!(built.graph.node(closures[0].inputs[1]).kind, Kind::LoadThis);
    assert_eq!(
        built.graph.node(closures[0].inputs[2]).kind,
        Kind::LoadNewTarget
    );
    assert!(
        closures[1]
            .inputs
            .iter()
            .all(|input| built.graph.node(*input).kind
                == Kind::ConstTagged(otter_vm::Value::UNDEFINED.to_bits()))
    );
    assert!(
        Kind::LoadThis.properties().effectful,
        "derived this changes at super binding and cannot be commoned/hoisted"
    );
}

#[test]
fn emitted_inline_factory_uses_own_plans_receiver_and_undefined_new_target() {
    use Operand::{ConstIndex, Imm32, Register};
    let mut factory = snapshot(
        902,
        1,
        5,
        vec![
            (Op::CreateContext, vec![Register(1), Register(4), Imm32(0)]),
            (Op::CopyContext, vec![Register(1), Register(1)]),
            (Op::LoadThis, vec![Register(2)]),
            (Op::LoadNewTarget, vec![Register(3)]),
            (
                Op::MakeClosure,
                vec![Register(4), ConstIndex(0), Register(1)],
            ),
            (Op::ReturnValue, vec![Register(4)]),
        ],
    );
    prepare(&mut factory);
    factory.literal_allocations.realm_id = 7;
    let closure_byte_pc = factory.instructions[4].byte_pc;
    let factory = Arc::new(factory);
    let mut caller = snapshot(
        901,
        3,
        4,
        vec![
            (
                Op::CallWithThis,
                vec![
                    Register(3),
                    Register(0),
                    Register(1),
                    ConstIndex(1),
                    Register(2),
                ],
            ),
            (Op::ReturnValue, vec![Register(3)]),
        ],
    );
    let call_byte_pc = caller.instructions[0].byte_pc;
    caller.instructions[0].call_attempted = true;
    caller.direct_callees.insert(
        call_byte_pc,
        vec![JitDirectCallee {
            plan: JitDirectCallPlan {
                function_id: 902,
                code_object_id: 1,
                entry_cell: 0,
                tier: NativeFrameKind::Baseline,
                this_mode: JitDirectCallThisMode::StrictOrLexical,
                is_derived_constructor: false,
                call_flags: 0,
                callee_cell: 0,
            },
            receiver_allocation: None,
        }],
    );
    caller
        .inline_callees
        .insert(call_byte_pc, JitInlineCallee { body: factory });
    let transitions = crate::entry::TransitionTable::resolve();
    let compiled = crate::graph::compile(&caller, 9001, &transitions, None, false).unwrap();
    assert_eq!(
        compiled.built.inline_views.len(),
        1,
        "the context/closure body is now an actual eligible splice"
    );
    assert_eq!(
        compiled.built.inline_views[0].literal_allocations.realm_id,
        7
    );
    let graph = &compiled.built.graph;
    let (id, closure) = graph
        .nodes
        .iter()
        .enumerate()
        .find(|(_, node)| node.kind == Kind::NewClosure && node.origin == 1)
        .unwrap();
    assert_eq!(closure.pc, 4);
    assert_eq!(
        graph.node(closure.inputs[1]).kind,
        Kind::InitialRegister(1),
        "explicit callee receiver"
    );
    assert_eq!(
        graph.node(closure.inputs[2]).kind,
        Kind::ConstTagged(otter_vm::Value::UNDEFINED.to_bits()),
        "ordinary inline call does not capture the physical constructor's new.target"
    );
    assert!(!graph.nodes.iter().any(|node| node.origin == 1
        && matches!(
            node.kind,
            Kind::LoadThis | Kind::LoadNewTarget | Kind::Generic { .. }
        )));
    let state = graph.frame_state(closure.eager.unwrap());
    assert_eq!(state.function_id, 902);
    assert!(state.caller.is_some());
    assert!(closure_byte_pc > caller.instructions.last().unwrap().byte_pc);
    let node = NodeId(id as u32);
    let source =
        crate::graph::metadata::source_view(&caller, &compiled.built.inline_views, graph, node);
    assert!(source.context_allocations.contains_key(&(902, 0)));
    assert_eq!(
        source.closure_allocations[&closure_byte_pc].call_word as u32,
        902
    );

    let captured = crate::graph::compile_optimized(
        &caller,
        9002,
        &transitions,
        None,
        Some(crate::artifact::ArtifactRequest {
            identity: JitArtifactIdentity {
                function_name: "inline-lexical-factory".into(),
                module: "inline-lexical-factory.js".into(),
            },
            tier: JitDebugTier::Optimizing,
            entry: JitDebugTarget::Entry,
        }),
        false,
    )
    .unwrap();
    let artifact = captured.artifact.unwrap();
    let read = |name| {
        serde_json::from_slice::<serde_json::Value>(artifact.file(name).unwrap().contents())
            .unwrap()
    };
    let map = read(JitArtifactFileName::CodeMap);
    let region = map["regions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| {
            row["operationIndex"].as_u64() == Some(id as u64) && row["kind"] == "instruction"
        })
        .unwrap();
    assert_eq!(region["functionId"], 902);
    assert_eq!(region["bytePc"], closure_byte_pc);
    assert_eq!(region["logicalPc"], 4);
    let relocations = read(JitArtifactFileName::Relocations);
    for (pc, descriptor) in [
        (0, abi::STUB_CREATE_CONTEXT_ALLOC),
        (1, abi::STUB_COPY_CONTEXT_ALLOC),
        (4, abi::STUB_JIT_MAKE_CLOSURE),
    ] {
        let own = map["regions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| {
                row["functionId"] == 902
                    && row["logicalPc"] == pc
                    && row["operation"].as_str().is_some_and(|text| {
                        text.contains(if pc == 0 {
                            "NativeNewContext"
                        } else if pc == 1 {
                            "CopyContext"
                        } else {
                            "NewClosure"
                        })
                    })
            })
            .unwrap();
        assert!(relocations["relocations"].as_array().unwrap().iter().any(
            |row| row["target"]["id"] == descriptor.id
                && row["target"]["signature"] == "allocValue3"
                && own["startOffset"].as_u64().unwrap() <= row["startOffset"].as_u64().unwrap()
                && row["startOffset"].as_u64().unwrap() < own["endOffset"].as_u64().unwrap()
        ));
    }
    let recovery = read(JitArtifactFileName::Deopt);
    for pc in [0, 1, 4] {
        let exit = recovery["exits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|exit| {
                exit["reason"] == "allocationMiss"
                    && exit["action"] == "resume"
                    && exit["resumePcs"]
                        .as_array()
                        .is_some_and(|pcs| pcs.last() == Some(&serde_json::Value::from(pc)))
            })
            .expect("exact pre-effect lexical Miss/OOM exit");
        let state = recovery["frameStates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|state| state["id"] == exit["frameStateId"])
            .unwrap();
        let frames = state["frames"].as_array().unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["functionId"], 901);
        assert_eq!(frames[1]["functionId"], 902);
    }
    let safepoints = read(JitArtifactFileName::Safepoints);
    assert!(safepoints["records"].as_array().unwrap().iter().any(|row| {
        row["inlineFrames"].as_array().is_some_and(|frames| {
            frames.last().is_some_and(|frame| {
                frame["functionId"] == 902 && frame["bytePc"] == closure_byte_pc
            })
        })
    }));
}
