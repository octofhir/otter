//! Cranelift code generation for profitable straight-line Number leaves.
//!
//! # Contents
//! - [`NumericLeafBackend`] — immutable host ISA configuration owned by the
//!   existing Otter compiler hook.
//! - `plan` — conservative direct-bytecode eligibility and expression graph.
//! - `lower` — CLIF construction and relocation-free machine-byte emission.
//! - Existing artifact-bundle and [`crate::CompiledCode`] publication glue.
//!
//! # Invariants
//! - This is an internal backend of the existing `Optimizing` tier, not a new
//!   tier, runtime, registry, or artifact-format generation.
//! - Cranelift never allocates executable memory; finalized bytes are copied
//!   into Otter's sole dynasm/W^X [`crate::CompiledCode`] owner.
//! - The backend stores only immutable ISA configuration. Per-compilation
//!   contexts are owned locals, with no TLS, locks, registries, or interior
//!   mutability.
//! - Numeric leaves are entry-only. An OSR-target request falls through to the
//!   general backend, which owns loop-header entry and frame reconstruction.
//! - Ineligible or rejected machine output falls through to the general AArch64
//!   optimizer.
//!
//! # See also
//! - [`crate::optimizing`] owns the common code object and entry lifecycle.
//! - [`crate::artifact`] captures the same code object installed by the VM.

mod lower;
mod plan;

use std::collections::BTreeMap;

use cranelift_codegen::{
    isa::OwnedTargetIsa,
    settings::{self, Configurable},
};
use otter_vm::{JitArtifactFileName, JitCompileSnapshot, deopt::DeoptTable};

use crate::{
    CompiledCode, Unsupported,
    artifact::{
        ArtifactRequest, CodeMapCapture, CodeRegion, NativeCompileOutput, build_bundle,
        relocation::RelocationCapture,
    },
};

use super::{OptimizedCode, OptimizedMetadata};
use plan::NumericLeafPlan;

pub(crate) struct NumericLeafBackend {
    isa: OwnedTargetIsa,
}

impl NumericLeafBackend {
    pub(crate) fn for_host() -> Option<Self> {
        let mut flags = settings::builder();
        flags.set("opt_level", "speed").ok()?;
        flags.set("is_pic", "false").ok()?;
        flags.set("use_colocated_libcalls", "false").ok()?;
        flags.set("preserve_frame_pointers", "true").ok()?;
        flags.set("enable_probestack", "false").ok()?;
        flags.set("enable_verifier", "true").ok()?;
        flags.set("unwind_info", "true").ok()?;
        let isa = cranelift_native::builder()
            .ok()?
            .finish(settings::Flags::new(flags))
            .ok()?;
        Some(Self { isa })
    }

    pub(crate) fn try_compile(
        &self,
        view: &JitCompileSnapshot,
        code_object_id: u64,
        osr_pc: Option<u32>,
        artifact_request: Option<ArtifactRequest>,
    ) -> Result<Option<NativeCompileOutput<OptimizedCode>>, Unsupported> {
        if osr_pc.is_some() {
            return Ok(None);
        }
        let Some(plan) = NumericLeafPlan::build(view) else {
            return Ok(None);
        };
        let Some(lowered) = lower::lower(
            &plan,
            view.code_block.id,
            self.isa.as_ref(),
            artifact_request.is_some(),
        ) else {
            return Ok(None);
        };
        let code = CompiledCode::from_aarch64_bytes(&lowered.bytes, 0)?;
        let deopt_table = DeoptTable::default();
        let safepoints = Box::default();
        let frame_maps = Box::default();
        let frame_map_bitmap_words = Box::default();

        let artifact = artifact_request.map(|request| {
            let mut code_map = CodeMapCapture::default();
            code_map.record(CodeRegion::structural(
                "craneliftNumericLeaf",
                0,
                code.len(),
            ));
            let mut mapped_end = 0usize;
            for range in &lowered.source_ranges {
                if mapped_end < range.start {
                    code_map.record(CodeRegion::structural(
                        "craneliftBackendGlue",
                        mapped_end,
                        range.start,
                    ));
                }
                code_map.record(CodeRegion::instruction(
                    range.start,
                    range.end,
                    None,
                    None,
                    view.code_block.id,
                    range.source.logical_pc,
                    range.source.byte_pc,
                    None,
                    format!("{:?}", range.source.operation),
                ));
                mapped_end = range.end;
            }
            if mapped_end < code.len() {
                code_map.record(CodeRegion::structural(
                    "craneliftBackendGlue",
                    mapped_end,
                    code.len(),
                ));
            }
            build_bundle(
                request,
                view,
                code_object_id,
                &code,
                JitArtifactFileName::OptimizedIr,
                lowered
                    .tier_input
                    .expect("requested Cranelift artifact has tier input"),
                code_map,
                RelocationCapture::new(true),
                Some(&deopt_table),
                &safepoints,
            )
        });

        let code = OptimizedCode::new(
            code,
            Some(lowered.generated_stack_frame_bytes),
            deopt_table,
            safepoints,
            frame_maps,
            frame_map_bitmap_words,
            BTreeMap::new(),
            Box::default(),
            BTreeMap::new(),
            Box::default(),
            Box::default(),
            OptimizedMetadata {
                code_object_id,
                function_id: view.code_block.id,
                param_count: view.code_block.param_count,
                register_count: view.code_block.register_count,
                // Allocation is internal to Cranelift and there are no Otter
                // machine homes, deopt spills, or safepoint slots to publish.
                machine_register_count: 0,
                linear_scan_spill_slot_count: 0,
                spill_slot_count: 0,
            },
        );
        Ok(Some(NativeCompileOutput {
            code,
            artifact,
            diagnostics: Box::default(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use otter_bytecode::{Op, Operand};
    use otter_vm::{
        JitArtifactFileName, JitArtifactIdentity, JitCompileSnapshot, JitDebugTarget, JitDebugTier,
        JitFunctionCode, Value,
        jit::JitTestInstruction,
        jit_feedback::{ARITH_FLOAT64, ARITH_INT32, ArithFeedback},
        native_abi::{NativeFrame, NativeFrameFlags, NativeFrameKind, VmFrameHeader, VmThread},
        value::tag,
    };

    use super::*;
    use crate::{
        artifact::relocation::RelocationCapture,
        entry::{JitCtx, JitEntry, JitRet, STATUS_BAILED, STATUS_RETURNED},
    };

    fn numeric_view(
        param_count: u16,
        register_count: u16,
        instructions: Vec<(Op, Vec<Operand>)>,
    ) -> JitCompileSnapshot {
        let mut view = JitCompileSnapshot::without_feedback(
            71,
            param_count,
            register_count,
            instructions
                .into_iter()
                .enumerate()
                .map(|(pc, (op, operands))| {
                    JitTestInstruction::new(op, pc as u32, pc as u32 * 8, operands)
                })
                .collect(),
        );
        for pc in 0..view.instructions.len() {
            if matches!(
                view.instructions[pc].op(view.code_block.as_ref()),
                Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Neg
            ) {
                view.seed_arith_feedback_for_test(
                    pc as u32,
                    ArithFeedback::from_bits(ARITH_INT32 | ARITH_FLOAT64),
                );
            }
        }
        view
    }

    fn arithmetic_view() -> JitCompileSnapshot {
        numeric_view(
            2,
            10,
            vec![
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Mul,
                    vec![
                        Operand::Register(3),
                        Operand::Register(2),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Sub,
                    vec![
                        Operand::Register(4),
                        Operand::Register(3),
                        Operand::Register(0),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(5),
                        Operand::Register(4),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Mul,
                    vec![
                        Operand::Register(6),
                        Operand::Register(5),
                        Operand::Register(0),
                    ],
                ),
                (
                    Op::Sub,
                    vec![
                        Operand::Register(7),
                        Operand::Register(6),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Div,
                    vec![
                        Operand::Register(8),
                        Operand::Register(7),
                        Operand::Register(0),
                    ],
                ),
                (Op::Neg, vec![Operand::Register(9), Operand::Register(8)]),
                (Op::ReturnValue, vec![Operand::Register(9)]),
            ],
        )
    }

    fn identity_view() -> JitCompileSnapshot {
        let mut instructions = Vec::new();
        let mut source = 0;
        for destination in 1..=8 {
            instructions.push((
                Op::Neg,
                vec![Operand::Register(destination), Operand::Register(source)],
            ));
            source = destination;
        }
        instructions.push((Op::ReturnValue, vec![Operand::Register(source)]));
        numeric_view(1, 9, instructions)
    }

    fn overflow_view() -> JitCompileSnapshot {
        let mut instructions = vec![(Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(1)])];
        let mut source = 0;
        for destination in 2..=9 {
            instructions.push((
                Op::Add,
                vec![
                    Operand::Register(destination),
                    Operand::Register(source),
                    Operand::Register(1),
                ],
            ));
            source = destination;
        }
        instructions.push((Op::ReturnValue, vec![Operand::Register(source)]));
        numeric_view(1, 10, instructions)
    }

    fn float_constant_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            2,
            11,
            vec![
                (
                    Op::LoadNumber,
                    vec![Operand::Register(2), Operand::ConstIndex(0)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::Mul,
                    vec![
                        Operand::Register(4),
                        Operand::Register(3),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Sub,
                    vec![
                        Operand::Register(5),
                        Operand::Register(4),
                        Operand::Register(0),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(6),
                        Operand::Register(5),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::Mul,
                    vec![
                        Operand::Register(7),
                        Operand::Register(6),
                        Operand::Register(0),
                    ],
                ),
                (
                    Op::Sub,
                    vec![
                        Operand::Register(8),
                        Operand::Register(7),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Div,
                    vec![
                        Operand::Register(9),
                        Operand::Register(8),
                        Operand::Register(2),
                    ],
                ),
                (Op::Neg, vec![Operand::Register(10), Operand::Register(9)]),
                (Op::ReturnValue, vec![Operand::Register(10)]),
            ],
        );
        view.instructions[0].load_number = Some(1.23456789);
        view
    }

    fn compile_output(
        view: &JitCompileSnapshot,
        artifact_request: Option<ArtifactRequest>,
    ) -> NativeCompileOutput<OptimizedCode> {
        NumericLeafBackend::for_host()
            .expect("AArch64 host ISA")
            .try_compile(view, 7001, None, artifact_request)
            .expect("numeric-leaf code generation")
            .expect("eligible numeric leaf")
    }

    fn compile(view: &JitCompileSnapshot) -> OptimizedCode {
        compile_output(view, None).code
    }

    fn execute(code: &OptimizedCode, args: &[u64], initial_pc: u32) -> (JitRet, Vec<u64>, u32) {
        assert!(args.len() <= code.metadata().register_count as usize);
        let entry: JitEntry = unsafe { std::mem::transmute(code.compiled_code().entry_ptr()) };
        let mut frame = vec![Value::undefined().to_bits(); code.metadata().register_count as usize];
        frame[..args.len()].copy_from_slice(args);
        let original_frame = frame.clone();
        let metadata = code.metadata();
        let mut native_frame = NativeFrame::new(
            VmFrameHeader {
                function_id: metadata.function_id,
                code_block_id: metadata.function_id,
                pc: initial_pc,
                register_count: metadata.register_count,
                kind: NativeFrameKind::Optimizing,
                flags: NativeFrameFlags::empty(),
            },
            frame.as_mut_ptr() as u64,
            Value::undefined(),
            Value::undefined(),
        );
        native_frame.set_materialized_activation(0);
        let mut thread = VmThread::empty();
        thread.current_frame = std::ptr::addr_of_mut!(native_frame) as u64;
        thread.current_code_object_id = metadata.code_object_id;
        let mut error = None;
        let mut ctx = JitCtx {
            thread: std::ptr::addr_of_mut!(thread),
            native_frame: std::ptr::addr_of_mut!(native_frame),
            error: &mut error,
            activation_base: std::ptr::null_mut(),
            activation_top_ptr: std::ptr::null_mut(),
            activation_limit: 0,
            global_this_offset: std::ptr::null(),
            native_stack_limit: 0,
            generated_feedback_clean: 1,
        };
        let result = entry(&mut ctx);
        assert_eq!(frame, original_frame, "leaf code must not mutate VM slots");
        (result, frame, native_frame.header.pc)
    }

    fn boxed_f64(value: f64) -> u64 {
        Value::number_f64(value).to_bits()
    }

    fn unbox_number(bits: u64) -> f64 {
        if tag::is_int32_bits(bits) {
            f64::from(tag::unbox_int32(bits))
        } else {
            assert!(tag::is_double_bits(bits), "result must be a Number");
            f64::from_bits(tag::unbox_double(bits))
        }
    }

    #[test]
    fn executes_mixed_number_parameters_through_the_existing_entry_abi() {
        let code = compile(&arithmetic_view());
        let (ret, _, _) = execute(&code, &[tag::box_int32(3), boxed_f64(1.25)], 77);

        let expected = -((((3.0 + 1.25) * 1.25 - 3.0 + 1.25) * 3.0 - 1.25) / 3.0);
        assert_eq!(ret.status, STATUS_RETURNED);
        assert_eq!(unbox_number(ret.value), expected);
        assert!(
            code.compiled_code().len() < 1024,
            "the narrow backend must not fall through to the large general emitter"
        );
        RelocationCapture::new(true)
            .render(code.compiled_code().bytes())
            .expect("numeric leaf must remain portable under artifact normalization");
        assert_eq!(code.deopt_table().len(), 0);
        assert_eq!(JitFunctionCode::metadata(&code).safepoint_count, 0);
        assert_eq!(JitFunctionCode::metadata(&code).frame_map_count, 0);
    }

    #[test]
    fn preserves_ieee_edges_and_canonical_number_results() {
        let identity = compile(&identity_view());
        for value in [f64::INFINITY, f64::NEG_INFINITY, -0.0_f64] {
            let (ret, _, _) = execute(&identity, &[boxed_f64(value)], 0);
            assert_eq!(ret.status, STATUS_RETURNED);
            assert_eq!(unbox_number(ret.value).to_bits(), value.to_bits());
        }
        let (nan, _, _) = execute(&identity, &[boxed_f64(f64::NAN)], 0);
        assert_eq!(nan.status, STATUS_RETURNED);
        assert!(unbox_number(nan.value).is_nan());

        let overflow = compile(&overflow_view());
        let (ret, _, _) = execute(&overflow, &[tag::box_int32(i32::MAX)], 0);
        assert_eq!(ret.status, STATUS_RETURNED);
        assert_eq!(unbox_number(ret.value), f64::from(i32::MAX) + 8.0);

        let (canonical_int, _, _) = execute(&identity, &[tag::box_int32(42)], 0);
        assert_eq!(canonical_int.value, tag::box_int32(42));
    }

    #[test]
    fn non_number_guard_restarts_before_effects() {
        let code = compile(&identity_view());
        let input = Value::undefined().to_bits();
        let (ret, frame, pc) = execute(&code, &[input], 91);

        assert_eq!(ret.status, STATUS_BAILED);
        assert_eq!(pc, 0);
        assert_eq!(frame[0], input);
    }

    #[test]
    fn osr_target_falls_through_to_the_general_backend() {
        let output = NumericLeafBackend::for_host()
            .expect("AArch64 host ISA")
            .try_compile(&arithmetic_view(), 7002, Some(3), None)
            .expect("OSR gate");

        assert!(
            output.is_none(),
            "entry-only numeric leaves must not claim an OSR-target request"
        );
    }

    #[test]
    fn float_constants_capture_the_exact_installed_artifact_bundle() {
        let output = compile_output(
            &float_constant_view(),
            Some(ArtifactRequest {
                identity: JitArtifactIdentity {
                    function_name: "numericArtifactLeaf".to_string(),
                    module: "test:numeric-artifact-leaf".to_string(),
                },
                tier: JitDebugTier::Optimizing,
                entry: JitDebugTarget::Entry,
            }),
        );
        let artifact = output.artifact.expect("requested artifact bundle");
        let text = |name| {
            std::str::from_utf8(artifact.file(name).expect("artifact payload").contents())
                .expect("text artifact")
        };

        assert!(
            text(JitArtifactFileName::OptimizedIr)
                .starts_with("; backend=cranelift numeric-leaf\n")
        );
        assert!(text(JitArtifactFileName::CodeMap).contains("\"kind\": \"craneliftNumericLeaf\""));
        assert!(text(JitArtifactFileName::CodeMap).contains("\"logicalPc\":"));
        assert!(text(JitArtifactFileName::CodeMap).contains("\"bytePc\":"));
        assert!(text(JitArtifactFileName::CodeMap).contains("\"operation\": \"LoadNumber\""));
        assert!(!text(JitArtifactFileName::CodeMap).contains("\"operationIndex\":"));
        assert!(text(JitArtifactFileName::Assembly).contains("pc="));
        assert!(text(JitArtifactFileName::Assembly).contains("tier-op=\"LoadNumber\""));
        assert!(text(JitArtifactFileName::Relocations).contains("\"relocations\": []"));
        assert!(text(JitArtifactFileName::Safepoints).contains("\"safepoints\": []"));
        assert!(text(JitArtifactFileName::Deopt).contains("\"exits\": []"));
        assert_eq!(
            artifact
                .file(JitArtifactFileName::Code)
                .expect("exact code artifact")
                .contents(),
            output.code.compiled_code().bytes(),
            "artifact code.bin must be the installed executable object"
        );

        let code_map: serde_json::Value =
            serde_json::from_str(text(JitArtifactFileName::CodeMap)).expect("code-map JSON");
        let regions = code_map["regions"].as_array().expect("code-map regions");
        for offset in (0..output.code.compiled_code().len()).step_by(4) {
            assert!(
                regions.iter().any(|region| {
                    matches!(
                        region["kind"].as_str(),
                        Some("instruction" | "craneliftBackendGlue")
                    ) && region["startOffset"]
                        .as_u64()
                        .is_some_and(|start| start <= offset as u64)
                        && region["endOffset"]
                            .as_u64()
                            .is_some_and(|end| offset as u64 + 4 <= end)
                }),
                "native instruction +0x{offset:08x} lacks an exact source or backend-glue range"
            );
        }
    }
}
