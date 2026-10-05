//! Receiver conversion proofs at explicit JavaScript body splices.
//!
//! # Contents
//! - Real CodeBlock call flags for sloppy, strict and lexical entry bindings.
//! - Object allocations and nested bindings through existing inline recipes.
//! - Evaluated Function.prototype.call inputs and exact identity exits.
//!
//! # Invariants
//! Function metadata is verified by the ordinary executable owner. Synthetic
//! call cells describe compiler geometry; the runtime fixture proves real
//! current entries, mutations, collection and primitive/nullish conversion.
//!
//! # See also
//! - `otter-runtime/tests/jit_graph_sloppy_receiver_inline.rs`.

use std::{rc::Rc, sync::Arc};

use otter_bytecode::{BytecodeModule, Function, FunctionCodeBuilder, Op, Operand, SourceKind};
use otter_vm::jit::{
    JitDirectCallPlan, JitDirectCallThisMode, JitDirectCallee, JitFunctionPrototypeCall,
    JitFunctionPrototypeCallSite,
};
use otter_vm::native_abi::{FUNCTION_CALL_NO_RECEIVER_CONVERSION, NativeFrameKind};
use otter_vm::{ExecutionContext, JitCompileSnapshot, JitInlineCallee};

use super::{Kind, build};
use crate::graph::{BaselineSupport, bytecode::Analysis};

fn function(
    id: u32,
    strict: bool,
    arrow: bool,
    params: u16,
    code: &[(Op, Vec<Operand>)],
) -> Function {
    let mut bytes = FunctionCodeBuilder::new();
    for (op, operands) in code {
        bytes.push(*op, operands);
    }
    Function {
        id,
        name: format!("receiver{id}"),
        locals: 5,
        param_count: params,
        is_strict: strict,
        is_arrow: arrow,
        code: bytes.finish(),
        ..Function::default()
    }
}
fn views(caller: Function, middle: Function, base: Function) -> Vec<JitCompileSnapshot> {
    let main = function(0, false, false, 0, &[(Op::ReturnUndefined, vec![])]);
    let context = ExecutionContext::from_module(
        BytecodeModule {
            module: "receiver-inline-geometry".into(),
            source_kind: SourceKind::JavaScript,
            functions: vec![main, caller, middle, base],
            constants: vec![],
            function_source: None,
            template_sites: vec![],
            module_resolutions: vec![],
            module_inits: vec![],
        },
        Default::default(),
    )
    .expect("verified receiver function metadata");
    (1..=3)
        .map(|fid| context.jit_compile_snapshot(fid).unwrap())
        .collect()
}
fn receiver_body(id: u32, strict: bool) -> Function {
    function(
        id,
        strict,
        false,
        0,
        &[
            (Op::LoadThis, vec![Operand::Register(2)]),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
    )
}
fn plan(body: &JitCompileSnapshot, mode: JitDirectCallThisMode) -> JitDirectCallee {
    JitDirectCallee {
        plan: JitDirectCallPlan {
            function_id: body.code_block.id,
            code_object_id: u64::from(body.code_block.id),
            entry_cell: 0,
            tier: NativeFrameKind::Baseline,
            this_mode: mode,
            is_derived_constructor: false,
            call_flags: body.code_block.call_flags(),
            callee_cell: 0,
        },
        receiver_allocation: None,
    }
}
fn prepare(
    caller: &mut JitCompileSnapshot,
    pc: usize,
    body: Arc<JitCompileSnapshot>,
    mode: JitDirectCallThisMode,
) {
    let byte_pc = caller.instructions[pc].byte_pc;
    caller.instructions[pc].call_attempted = true;
    caller
        .direct_callees
        .insert(byte_pc, vec![plan(&body, mode)]);
    caller
        .inline_callees
        .insert(byte_pc, JitInlineCallee { body });
}
fn graph(view: &JitCompileSnapshot) -> super::Built {
    let analysis = Rc::new(Analysis::build(view).unwrap());
    build(
        view,
        &analysis,
        &BaselineSupport {
            supported: vec![true; view.instructions.len()],
        },
        None,
    )
    .unwrap()
}
fn own_receiver(strict: bool, arrow: bool) -> Function {
    use Operand::{ConstIndex as C, Register as R};
    function(
        1,
        strict,
        arrow,
        1,
        &[
            (Op::LoadThis, vec![R(1)]),
            (Op::CallWithThis, vec![R(2), R(0), R(1), C(0)]),
            (Op::ReturnValue, vec![R(2)]),
        ],
    )
}
#[test]
fn only_actual_converted_entry_this_proves_an_object() {
    for (strict, arrow, admitted) in [
        (false, false, true),
        (true, false, false),
        (false, true, false),
    ] {
        let mut snapshots = views(
            own_receiver(strict, arrow),
            receiver_body(2, false),
            receiver_body(3, false),
        );
        let body = Arc::new(snapshots.remove(1));
        let mut caller = snapshots.remove(0);
        assert_eq!(
            caller.code_block.call_flags() & FUNCTION_CALL_NO_RECEIVER_CONVERSION == 0,
            admitted
        );
        prepare(&mut caller, 1, body, JitDirectCallThisMode::SloppyGlobal);
        let built = graph(&caller);
        assert_eq!(built.graph.inlined.len(), usize::from(admitted));
        assert_eq!(
            built
                .graph
                .nodes
                .iter()
                .filter(|n| matches!(n.kind, Kind::CallJs { .. }))
                .count(),
            usize::from(!admitted)
        );
        assert_eq!(
            built
                .graph
                .nodes
                .iter()
                .filter(|n| n.kind == Kind::LoadThis)
                .count(),
            1
        );
    }
}
#[test]
fn object_allocations_prove_this_but_unknown_arguments_do_not() {
    use Operand::{ConstIndex as C, Register as R};
    for allocation in [
        Some((Op::NewObject, vec![R(1)])),
        Some((Op::NewArray, vec![R(1), C(0)])),
        None,
    ] {
        let mut code = vec![];
        if let Some(allocation) = allocation.clone() {
            code.push(allocation);
        }
        code.push((Op::CallWithThis, vec![R(2), R(0), R(1), C(0)]));
        code.push((Op::ReturnValue, vec![R(2)]));
        let mut snapshots = views(
            function(1, false, false, 2, &code),
            receiver_body(2, false),
            receiver_body(3, false),
        );
        let body = Arc::new(snapshots.remove(1));
        let mut caller = snapshots.remove(0);
        prepare(
            &mut caller,
            usize::from(allocation.is_some()),
            body,
            JitDirectCallThisMode::SloppyGlobal,
        );
        let built = graph(&caller);
        assert_eq!(built.graph.inlined.len(), usize::from(allocation.is_some()));
    }
}
#[test]
fn inlined_load_this_inherits_the_actual_binding_and_nested_source() {
    use Operand::{ConstIndex as C, Register as R};
    for proved in [false, true] {
        let caller = if proved {
            own_receiver(false, false)
        } else {
            function(
                1,
                true,
                false,
                2,
                &[
                    (Op::CallWithThis, vec![R(2), R(0), R(1), C(0)]),
                    (Op::ReturnValue, vec![R(2)]),
                ],
            )
        };
        let middle = function(
            2,
            true,
            false,
            0,
            &[
                (Op::LoadThis, vec![R(1)]),
                (Op::LoadSelf, vec![R(0)]),
                (Op::CallWithThis, vec![R(2), R(0), R(1), C(0)]),
                (Op::ReturnValue, vec![R(2)]),
            ],
        );
        let mut snapshots = views(caller, middle, receiver_body(3, false));
        let base = Arc::new(snapshots.remove(2));
        let mut middle = snapshots.remove(1);
        prepare(&mut middle, 2, base, JitDirectCallThisMode::SloppyGlobal);
        let mut caller = snapshots.remove(0);
        prepare(
            &mut caller,
            usize::from(proved),
            Arc::new(middle),
            JitDirectCallThisMode::StrictOrLexical,
        );
        let built = graph(&caller);
        assert_eq!(built.graph.inlined.len(), if proved { 2 } else { 1 });
        assert_eq!(
            built
                .graph
                .nodes
                .iter()
                .filter(|n| n.kind == Kind::LoadThis)
                .count(),
            usize::from(proved)
        );
        if proved {
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
            let guard = built
                .graph
                .nodes
                .iter()
                .find(|n| matches!(n.kind, Kind::CheckFunction { function_id: 3, .. }))
                .unwrap();
            let frame = built.graph.frame_state(guard.eager.unwrap());
            assert_eq!((frame.function_id, frame.pc), (2, 2));
            assert!(frame.caller.is_some());
        }
    }
}
#[test]
fn evaluated_call_intrinsic_and_target_identity_keep_the_original_call_exit() {
    use Operand::{ConstIndex as C, Register as R};
    let caller = function(
        1,
        false,
        false,
        2,
        &[
            (Op::LoadThis, vec![R(2)]),
            (Op::CallWithThis, vec![R(3), R(0), R(1), C(1), R(2)]),
            (Op::ReturnValue, vec![R(3)]),
        ],
    );
    let mut snapshots = views(caller, receiver_body(2, false), receiver_body(3, false));
    let body = Arc::new(snapshots.remove(1));
    let mut caller = snapshots.remove(0);
    let byte_pc = caller.instructions[1].byte_pc;
    caller.instructions[1].call_attempted = true;
    caller.function_prototype_calls.insert(
        byte_pc,
        JitFunctionPrototypeCallSite {
            proof: JitFunctionPrototypeCall {
                lookup: None,
                call_native_ref: 49,
            },
            callee: plan(&body, JitDirectCallThisMode::SloppyGlobal),
        },
    );
    caller
        .inline_callees
        .insert(byte_pc, JitInlineCallee { body });
    let built = graph(&caller);
    assert_eq!(built.graph.inlined.len(), 1);
    for guard in built.graph.nodes.iter().filter(|n| {
        matches!(
            n.kind,
            Kind::CheckFunctionPrototypeCall(_) | Kind::CheckFunction { .. }
        )
    }) {
        let frame = built.graph.frame_state(guard.eager.unwrap());
        assert_eq!(
            (frame.function_id, frame.pc, frame.byte_pc),
            (1, 1, byte_pc)
        );
    }
    let intrinsic = built
        .graph
        .nodes
        .iter()
        .find(|n| matches!(n.kind, Kind::CheckFunctionPrototypeCall(_)))
        .unwrap();
    assert_eq!(
        built.graph.node(intrinsic.inputs[0]).kind,
        Kind::InitialRegister(0)
    );
    let target = built
        .graph
        .nodes
        .iter()
        .find(|n| matches!(n.kind, Kind::CheckFunction { function_id: 2, .. }))
        .unwrap();
    assert_eq!(
        built.graph.node(target.inputs[0]).kind,
        Kind::InitialRegister(1)
    );
    assert!(
        !built
            .graph
            .nodes
            .iter()
            .any(|n| matches!(n.kind, Kind::CallJs { .. }))
    );
}
