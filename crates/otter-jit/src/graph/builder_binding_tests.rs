//! Source-owned global reads across nested JavaScript body splices.
//!
//! # Contents
//! - Verifier-valid global reads before and after a nested call.
//! - Exact inner proof selection and complete eager source chains.
//! - Unproved and previously refused sites retain whole-splice refusal.
//!
//! # Invariants
//! Synthetic permanent-cell offsets describe compiler metadata only. Real
//! executable guards, realm ownership and moving roots belong to runtime
//! proofs; these tests neither call fabricated cells nor claim GC evidence.
//!
//! # See also
//! - `otter-runtime/tests/jit_graph_sloppy_receiver_inline.rs`.
//! - `otter-runtime/tests/jit_global_access.rs`.

use std::{rc::Rc, sync::Arc};

use otter_bytecode::{
    BytecodeModule, Constant, Function, FunctionCodeBuilder, Op, Operand, SourceKind,
};
use otter_vm::{
    ExecutionContext, JitCompileSnapshot, JitInlineCallee,
    jit::{BindingHitProof, JitDirectCallPlan, JitDirectCallThisMode, JitDirectCallee},
    native_abi::{ExitReason, NativeFrameKind},
};

use super::{Kind, build};
use crate::graph::{BaselineSupport, bytecode::Analysis, ir::Repr};

fn function(id: u32, code: &[(Op, Vec<Operand>)]) -> Function {
    let mut bytes = FunctionCodeBuilder::new();
    for (op, operands) in code {
        bytes.push(*op, operands);
    }
    Function {
        id,
        name: format!("binding{id}"),
        locals: 6,
        is_strict: true,
        code: bytes.finish(),
        ..Function::default()
    }
}

fn views() -> Vec<JitCompileSnapshot> {
    use Operand::{ConstIndex as C, Register as R};
    let context = ExecutionContext::from_module(
        BytecodeModule {
            module: "binding-inline-geometry".into(),
            source_kind: SourceKind::JavaScript,
            functions: vec![
                function(0, &[(Op::ReturnUndefined, vec![])]),
                function(
                    1,
                    &[
                        (Op::LoadGlobalOrThrow, vec![R(0), C(0)]),
                        (Op::Call, vec![R(1), R(0), C(0)]),
                        (Op::ReturnValue, vec![R(1)]),
                    ],
                ),
                function(
                    2,
                    &[
                        (Op::LoadGlobalOrUndefined, vec![R(0), C(1)]),
                        (Op::Call, vec![R(1), R(0), C(0)]),
                        (Op::LoadGlobalOrThrow, vec![R(2), C(2)]),
                        (Op::ReturnValue, vec![R(1)]),
                    ],
                ),
                function(
                    3,
                    &[
                        (Op::LoadGlobalOrThrow, vec![R(0), C(2)]),
                        (Op::ReturnValue, vec![R(0)]),
                    ],
                ),
            ],
            constants: ["middle", "base", "observed"]
                .into_iter()
                .map(|name| Constant::String {
                    utf16: name.encode_utf16().collect(),
                })
                .collect(),
            function_source: None,
            template_sites: vec![],
            module_resolutions: vec![],
            module_inits: vec![],
        },
        Default::default(),
    )
    .expect("verified global binding source");
    (1..=3)
        .map(|fid| context.jit_compile_snapshot(fid).unwrap())
        .collect()
}

fn offer(caller: &mut JitCompileSnapshot, body: JitCompileSnapshot) {
    let pc = caller.instructions[1].byte_pc;
    caller.instructions[1].call_attempted = true;
    caller.direct_callees.insert(
        pc,
        vec![JitDirectCallee {
            plan: JitDirectCallPlan {
                function_id: body.code_block.id,
                code_object_id: u64::from(body.code_block.id),
                entry_cell: 0,
                tier: NativeFrameKind::Baseline,
                this_mode: JitDirectCallThisMode::StrictOrLexical,
                is_derived_constructor: false,
                call_flags: body.code_block.call_flags(),
                callee_cell: 0,
            },
            receiver_allocation: None,
        }],
    );
    caller.inline_callees.insert(
        pc,
        JitInlineCallee {
            body: Arc::new(body),
        },
    );
}

#[test]
fn nested_global_reads_keep_their_source_proofs_and_before_load_exit_chain() {
    for (proof, refused) in [(true, false), (false, false), (true, true)] {
        let mut snapshots = views();
        for view in &mut snapshots {
            for instruction in &view.instructions {
                if matches!(
                    instruction.op(&view.code_block),
                    Op::LoadGlobalOrThrow | Op::LoadGlobalOrUndefined
                ) {
                    view.binding_hit_proofs.insert(
                        instruction.byte_pc,
                        BindingHitProof::GlobalLexical {
                            cell_offset: view.code_block.id * 32 + instruction.byte_pc,
                            writable: true,
                        },
                    );
                }
            }
        }
        let base = snapshots.remove(2);
        let mut middle = snapshots.remove(1);
        let middle_pc = middle.instructions[0].byte_pc;
        if !proof {
            middle.binding_hit_proofs.remove(&middle_pc);
        }
        if refused {
            middle.optimized_exit_reasons.insert(
                0,
                std::collections::BTreeSet::from([ExitReason::ShapeGuard]),
            );
        }
        offer(&mut middle, base);
        let mut caller = snapshots.remove(0);
        offer(&mut caller, middle);
        let analysis = Rc::new(Analysis::build(&caller).expect("binding analysis"));
        let built = build(
            &caller,
            &analysis,
            &BaselineSupport {
                supported: vec![true; caller.instructions.len()],
            },
            None,
        )
        .expect("binding graph");
        if !proof || refused {
            assert!(built.graph.inlined.is_empty());
            assert!(
                built
                    .graph
                    .nodes
                    .iter()
                    .any(|node| matches!(node.kind, Kind::CallJs { .. }))
            );
            continue;
        }
        assert_eq!(built.graph.inlined.len(), 2);
        assert_eq!(
            (
                built.graph.inlined[0].function_id,
                built.graph.inlined[0].parent
            ),
            (2, 0)
        );
        assert_eq!(
            (
                built.graph.inlined[1].function_id,
                built.graph.inlined[1].parent
            ),
            (3, 1)
        );
        assert!(
            !built
                .graph
                .nodes
                .iter()
                .any(|node| matches!(node.kind, Kind::Generic { .. }))
        );
        let reads: Vec<_> = built
            .graph
            .nodes
            .iter()
            .filter(|node| matches!(node.kind, Kind::LoadGlobalBinding(_)))
            .collect();
        assert_eq!(reads.len(), 4, "the post-call read remains observable");
        for node in reads {
            let Kind::LoadGlobalBinding(byte_pc) = node.kind else {
                unreachable!()
            };
            let view = if node.origin == 0 {
                &caller
            } else {
                &built.inline_views[usize::from(node.origin - 1)]
            };
            let state = built
                .graph
                .frame_state(node.eager.expect("before-load guard"));
            assert_eq!(
                (state.function_id, state.pc, state.byte_pc),
                (view.code_block.id, node.pc, byte_pc)
            );
            assert_eq!(node.repr, Repr::Tagged);
            assert_eq!(
                view.binding_hit_proofs[&byte_pc],
                BindingHitProof::GlobalLexical {
                    cell_offset: state.function_id * 32 + byte_pc,
                    writable: true,
                }
            );
            let mut chain = state;
            let mut depth = 0;
            while let Some(frame) = chain.caller {
                chain = built.graph.frame_state(frame.state);
                depth += 1;
            }
            assert_eq!(depth, usize::from(node.origin));
            assert_eq!(chain.function_id, 1);
        }
    }
}
