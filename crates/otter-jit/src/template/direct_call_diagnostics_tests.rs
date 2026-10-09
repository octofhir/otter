//! Independent emitted-code proofs for Template direct-call diagnostics.
//!
//! # Contents
//! - Plain calls join typed outcomes to function-cell relocations and call bytes.
//! - Monomorphic/polymorphic methods prove each exact emitted target on both targets.
//! - Constructor links distinguish base, derived and super semantics.
//!
//! # Invariants
//! - Synthetic target addresses are only encoded, never dereferenced or executed.
//! - Proofs consume finalized artifacts, not the diagnostic producer's helper.
//! - Optional diagnostics do not change code bytes or default-off ownership.
//!
//! # See also
//! - [`super::seed_direct_call_events`] for initial target outcomes.
//! - [`crate::artifact::relocation`] for typed address materializations.

use otter_bytecode::{Op, Operand};
use otter_vm::{
    JitArtifactBundle, JitArtifactFileName, JitCompileSnapshot, JitCompilerDiagnostic,
    JitDebugTarget, JitDebugTier, JitDirectCallKind, JitDirectCallLoweringOutcome,
    jit::{
        JitDirectCallPlan, JitDirectCallThisMode, JitDirectCallee, JitDirectMethod, JitMethodGuard,
        JitTestInstruction,
    },
    native_abi::{FUNCTION_CALL_NO_RECEIVER_CONVERSION, NativeFrameKind},
};
use serde_json::Value;

const CALL_PC: u32 = 0;
const CALL_BYTE_PC: u32 = 0;

fn view(method: bool) -> JitCompileSnapshot {
    let (op, operands) = if method {
        (
            Op::CallMethodValue,
            vec![
                Operand::Register(0),
                Operand::Register(1),
                Operand::ConstIndex(0),
                Operand::ConstIndex(1),
                Operand::Register(2),
            ],
        )
    } else {
        (
            Op::Call,
            vec![
                Operand::Register(0),
                Operand::Register(1),
                Operand::ConstIndex(1),
                Operand::Register(2),
            ],
        )
    };
    let mut view = JitCompileSnapshot::without_feedback(
        7,
        0,
        4,
        vec![
            JitTestInstruction::new(op, CALL_PC, CALL_BYTE_PC, operands),
            JitTestInstruction::new(Op::ReturnValue, 1, 32, vec![Operand::Register(0)]),
        ],
    );
    // A syntactic method guard needs the cage base; no emitted code is run.
    view.cage_base = 0x1000;
    view
}

fn target(fid: u32, generation: u64, tier: NativeFrameKind) -> JitDirectCallee {
    JitDirectCallee {
        plan: JitDirectCallPlan {
            function_id: fid,
            code_object_id: generation,
            entry_cell: 0x1122_3344 + u64::from(fid) * 16,
            tier,
            this_mode: JitDirectCallThisMode::StrictOrLexical,
            is_derived_constructor: false,
            call_flags: FUNCTION_CALL_NO_RECEIVER_CONVERSION,
            callee_cell: 0,
        },
        receiver_allocation: None,
    }
}

fn method(index: u32, count: u32) -> JitDirectMethod {
    let fid = 11 + index;
    JitDirectMethod {
        target_index: index,
        target_count: count,
        guard: JitMethodGuard {
            method_fid: fid,
            recv_shape: 0x80 + index * 8,
            prototype_validity: None,
            holder_root: 0,
            method_field: otter_vm::object::FieldLocation::inline(0),
        },
        callee: target(fid, 91 + u64::from(index), NativeFrameKind::Baseline),
        body: None,
    }
}

fn compile(
    view: &JitCompileSnapshot,
    capture_events: bool,
) -> crate::artifact::NativeCompileOutput<super::TemplateCode> {
    super::compile_with_artifacts(
        view,
        3,
        &crate::entry::TransitionTable::resolve(),
        Some(crate::artifact::ArtifactRequest {
            identity: otter_vm::JitArtifactIdentity {
                function_name: "directDiagnostics".into(),
                module: "direct-diagnostics.js".into(),
            },
            tier: JitDebugTier::Template,
            entry: JitDebugTarget::Entry,
        }),
        capture_events,
    )
    .expect("synthetic call compiles")
}

fn json(bundle: &JitArtifactBundle, name: JitArtifactFileName) -> Value {
    serde_json::from_slice(bundle.file(name).expect("artifact file").contents())
        .expect("artifact JSON")
}

fn assert_generated(
    output: &crate::artifact::NativeCompileOutput<super::TemplateCode>,
    kind: JitDirectCallKind,
    target: &JitDirectCallee,
    index: u32,
    count: u32,
) {
    let diagnostics: Vec<_> = output
        .diagnostics
        .iter()
        .filter_map(|diagnostic| match diagnostic {
            JitCompilerDiagnostic::DirectCallLowered {
                call_kind,
                instruction_pc,
                byte_pc,
                callee_function_id,
                target_index,
                target_count,
                outcome,
            } if *callee_function_id == target.plan.function_id => Some((
                *call_kind,
                *instruction_pc,
                *byte_pc,
                *target_index,
                *target_count,
                *outcome,
            )),
            _ => None,
        })
        .collect();
    let tier = match target.plan.tier {
        NativeFrameKind::Baseline => JitDebugTier::Template,
        NativeFrameKind::Optimizing => JitDebugTier::Optimizing,
        _ => JitDebugTier::Interpreter,
    };
    assert_eq!(
        diagnostics,
        vec![(
            kind,
            CALL_PC,
            CALL_BYTE_PC,
            index,
            count,
            JitDirectCallLoweringOutcome::Generated {
                code_object_id: target.plan.code_object_id,
                target_tier: tier,
                this_mode: target.plan.this_mode
            }
        )]
    );
    let bundle = output.artifact.as_deref().expect("artifact capture");
    let relocations = json(bundle, JitArtifactFileName::Relocations);
    let links: Vec<_> = relocations["relocations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|relocation| {
            // A literal-pool word is the address the load site reads.
            relocation["target"]["kind"] == "functionEntryCell"
                && relocation["target"]["functionId"] == target.plan.function_id
                && relocation["form"] != "literalWord"
        })
        .collect();
    assert_eq!(links.len(), 1, "each target has one actual Known edge");
    let start = links[0]["startOffset"].as_u64().unwrap();
    let end = links[0]["endOffset"].as_u64().unwrap();
    let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
    #[cfg(target_arch = "aarch64")]
    {
        // LDR x8,[x8]; LDR x16,[x8]; BLR x16: current-generation dispatch.
        let expected: Vec<_> = [0xf940_0108u32, 0xf940_0110, 0xd63f_0200]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect();
        assert_eq!(
            &code[end as usize..end as usize + expected.len()],
            expected.as_slice()
        );
    }
    #[cfg(target_arch = "x86_64")]
    {
        // MOV r9,[r11]; CALL [r9]: current-generation dispatch.
        assert_eq!(
            &code[end as usize..end as usize + 6],
            &[0x4d, 0x8b, 0x0b, 0x41, 0xff, 0x11]
        );
    }
    let map = json(bundle, JitArtifactFileName::CodeMap);
    let linkage_bytes = if cfg!(target_arch = "aarch64") { 12 } else { 6 };
    assert!(
        map["regions"].as_array().unwrap().iter().any(|region| {
            region["kind"] == "callTrampoline"
                && region["functionId"] == 7
                && region["logicalPc"] == CALL_PC
                && region["bytePc"] == CALL_BYTE_PC
                && region["callTargetFunctionId"] == target.plan.function_id
                && region["startOffset"].as_u64().unwrap() <= start
                && region["endOffset"].as_u64().unwrap() >= end + linkage_bytes
        }),
        "typed target must join its emitted call region"
    );
}

#[cfg(target_arch = "x86_64")]
#[test]
fn tail_known_linkage_matches_diagnostic_and_call_artifacts() {
    let mut view = JitCompileSnapshot::without_feedback(
        7,
        0,
        4,
        vec![JitTestInstruction::new(
            Op::TailCall,
            CALL_PC,
            CALL_BYTE_PC,
            vec![
                Operand::Register(0),
                Operand::Register(1),
                Operand::ConstIndex(1),
                Operand::Register(2),
            ],
        )],
    );
    let target = target(11, 91, NativeFrameKind::Baseline);
    view.direct_callees.insert(CALL_BYTE_PC, vec![target]);
    let output = compile(&view, true);
    assert_eq!(output.diagnostics.len(), 1);
    assert!(matches!(
        output.diagnostics[0],
        JitCompilerDiagnostic::DirectCallLowered {
            call_kind: JitDirectCallKind::Plain,
            instruction_pc: CALL_PC,
            byte_pc: CALL_BYTE_PC,
            callee_function_id: 11,
            target_index: 0,
            target_count: 1,
            outcome: JitDirectCallLoweringOutcome::Generated {
                code_object_id: 91,
                target_tier: JitDebugTier::Template,
                this_mode: JitDirectCallThisMode::StrictOrLexical,
            },
        }
    ));
    let bundle = output.artifact.as_deref().unwrap();
    let relocations = json(bundle, JitArtifactFileName::Relocations);
    let links: Vec<_> = relocations["relocations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|relocation| {
            relocation["target"]["kind"] == "functionEntryCell"
                && relocation["target"]["functionId"] == 11
        })
        .collect();
    assert_eq!(links.len(), 2, "Known tail transfer and ordinary fallback");
    let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
    let actual: Vec<_> = links
        .iter()
        .map(|relocation| {
            let end = relocation["endOffset"].as_u64().unwrap() as usize;
            &code[end..end + 6]
        })
        .collect();
    assert_eq!(
        actual,
        vec![
            // MOV RAX, [r9]: the tail transfer reads the entry, then tags
            // a handed-over caller anchor before jumping.
            &[0x4d, 0x8b, 0x0b, 0x49, 0x8b, 0x01][..],
            &[0x4d, 0x8b, 0x0b, 0x41, 0xff, 0x11][..], // CALL [r9]
        ]
    );
    let map = json(bundle, JitArtifactFileName::CodeMap);
    assert!(map["regions"].as_array().unwrap().iter().any(|region| {
        region["kind"] == "tailCall"
            && region["functionId"] == 7
            && region["logicalPc"] == CALL_PC
            && region["bytePc"] == CALL_BYTE_PC
            && region["callTargetFunctionId"] == 11
            && links.iter().all(|relocation| {
                region["startOffset"].as_u64().unwrap()
                    <= relocation["startOffset"].as_u64().unwrap()
                    && region["endOffset"].as_u64().unwrap()
                        >= relocation["endOffset"].as_u64().unwrap() + 6
            })
    }));
    let disabled = compile(&view, false);
    assert!(disabled.diagnostics.is_empty());
    assert_eq!(
        output.code.exact_bytes_for_test(),
        disabled.code.exact_bytes_for_test()
    );
}

#[test]
fn plain_known_linkage_matches_diagnostic_and_call_artifacts() {
    for (generation, tier) in [
        (91, NativeFrameKind::Baseline),
        (92, NativeFrameKind::Optimizing),
        (0, NativeFrameKind::Interpreter),
    ] {
        let mut view = view(false);
        let target = target(11, generation, tier);
        view.direct_callees.insert(CALL_BYTE_PC, vec![target]);
        let output = compile(&view, true);
        assert_generated(&output, JitDirectCallKind::Plain, &target, 0, 1);
        let disabled = compile(&view, false);
        assert!(disabled.diagnostics.is_empty());
        assert_eq!(
            output.code.exact_bytes_for_test(),
            disabled.code.exact_bytes_for_test(),
            "events do not alter emitted code"
        );
    }
}

#[test]
fn monomorphic_method_known_linkage_matches_diagnostic_and_call_artifacts() {
    let mut view = view(true);
    let method = method(0, 1);
    view.direct_methods
        .insert(CALL_BYTE_PC, vec![method.clone()]);
    assert_generated(
        &compile(&view, true),
        JitDirectCallKind::Method,
        &method.callee,
        0,
        1,
    );
}

#[test]
fn polymorphic_method_known_linkage_matches_each_diagnostic_and_call_artifacts() {
    let mut view = view(true);
    let methods = vec![method(0, 2), method(1, 2)];
    view.direct_methods.insert(CALL_BYTE_PC, methods.clone());
    let output = compile(&view, true);
    for method in methods {
        assert_generated(
            &output,
            JitDirectCallKind::Method,
            &method.callee,
            method.target_index,
            method.target_count,
        );
    }
}

#[cfg(target_arch = "x86_64")]
#[test]
fn method_guard_chain_has_one_committed_resolver_after_all_known_edges() {
    let mut view = view(true);
    let methods = vec![method(0, 2), method(1, 2)];
    view.direct_methods.insert(CALL_BYTE_PC, methods.clone());
    let output = compile(&view, true);
    for method in &methods {
        assert_generated(
            &output,
            JitDirectCallKind::Method,
            &method.callee,
            method.target_index,
            2,
        );
    }
    let bundle = output.artifact.as_deref().unwrap();
    let relocations = json(bundle, JitArtifactFileName::Relocations);
    let relocations = relocations["relocations"].as_array().unwrap();
    let resolvers: Vec<_> = relocations
        .iter()
        .filter(|relocation| {
            relocation["target"]["kind"] == "runtimeStub"
                && relocation["target"]["name"] == "jit_resolve_method"
        })
        .collect();
    assert_eq!(resolvers.len(), 1, "one final committed method resolution");
    let resolver_start = resolvers[0]["startOffset"].as_u64().unwrap();
    assert!(
        relocations
            .iter()
            .filter(|relocation| { relocation["target"]["kind"] == "functionEntryCell" })
            .all(|link| link["endOffset"].as_u64().unwrap() + 6 <= resolver_start)
    );
    let disabled = compile(&view, false);
    assert!(disabled.diagnostics.is_empty());
    assert_eq!(
        output.code.exact_bytes_for_test(),
        disabled.code.exact_bytes_for_test()
    );
}

#[test]
fn spread_constructor_refusals_preserve_super_and_derived_semantics() {
    use otter_vm::JitDirectCallLoweringRejectionReason;
    for (op, derived, kind) in [
        (Op::NewSpread, false, JitDirectCallKind::Construct),
        (Op::NewSpread, true, JitDirectCallKind::DerivedConstruct),
        (
            Op::SuperConstructSpread,
            false,
            JitDirectCallKind::SuperConstruct,
        ),
        (
            Op::SuperConstructSpread,
            true,
            JitDirectCallKind::DerivedSuperConstruct,
        ),
    ] {
        let mut view = JitCompileSnapshot::without_feedback(
            7,
            0,
            4,
            vec![
                JitTestInstruction::new(
                    op,
                    CALL_PC,
                    CALL_BYTE_PC,
                    vec![
                        Operand::Register(0),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 1, 32, vec![Operand::Register(0)]),
            ],
        );
        let mut target = target(11, 91, NativeFrameKind::Baseline);
        target.plan.is_derived_constructor = derived;
        view.direct_constructs.insert(CALL_BYTE_PC, target);
        let output = compile(&view, true);
        assert_eq!(output.diagnostics.len(), 1);
        assert!(matches!(output.diagnostics[0],
            JitCompilerDiagnostic::DirectCallLowered {
                call_kind, callee_function_id: 11,
                outcome: JitDirectCallLoweringOutcome::Rejected {
                    reason: JitDirectCallLoweringRejectionReason::BackendUnsupported
                }, ..
            } if call_kind == kind
        ));
        let relocations = json(
            output.artifact.as_deref().unwrap(),
            JitArtifactFileName::Relocations,
        );
        assert!(
            !relocations["relocations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["target"]["kind"] == "functionEntryCell"),
            "spread staging has no unimplemented Known link"
        );
    }
}

#[test]
fn constructor_known_linkage_uses_constructor_plans_and_exact_semantics() {
    for (super_construct, derived, kind) in [
        (false, false, JitDirectCallKind::Construct),
        (false, true, JitDirectCallKind::DerivedConstruct),
        (true, false, JitDirectCallKind::SuperConstruct),
        (true, true, JitDirectCallKind::DerivedSuperConstruct),
    ] {
        let mut view = JitCompileSnapshot::without_feedback(
            7,
            0,
            4,
            vec![
                JitTestInstruction::new(
                    if super_construct {
                        Op::SuperConstruct
                    } else {
                        Op::New
                    },
                    CALL_PC,
                    CALL_BYTE_PC,
                    vec![
                        Operand::Register(0),
                        Operand::Register(1),
                        Operand::ConstIndex(1),
                        Operand::Register(2),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 1, 32, vec![Operand::Register(0)]),
            ],
        );
        let mut target = target(11, 91, NativeFrameKind::Baseline);
        target.plan.call_flags |= otter_vm::native_abi::FUNCTION_CALL_CONSTRUCTIBLE;
        target.plan.is_derived_constructor = derived;
        target.plan.this_mode = if derived {
            JitDirectCallThisMode::DerivedConstructor
        } else {
            JitDirectCallThisMode::ConstructReceiver
        };
        view.direct_constructs.insert(CALL_BYTE_PC, target);
        // The plain-call table is deliberately empty: constructor lowering
        // cannot accidentally obtain the correct target from it.
        assert!(view.direct_callees.is_empty());
        let output = compile(&view, true);
        assert_generated(&output, kind, &target, 0, 1);
        let disabled = compile(&view, false);
        assert!(disabled.diagnostics.is_empty());
        assert_eq!(
            output.code.exact_bytes_for_test(),
            disabled.code.exact_bytes_for_test()
        );
    }
}
