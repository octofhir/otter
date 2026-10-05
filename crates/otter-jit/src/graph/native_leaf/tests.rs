//! Source, call-clobber and emitted recovery proofs for pure receiver leaves.
//!
//! # Contents
//! - Exact declaration admission and refusal of different arity or effects.
//! - Builder operand selection from the evaluated CallWithThis state.
//! - Both register files' CFG-live and eager-only canonical homes.
//! - Nested source emission with an exact native edge and eager frame chain.
//!
//! # Invariants
//! Synthetic callable identities are compiler geometry only. Actual execution
//! and moving cells are covered by the runtime receiver-leaf fixture. No test
//! manufactures a second entry ABI or a materialized argument window.
//!
//! # See also
//! - `otter-runtime/tests/jit_graph_receiver_leaves.rs`.

use std::{rc::Rc, sync::Arc};

use otter_bytecode::{FunctionCodeBuilder, Op, Operand, encoding::measure_wordcode_function};
use otter_vm::jit::{
    JitDirectCallPlan, JitDirectCallThisMode, JitDirectCallee, JitNativeCall, JitNativeCallLayout,
    JitTestInstruction,
};
use otter_vm::native_abi::{self as abi, NativeFrameKind};
use otter_vm::{JitCompileSnapshot, JitInlineCallee, JitStaticNativeCall};

use crate::graph::{BaselineSupport, bytecode::Analysis};
use crate::graph::{
    builder,
    ir::{BlockId, FrameState, Graph, Kind, NodeId, Repr},
    regalloc::{self, Location},
    registers,
};

fn snapshot(
    fid: u32,
    params: u16,
    registers: u16,
    code: Vec<(Op, Vec<Operand>)>,
) -> JitCompileSnapshot {
    let mut bytes = FunctionCodeBuilder::new();
    for (op, operands) in &code {
        bytes.push(*op, operands);
    }
    let positions = measure_wordcode_function(&bytes.finish())
        .unwrap()
        .instr_to_byte_pc;
    let mut view = JitCompileSnapshot::without_feedback(
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
    );
    view.native_call_layout = JitNativeCallLayout::current();
    view
}
fn target(id: abi::RuntimeStubId, arguments: u8) -> JitStaticNativeCall {
    JitStaticNativeCall {
        builtin_native_ref: 73,
        leaf_stub_id: id,
        argument_count: arguments,
    }
}
fn prepare(view: &mut JitCompileSnapshot, pc: usize, target: JitStaticNativeCall) {
    view.instructions[pc].call_attempted = true;
    view.native_calls
        .insert(view.instructions[pc].byte_pc, JitNativeCall::Leaf(target));
}
fn call(id: abi::RuntimeStubId, count: u8) -> JitCompileSnapshot {
    use Operand::{ConstIndex, Register};
    let mut operands = vec![
        Register(3),
        Register(0),
        Register(1),
        ConstIndex(u32::from(count)),
    ];
    operands.extend((0..count).map(|_| Register(2)));
    let mut view = snapshot(
        701,
        3,
        4,
        vec![
            (Op::CallWithThis, operands),
            (Op::ReturnValue, vec![Register(3)]),
        ],
    );
    prepare(&mut view, 0, target(id, count));
    view
}
fn build(view: &JitCompileSnapshot) -> builder::Built {
    let analysis = Rc::new(Analysis::build(view).unwrap());
    builder::build(
        view,
        &analysis,
        &BaselineSupport {
            supported: vec![true; view.instructions.len()],
        },
        None,
    )
    .unwrap()
}
#[test]
fn declaration_arity_effects_and_identity_layout_have_one_admission() {
    let view = call(abi::STUB_STRING_CHAR_CODE_AT_LEAF.id, 1);
    let declared =
        super::admit(&view, target(abi::STUB_STRING_CHAR_CODE_AT_LEAF.id, 1), 1).unwrap();
    assert!(declared.this_operand);
    assert!(super::admit(&view, target(abi::STUB_STRING_CHAR_CODE_AT_LEAF.id, 1), 0).is_none());
    assert!(super::admit(&view, target(abi::STUB_STRING_CHAR_CODE_AT_LEAF.id, 2), 1).is_none());
    assert!(
        super::admit(
            &view,
            target(abi::STUB_COLLECTION_MAP_SET_MUTATING.id, 2),
            2
        )
        .is_none()
    );
    assert!(super::admit(&view, target(u32::MAX, 1), 1).is_none());
    let mut absent = view.clone();
    absent.native_call_layout = JitNativeCallLayout::default();
    assert!(super::admit(&absent, target(abi::STUB_STRING_CHAR_CODE_AT_LEAF.id, 1), 1).is_none());
}
#[test]
fn builder_preserves_evaluated_receiver_operands_and_exact_eager_call_pc() {
    for (id, argc, receiver) in [
        (abi::STUB_STRING_CHAR_CODE_AT_LEAF.id, 1, true),
        (abi::STUB_MATH_MAX_LEAF.id, 2, false),
        (abi::STUB_MATH_ABS_LEAF.id, 1, false),
    ] {
        let view = call(id, argc);
        let built = build(&view);
        let graph = &built.graph;
        let leaf = graph
            .nodes
            .iter()
            .find(|node| matches!(node.kind, Kind::NativeLeaf(_)))
            .unwrap();
        assert_eq!(leaf.inputs.len(), 2);
        assert_eq!(
            graph.node(leaf.inputs[0]).kind,
            Kind::InitialRegister(if receiver { 1 } else { 2 })
        );
        if receiver || argc == 2 {
            assert_eq!(graph.node(leaf.inputs[1]).kind, Kind::InitialRegister(2));
        } else {
            assert!(matches!(
                graph.node(leaf.inputs[1]).kind,
                Kind::ConstTagged(_)
            ));
        }
        let state = graph.frame_state(leaf.eager.unwrap());
        assert_eq!(
            (state.function_id, state.pc, state.byte_pc),
            (701, 0, view.instructions[0].byte_pc)
        );
        assert!(leaf.lazy.is_none());
        let properties = leaf.kind.properties();
        assert!(properties.call && properties.eager_deopt && properties.effectful);
        assert!(
            !properties.may_collect
                && !properties.writes
                && !properties.can_throw
                && !properties.lazy_deopt
        );
        assert!(
            graph
                .nodes
                .iter()
                .any(|node| matches!(node.kind, Kind::CheckNative(73)))
        );
        assert!(
            !graph
                .nodes
                .iter()
                .any(|node| matches!(node.kind, Kind::Generic { .. } | Kind::CallJs { .. }))
        );
    }
    for view in [
        call(abi::STUB_STRING_CHAR_CODE_AT_LEAF.id, 0),
        call(abi::STUB_COLLECTION_MAP_SET_MUTATING.id, 2),
    ] {
        let built = build(&view);
        assert!(
            built
                .graph
                .nodes
                .iter()
                .any(|node| matches!(node.kind, Kind::Generic { .. }))
        );
        assert!(
            !built
                .graph
                .nodes
                .iter()
                .any(|node| matches!(node.kind, Kind::NativeLeaf(_)))
        );
    }
}
fn append(graph: &mut Graph, block: BlockId, kind: Kind, inputs: &[NodeId], repr: Repr) -> NodeId {
    let id = graph.add_node(kind, inputs, repr);
    graph.node_mut(id).block = Some(block);
    graph.block_mut(block).body.push(id);
    id
}
#[test]
fn native_leaf_calls_home_eager_only_gp_fp_and_cfg_live_values_on_both_targets() {
    for target in [registers::AARCH64, registers::X86_64] {
        let mut graph = Graph::default();
        let block = graph.new_block();
        let receiver = append(
            &mut graph,
            block,
            Kind::InitialRegister(0),
            &[],
            Repr::Tagged,
        );
        let argument = append(
            &mut graph,
            block,
            Kind::InitialRegister(1),
            &[],
            Repr::Tagged,
        );
        let eager_gp = append(
            &mut graph,
            block,
            Kind::InitialRegister(2),
            &[],
            Repr::Tagged,
        );
        let eager_fp = append(
            &mut graph,
            block,
            Kind::InitialRegister(3),
            &[],
            Repr::Float64,
        );
        let live = append(
            &mut graph,
            block,
            Kind::InitialRegister(4),
            &[],
            Repr::Tagged,
        );
        let state = graph.add_frame_state(FrameState {
            function_id: 702,
            pc: 9,
            byte_pc: 61,
            register_count: 5,
            registers: vec![
                (0, receiver),
                (1, argument),
                (2, eager_gp),
                (3, eager_fp),
                (4, live),
            ],
            caller: None,
        });
        let leaf = append(
            &mut graph,
            block,
            Kind::NativeLeaf(abi::STUB_STRING_CHAR_CODE_AT_LEAF.id),
            &[receiver, argument],
            Repr::Tagged,
        );
        graph.node_mut(leaf).eager = Some(state);
        let ret = graph.add_node(Kind::Return, &[live], Repr::None);
        graph.node_mut(ret).block = Some(block);
        graph.block_mut(block).control = Some(ret);
        let allocation = regalloc::allocate(&graph, &[block], target);
        let assigned = allocation.node(leaf);
        assert!(
            !assigned.skipped,
            "an unused leaf still owns its eager miss"
        );
        assert_eq!(assigned.result, Some(Location::Gp(target.call_result)));
        assert!(
            assigned
                .inputs
                .iter()
                .all(|slot| matches!(slot, Location::TaggedSlot(_)))
        );
        assert!(
            assigned
                .eager
                .iter()
                .all(|slot| matches!(slot, Location::TaggedSlot(_) | Location::UntaggedSlot(_)))
        );
        assert!(
            assigned.eager_spills.is_empty(),
            "no clobbered-register recovery after C entry"
        );
        assert!(assigned.live_registers.is_empty() && assigned.live_homes.is_empty());
        for value in [receiver, argument, eager_gp, eager_fp, live] {
            assert!(
                allocation.definition_spills.contains(&value),
                "complete call owner for {value:?}"
            );
        }
        assert!(matches!(
            allocation.spill[&eager_fp],
            Location::UntaggedSlot(_)
        ));
        assert!(matches!(
            allocation.spill[&eager_gp],
            Location::TaggedSlot(_)
        ));
    }
}
#[test]
fn nested_native_leaf_has_its_own_descriptor_source_and_complete_miss_recipe() {
    use Operand::{ConstIndex, Imm32, Register};
    let mut body = snapshot(
        703,
        3,
        4,
        vec![
            (Op::LoadLocal, vec![Register(3), Imm32(2)]),
            (Op::LoadLocal, vec![Register(3), Imm32(1)]),
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
    prepare(
        &mut body,
        2,
        target(abi::STUB_STRING_CHAR_CODE_AT_LEAF.id, 1),
    );
    let call_byte_pc = body.instructions[2].byte_pc;
    let mut root = snapshot(
        704,
        4,
        5,
        vec![
            (
                Op::Call,
                vec![
                    Register(4),
                    Register(0),
                    ConstIndex(3),
                    Register(1),
                    Register(2),
                    Register(3),
                ],
            ),
            (Op::ReturnValue, vec![Register(4)]),
        ],
    );
    root.instructions[0].call_attempted = true;
    root.direct_callees.insert(
        root.instructions[0].byte_pc,
        vec![JitDirectCallee {
            plan: JitDirectCallPlan {
                function_id: 703,
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
    root.inline_callees.insert(
        root.instructions[0].byte_pc,
        JitInlineCallee {
            body: Arc::new(body),
        },
    );
    let compiled = crate::graph::compile(
        &root,
        707,
        &crate::entry::TransitionTable::resolve(),
        None,
        true,
    )
    .unwrap();
    assert_eq!(
        compiled.built.inline_views.len(),
        1,
        "the leaf body is actually accepted"
    );
    let (index, node) = compiled
        .built
        .graph
        .nodes
        .iter()
        .enumerate()
        .find(|(_, node)| matches!(node.kind, Kind::NativeLeaf(_)))
        .unwrap();
    assert_eq!(node.origin, 1);
    assert!(node.pc as usize >= root.instructions.len());
    let id = NodeId(index as u32);
    let state = compiled.built.graph.frame_state(node.eager.unwrap());
    assert_eq!(
        (state.function_id, state.pc, state.byte_pc),
        (703, 2, call_byte_pc)
    );
    assert_eq!(
        compiled.built.graph.state_chain(node.eager.unwrap()).len(),
        2
    );
    let exit = compiled
        .emission
        .exits
        .iter()
        .position(|exit| exit.node == id && !exit.lazy)
        .unwrap();
    let descriptor = &compiled.deopt.exits[exit];
    let recipe = compiled.deopt.table.lookup(descriptor.state).unwrap();
    assert_eq!(
        recipe
            .frames
            .iter()
            .map(|f| (f.function_id, f.byte_pc))
            .collect::<Vec<_>>(),
        [(704, root.instructions[1].byte_pc), (703, call_byte_pc)]
    );
    // The restored caller waits on this inlined body and resumes after its
    // call; the innermost leaf resumes at its eager call without replay.
    assert_eq!(descriptor.resume_pcs.as_ref(), &[1, 2]);
    let captured = crate::graph::compile_optimized(
        &root,
        708,
        &crate::entry::TransitionTable::resolve(),
        None,
        Some(crate::artifact::ArtifactRequest {
            identity: otter_vm::JitArtifactIdentity {
                function_name: "nested-pure-receiver".into(),
                module: "nested-pure-receiver.js".into(),
            },
            tier: otter_vm::JitDebugTier::Optimizing,
            entry: otter_vm::JitDebugTarget::Entry,
        }),
        false,
    )
    .unwrap();
    let bundle = captured.artifact.unwrap();
    let json = |name| {
        serde_json::from_slice::<serde_json::Value>(bundle.file(name).unwrap().contents()).unwrap()
    };
    let map = json(otter_vm::JitArtifactFileName::CodeMap);
    let region = map["regions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| {
            r["operation"]
                .as_str()
                .is_some_and(|op| op.contains("NativeLeaf"))
        })
        .unwrap();
    assert_eq!(region["functionId"], 703);
    assert_eq!(region["logicalPc"], 2);
    assert_eq!(region["bytePc"], call_byte_pc);
    let relocations = json(otter_vm::JitArtifactFileName::Relocations);
    assert!(
        relocations["relocations"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |r| r["target"]["id"] == abi::STUB_STRING_CHAR_CODE_AT_LEAF.id
                    && r["target"]["signature"] == "leafValue2"
                    && r["startOffset"].as_u64() >= region["startOffset"].as_u64()
                    && r["startOffset"].as_u64() < region["endOffset"].as_u64()
            )
    );
    let points = json(otter_vm::JitArtifactFileName::Safepoints);
    assert!(
        !points["returnSites"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |site| site["nativeReturnOffset"].as_u64() >= region["startOffset"].as_u64()
                    && site["nativeReturnOffset"].as_u64() < region["endOffset"].as_u64()
            ),
        "no collecting return site at this passive native edge"
    );
}
