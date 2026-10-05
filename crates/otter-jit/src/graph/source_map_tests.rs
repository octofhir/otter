//! Source attribution proofs for actual emitted nested Graph regions.
//!
//! # Contents
//! - Nested arithmetic splices with different local instruction geometries.
//! - Exact region ownership and disabled-capture compilation parity.
//! - Generic-owning callee abandonment with accepted nested specialized bodies.
//!
//! # Invariants
//! Expected source positions come from the accepted graph origins and each
//! origin's baked bytecode, including a callee PC beyond the root's body.
//! These tests inspect emitted artifacts; they do not claim execution of the
//! synthetic callee linkage.
//!
//! # See also
//! - [`super::metadata`] for shared semantic source lookup.
//! - [`super::compile_optimized`] for opt-in artifact construction.

use std::sync::Arc;

use otter_bytecode::{FunctionCodeBuilder, Op, Operand, encoding::measure_wordcode_function};
use otter_vm::jit::{
    JitDirectCallPlan, JitDirectCallThisMode, JitDirectCallee, JitTestInstruction,
};
use otter_vm::jit_feedback::{ARITH_INT32, ArithFeedback};
use otter_vm::native_abi::NativeFrameKind;
use otter_vm::{
    JitArtifactFileName, JitArtifactIdentity, JitCompileSnapshot, JitDebugTarget, JitDebugTier,
    JitFunctionCode, JitInlineCallee,
};

fn snapshot(
    id: u32,
    params: u16,
    registers: u16,
    instructions: Vec<(Op, Vec<Operand>)>,
) -> JitCompileSnapshot {
    let mut code = FunctionCodeBuilder::new();
    for (op, operands) in &instructions {
        code.push(*op, operands);
    }
    let layout = measure_wordcode_function(&code.finish()).unwrap();
    JitCompileSnapshot::without_feedback(
        id,
        params,
        registers,
        instructions
            .into_iter()
            .zip(layout.instr_to_byte_pc)
            .enumerate()
            .map(|(pc, ((op, operands), byte_pc))| {
                JitTestInstruction::new(op, pc as u32, byte_pc, operands)
            })
            .collect(),
    )
}

fn offer(caller: &mut JitCompileSnapshot, body: Arc<JitCompileSnapshot>) {
    offer_at(caller, 0, body);
}

fn offer_at(caller: &mut JitCompileSnapshot, pc: usize, body: Arc<JitCompileSnapshot>) {
    caller.instructions[pc].call_attempted = true;
    let byte_pc = caller.instructions[pc].byte_pc;
    caller.direct_callees.insert(
        byte_pc,
        vec![JitDirectCallee {
            plan: JitDirectCallPlan {
                function_id: body.code_block.id,
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
        .insert(byte_pc, JitInlineCallee { body });
}

#[test]
fn nested_native_regions_use_their_own_function_and_byte_pc() {
    use Operand::{ConstIndex, Register};
    let mut leaf = snapshot(
        91,
        1,
        2,
        vec![
            (Op::Neg, vec![Register(1), Register(0)]),
            (Op::Neg, vec![Register(1), Register(1)]),
            (Op::Neg, vec![Register(1), Register(1)]),
            (Op::ReturnValue, vec![Register(1)]),
        ],
    );
    for pc in 0..3 {
        leaf.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
    }
    let mut middle = snapshot(
        92,
        2,
        4,
        vec![
            (
                Op::Call,
                vec![Register(2), Register(0), ConstIndex(1), Register(1)],
            ),
            (Op::Add, vec![Register(3), Register(2), Register(1)]),
            (Op::ReturnValue, vec![Register(3)]),
        ],
    );
    middle.seed_arith_feedback_for_test(1, ArithFeedback::from_bits(ARITH_INT32));
    offer(&mut middle, Arc::new(leaf));
    let mut root = snapshot(
        90,
        3,
        4,
        vec![
            (
                Op::Call,
                vec![
                    Register(3),
                    Register(0),
                    ConstIndex(2),
                    Register(1),
                    Register(2),
                ],
            ),
            (Op::ReturnValue, vec![Register(3)]),
        ],
    );
    offer(&mut root, Arc::new(middle));
    let transitions = crate::entry::TransitionTable::resolve();
    let compiled = super::compile(&root, 7001, &transitions, None, false).unwrap();
    assert_eq!(
        compiled.built.inline_views.len(),
        2,
        "both actual splices accepted"
    );
    assert_eq!(
        compiled
            .built
            .graph
            .inlined
            .iter()
            .map(|body| (body.function_id, body.parent))
            .collect::<Vec<_>>(),
        [(92, 0), (91, 1)],
        "the accepted middle and leaf retain their declared ancestry"
    );
    let request = crate::artifact::ArtifactRequest {
        identity: JitArtifactIdentity {
            function_name: "nested-source-map".into(),
            module: "nested-source-map.js".into(),
        },
        tier: JitDebugTier::Optimizing,
        entry: JitDebugTarget::Entry,
    };
    let captured =
        super::compile_optimized(&root, 7002, &transitions, None, Some(request), false).unwrap();
    let disabled = super::compile_optimized(&root, 7003, &transitions, None, None, false).unwrap();
    assert!(disabled.artifact.is_none());
    assert_eq!(disabled.ir_node_count, captured.ir_node_count);
    assert_eq!(disabled.code.code_len(), captured.code.code_len());
    assert_eq!(
        disabled.code.spliced_functions(),
        captured.code.spliced_functions()
    );
    assert!(disabled.diagnostics.is_empty() && captured.diagnostics.is_empty());
    let artifact = captured.artifact.unwrap();
    let map: serde_json::Value = serde_json::from_slice(
        artifact
            .file(JitArtifactFileName::CodeMap)
            .unwrap()
            .contents(),
    )
    .unwrap();
    let regions = map["regions"].as_array().unwrap();
    let mut origins = std::collections::BTreeSet::new();
    let mut beyond_root = false;
    let mut different_byte_pc = false;
    for region in regions
        .iter()
        .filter(|region| region["kind"] == "instruction")
    {
        let id = super::ir::NodeId(region["operationIndex"].as_u64().unwrap() as u32);
        let node = compiled.built.graph.node(id);
        let function_id = [90, 92, 91][usize::from(node.origin)];
        let source = std::iter::once(&root)
            .chain(compiled.built.inline_views.iter().map(Arc::as_ref))
            .find(|view| view.code_block.id == function_id)
            .unwrap();
        assert_eq!(region["functionId"], function_id);
        assert_eq!(region["operation"], format!("v{} {:?}", id.0, node.kind));
        assert_eq!(region["logicalPc"], node.pc);
        assert_eq!(
            region["bytePc"],
            source.instructions[node.pc as usize].byte_pc
        );
        if node.origin != 0 {
            origins.insert(node.origin);
            beyond_root |= node.pc as usize >= root.instructions.len();
            different_byte_pc |=
                root.instructions
                    .get(node.pc as usize)
                    .is_some_and(|instruction| {
                        instruction.byte_pc != source.instructions[node.pc as usize].byte_pc
                    });
        }
    }
    assert_eq!(origins, [1, 2].into_iter().collect());
    assert!(
        beyond_root,
        "an emitted leaf PC exceeds the root's instruction count"
    );
    assert!(
        different_byte_pc,
        "equal logical PCs have different encoded positions"
    );
}

#[test]
fn generic_window_owner_survives_nested_splices_and_abandoned_inline_source() {
    use super::ir::Kind;
    use Operand::{ConstIndex, Imm32, Register};
    let typeof_number = otter_bytecode::TypeOfKind::Number as i32;
    let mut leaf = snapshot(
        201,
        1,
        2,
        vec![
            (Op::Neg, vec![Register(1), Register(0)]),
            (Op::ReturnValue, vec![Register(1)]),
        ],
    );
    leaf.seed_arith_feedback_for_test(0, ArithFeedback::from_bits(ARITH_INT32));
    let mut middle = snapshot(
        202,
        2,
        4,
        vec![
            (
                Op::Call,
                vec![Register(2), Register(0), ConstIndex(1), Register(1)],
            ),
            (Op::Add, vec![Register(3), Register(2), Register(1)]),
            (Op::ReturnValue, vec![Register(3)]),
        ],
    );
    middle.seed_arith_feedback_for_test(1, ArithFeedback::from_bits(ARITH_INT32));
    offer(&mut middle, Arc::new(leaf));
    // This candidate builds a specialized node before its Generic instruction
    // requires a real baseline activation. The whole attempted body must roll
    // back, including the otherwise valid preceding arithmetic node.
    let mut needs_window = snapshot(
        203,
        1,
        2,
        vec![
            (Op::Neg, vec![Register(1), Register(0)]),
            (
                Op::TestTypeOf,
                vec![Register(1), Register(1), Imm32(typeof_number)],
            ),
            (Op::ReturnValue, vec![Register(1)]),
        ],
    );
    needs_window.seed_arith_feedback_for_test(0, ArithFeedback::from_bits(ARITH_INT32));
    let mut root = snapshot(
        200,
        4,
        7,
        vec![
            (
                Op::TestTypeOf,
                vec![Register(4), Register(3), Imm32(typeof_number)],
            ),
            (
                Op::Call,
                vec![
                    Register(5),
                    Register(0),
                    ConstIndex(2),
                    Register(1),
                    Register(3),
                ],
            ),
            (
                Op::Call,
                vec![Register(6), Register(2), ConstIndex(1), Register(5)],
            ),
            (Op::ReturnValue, vec![Register(6)]),
        ],
    );
    offer_at(&mut root, 1, Arc::new(middle));
    offer_at(&mut root, 2, Arc::new(needs_window));
    let transitions = crate::entry::TransitionTable::resolve();
    let mut compiled = super::compile(&root, 7101, &transitions, None, false).unwrap();
    assert_eq!(
        compiled.built.inline_views.len(),
        2,
        "middle and leaf actually spliced"
    );
    assert_eq!(
        compiled
            .built
            .graph
            .inlined
            .iter()
            .map(|body| (body.function_id, body.parent))
            .collect::<Vec<_>>(),
        [(202, 0), (201, 1)]
    );
    let graph = &compiled.built.graph;
    let generic = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| matches!(node.kind, Kind::Generic { .. }))
        .map(|(index, node)| (super::ir::NodeId(index as u32), node))
        .collect::<Vec<_>>();
    assert_eq!(
        generic.len(),
        1,
        "only the root has a physical baseline window"
    );
    let (generic_id, data) = generic[0];
    assert_eq!(data.origin, 0);
    assert_eq!(data.pc, 0);
    assert!(matches!(data.kind, Kind::Generic { pc: 0, .. }));
    assert_eq!(
        super::metadata::source_view(&root, &compiled.built.inline_views, graph, generic_id)
            .code_block
            .id,
        200
    );
    assert!(
        graph.nodes.iter().any(|node| matches!(node.kind,
        Kind::CallJs { pc: 2, plan: crate::call_linkage::CallPlan::Bytecode(plan), .. } if plan.function_id == 203)),
        "abandoned candidate retains the actual call boundary"
    );
    assert!(
        graph
            .nodes
            .iter()
            .filter(|node| node.origin != 0)
            .all(|node| !matches!(node.kind, Kind::Generic { .. }))
    );
    super::metadata::validate_generic_sources(graph).unwrap();
    let request = crate::artifact::ArtifactRequest {
        identity: JitArtifactIdentity {
            function_name: "generic-source-owner".into(),
            module: "generic-source-owner.js".into(),
        },
        tier: JitDebugTier::Optimizing,
        entry: JitDebugTarget::Entry,
    };
    let captured =
        super::compile_optimized(&root, 7102, &transitions, None, Some(request), false).unwrap();
    let artifact = captured.artifact.unwrap();
    let map: serde_json::Value = serde_json::from_slice(
        artifact
            .file(JitArtifactFileName::CodeMap)
            .unwrap()
            .contents(),
    )
    .unwrap();
    let region = map["regions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|region| region["kind"] == "instruction" && region["operationIndex"] == generic_id.0)
        .expect("actual emitted Generic region");
    assert_eq!(region["functionId"], 200);
    assert_eq!(region["logicalPc"], 0);
    assert_eq!(region["bytePc"], root.instructions[0].byte_pc);
    assert_eq!(
        region["operation"],
        format!("v{} {:?}", generic_id.0, data.kind)
    );
    assert!(
        region["endOffset"].as_u64().unwrap() > region["startOffset"].as_u64().unwrap(),
        "root Generic actually emits its baseline operation"
    );
    // Corrupt only its source ownership after the normal production build.
    // Both actual target emission entries must decline before opening code.
    compiled.built.graph.node_mut(generic_id).origin = 1;
    let slots = super::frame::SlotLayout::of(&compiled.allocation).unwrap();
    let deopt = std::ptr::from_ref(compiled.deopt.as_ref());
    #[cfg(target_arch = "aarch64")]
    let rejected = super::arm64::emit(
        &root,
        &compiled.built,
        &compiled.allocation,
        &transitions,
        7103,
        deopt,
        &compiled.plan,
        slots,
        false,
        false,
    );
    #[cfg(target_arch = "x86_64")]
    let rejected = super::x86_64::emit(
        &root,
        &compiled.built,
        &compiled.allocation,
        &transitions,
        7103,
        deopt,
        &compiled.plan,
        slots,
        false,
    );
    assert!(matches!(
        rejected,
        Err(crate::Unsupported::OperandShape(
            "inlined Generic has no baseline register window"
        ))
    ));
}
