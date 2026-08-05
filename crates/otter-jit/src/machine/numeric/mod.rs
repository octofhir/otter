//! Production numeric-leaf lowering through the shared Machine IR pipeline.
//!
//! # Contents
//! - `hir` — typed, side-effect-free numeric semantic graph.
//! - `arm64` — allocation-driven AArch64 emission.
//! - [`try_compile`] — production optimizing-tier entry for this vertical slice.
//!
//! # Invariants
//! - Bytecode is inspected only while building HIR; Machine IR and the emitter
//!   contain no bytecode operations.
//! - Parameter guards bail at logical PC zero before observable effects.
//! - Machine locations, edits, and frame size come only from regalloc2 output.
//! - The emitted leaf contains no call or safepoint and owns no GC roots.

mod arm64;
mod hir;

use otter_vm::{JitArtifactFileName, JitCompileSnapshot, deopt::DeoptTable};
use std::collections::BTreeMap;

use self::hir::{NumericFunction, NumericNode};
use super::{
    ControlFlow, DeoptId, InstructionSequence, MachineBlock, MachineBlockData, MachineInstruction,
    MachineInstructionId, MachineOpcode, MachineOperand, MachineRepresentation, MachineValue,
    TargetRegisterFile,
};
use crate::{
    Unsupported,
    artifact::{
        ArtifactRequest, CodeMapCapture, CodeRegion, NativeCompileOutput, build_bundle,
        relocation::RelocationCapture,
    },
    optimizing::{OptimizedCode, OptimizedMetadata},
};

pub(crate) fn try_compile(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    artifact_request: Option<ArtifactRequest>,
) -> Result<Option<NativeCompileOutput<OptimizedCode>>, Unsupported> {
    let Some(hir) = NumericFunction::build(view) else {
        return Ok(None);
    };
    let sequence = select(&hir)
        .map_err(|_| Unsupported::OperandShape("numeric HIR to Machine IR selection"))?;
    let allocation = sequence
        .allocate(&TargetRegisterFile::aarch64_numeric_leaf())
        .map_err(|_| Unsupported::OperandShape("numeric Machine IR allocation"))?;
    let emission = arm64::emit(&sequence, &allocation)?;
    let machine_register_count = u8::try_from(allocation.used_register_count())
        .map_err(|_| Unsupported::OperandShape("numeric machine register count"))?;
    let deopt_table = DeoptTable::default();
    let safepoints = Box::default();
    let frame_maps = Box::default();
    let frame_map_bitmap_words = Box::default();

    let artifact = artifact_request.map(|request| {
        let mut tier_input = format!(
            "; backend=otter-machine-ir numeric-leaf\n; parameters={} registers={} arithmetic-ops={}\n",
            hir.parameter_count, hir.register_count, hir.arithmetic_op_count
        );
        tier_input.push_str(&sequence.normalized());
        tier_input.push_str(&allocation.normalized());
        let mut code_map = CodeMapCapture::default();
        code_map.record(CodeRegion::structural(
            "machineNumericLeaf",
            0,
            emission.code.len(),
        ));
        build_bundle(
            request,
            view,
            code_object_id,
            &emission.code,
            JitArtifactFileName::OptimizedIr,
            tier_input,
            code_map,
            RelocationCapture::new(true),
            Some(&deopt_table),
            &safepoints,
        )
    });

    let code = OptimizedCode::new(
        emission.code,
        Some(emission.stack_frame_bytes),
        Box::new(otter_vm::deopt::DeoptRuntime {
            table: deopt_table,
            exits: Box::default(),
            gpr_budget: 0,
        }),
        safepoints,
        frame_maps,
        frame_map_bitmap_words,
        BTreeMap::new(),
        Box::default(),
        Box::default(),
        Box::default(),
        OptimizedMetadata {
            code_object_id,
            function_id: view.code_block.id,
            param_count: view.code_block.param_count,
            register_count: view.code_block.register_count,
            machine_register_count,
            linear_scan_spill_slot_count: allocation.spill_slots(),
            spill_slot_count: allocation.spill_slots(),
        },
    );
    Ok(Some(NativeCompileOutput {
        code,
        artifact,
        diagnostics: Box::default(),
    }))
}

fn select(hir: &NumericFunction) -> Result<InstructionSequence, super::VerificationError> {
    let mut representations =
        Vec::with_capacity(hir.nodes.len() + hir.parameter_count as usize + 1);
    let mut instructions = Vec::with_capacity(hir.nodes.len() + hir.parameter_count as usize + 2);
    let mut tagged_parameters = Vec::with_capacity(hir.parameter_count as usize);
    for parameter in 0..hir.parameter_count {
        let tagged = push_value(&mut representations, MachineRepresentation::Tagged);
        tagged_parameters.push(tagged);
        instructions.push(MachineInstruction::plain(
            MachineOpcode::EntryValue(parameter),
            vec![MachineOperand::register_output(tagged)],
        ));
    }

    let mut values = vec![None; hir.nodes.len()];
    let mut next_deopt = 0u32;
    for (index, node) in hir.nodes.iter().copied().enumerate() {
        let result = push_value(&mut representations, MachineRepresentation::Float64);
        let instruction = match node {
            NumericNode::Parameter(parameter) => {
                let tagged = tagged_parameters[usize::from(parameter)];
                let mut instruction = MachineInstruction::plain(
                    MachineOpcode::DecodeNumber,
                    vec![
                        MachineOperand::register_input(tagged),
                        MachineOperand::register_output(result),
                        MachineOperand::deopt(tagged),
                    ],
                );
                instruction.deopt = Some(DeoptId(next_deopt));
                next_deopt += 1;
                instruction
            }
            NumericNode::Constant(value) => MachineInstruction::plain(
                MachineOpcode::FloatConstant(value.to_bits()),
                vec![MachineOperand::register_output(result)],
            ),
            NumericNode::Add(left, right)
            | NumericNode::Sub(left, right)
            | NumericNode::Mul(left, right)
            | NumericNode::Div(left, right) => {
                let opcode = match node {
                    NumericNode::Add(..) => MachineOpcode::FloatAdd,
                    NumericNode::Sub(..) => MachineOpcode::FloatSub,
                    NumericNode::Mul(..) => MachineOpcode::FloatMul,
                    NumericNode::Div(..) => MachineOpcode::FloatDiv,
                    _ => unreachable!("matched binary numeric node"),
                };
                MachineInstruction::plain(
                    opcode,
                    vec![
                        MachineOperand::register_input(value(&values, left)),
                        MachineOperand::register_input(value(&values, right)),
                        MachineOperand::register_output(result),
                    ],
                )
            }
            NumericNode::Neg(source) => MachineInstruction::plain(
                MachineOpcode::FloatNeg,
                vec![
                    MachineOperand::register_input(value(&values, source)),
                    MachineOperand::register_output(result),
                ],
            ),
        };
        instructions.push(instruction);
        values[index] = Some(result);
    }

    let boxed = push_value(&mut representations, MachineRepresentation::Tagged);
    instructions.push(MachineInstruction::plain(
        MachineOpcode::BoxNumber,
        vec![
            MachineOperand::register_input(value(&values, hir.result)),
            MachineOperand::register_output(boxed),
        ],
    ));
    let mut ret = MachineInstruction::plain(
        MachineOpcode::Return,
        vec![MachineOperand::register_input(boxed)],
    );
    ret.control = ControlFlow::Return;
    instructions.push(ret);
    let end = MachineInstructionId(instructions.len() as u32);

    InstructionSequence::new(
        MachineBlock(0),
        representations,
        Vec::new(),
        vec![MachineBlockData {
            first: MachineInstructionId(0),
            end,
            predecessors: Vec::new(),
            successors: Vec::new(),
            parameters: Vec::new(),
            successor_arguments: Vec::new(),
        }],
        instructions,
    )
}

fn push_value(
    representations: &mut Vec<MachineRepresentation>,
    representation: MachineRepresentation,
) -> MachineValue {
    let value = MachineValue(representations.len() as u32);
    representations.push(representation);
    value
}

fn value(values: &[Option<MachineValue>], value: hir::NumericValue) -> MachineValue {
    values[value.0].expect("numeric HIR is topologically ordered")
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
    use crate::entry::{JitCtx, JitEntry, JitRet, STATUS_BAILED, STATUS_RETURNED};

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

    fn small_leaf_view() -> JitCompileSnapshot {
        numeric_view(
            1,
            2,
            vec![
                (Op::Neg, vec![Operand::Register(1), Operand::Register(0)]),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        )
    }

    fn spill_pressure_view() -> JitCompileSnapshot {
        let mut instructions = (1..=32)
            .map(|register| {
                (
                    Op::LoadInt32,
                    vec![
                        Operand::Register(register),
                        Operand::Imm32(i32::from(register)),
                    ],
                )
            })
            .collect::<Vec<_>>();
        let mut source = 0;
        let mut destination = 33;
        for right in 1..=32 {
            instructions.push((
                Op::Add,
                vec![
                    Operand::Register(destination),
                    Operand::Register(source),
                    Operand::Register(right),
                ],
            ));
            source = destination;
            destination += 1;
        }
        instructions.push((Op::ReturnValue, vec![Operand::Register(source)]));
        numeric_view(1, destination, instructions)
    }

    fn compile_output(
        view: &JitCompileSnapshot,
        artifact_request: Option<ArtifactRequest>,
    ) -> NativeCompileOutput<OptimizedCode> {
        try_compile(view, 7001, artifact_request)
            .expect("numeric Machine IR code generation")
            .expect("eligible numeric leaf")
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
        assert_eq!(
            frame, original_frame,
            "numeric leaf must not mutate VM slots"
        );
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
    fn executes_ieee_edges_and_boxes_canonical_results() {
        let identity = compile_output(&identity_view(), None).code;
        for value in [f64::INFINITY, f64::NEG_INFINITY, -0.0_f64] {
            let (ret, _, _) = execute(&identity, &[boxed_f64(value)], 0);
            assert_eq!(ret.status, STATUS_RETURNED);
            assert_eq!(unbox_number(ret.value).to_bits(), value.to_bits());
        }
        let (nan, _, _) = execute(&identity, &[boxed_f64(f64::NAN)], 0);
        assert_eq!(nan.status, STATUS_RETURNED);
        assert!(unbox_number(nan.value).is_nan());

        let overflow = compile_output(&overflow_view(), None).code;
        let (ret, _, _) = execute(&overflow, &[tag::box_int32(i32::MAX)], 0);
        assert_eq!(ret.status, STATUS_RETURNED);
        assert_eq!(unbox_number(ret.value), f64::from(i32::MAX) + 8.0);

        let (canonical_int, _, _) = execute(&identity, &[tag::box_int32(42)], 0);
        assert_eq!(canonical_int.value, tag::box_int32(42));
    }

    #[test]
    fn small_numeric_leaf_is_not_hidden_behind_a_fixture_size_threshold() {
        let code = compile_output(&small_leaf_view(), None).code;
        let (ret, _, _) = execute(&code, &[tag::box_int32(9)], 0);

        assert_eq!(ret.status, STATUS_RETURNED);
        assert_eq!(ret.value, tag::box_int32(-9));
    }

    #[test]
    fn executes_allocator_spills_through_the_shared_frame_layout() {
        let code = compile_output(&spill_pressure_view(), None).code;
        assert!(code.metadata().spill_slot_count > 0);
        assert!(
            JitFunctionCode::generated_stack_frame_bytes(&code)
                .is_some_and(|frame_bytes| frame_bytes > 16)
        );

        let (ret, _, _) = execute(&code, &[tag::box_int32(0)], 0);
        assert_eq!(ret.status, STATUS_RETURNED);
        assert_eq!(ret.value, tag::box_int32(528));

        let (bail, frame, pc) = execute(&code, &[Value::undefined().to_bits()], 77);
        assert_eq!(bail.status, STATUS_BAILED);
        assert_eq!(pc, 0);
        assert_eq!(frame[0], Value::undefined().to_bits());
    }

    #[test]
    fn number_guard_bails_before_observable_effects() {
        let code = compile_output(&identity_view(), None).code;
        let input = Value::undefined().to_bits();
        let (ret, frame, pc) = execute(&code, &[input], 91);

        assert_eq!(ret.status, STATUS_BAILED);
        assert_eq!(pc, 0);
        assert_eq!(frame[0], input);
    }

    #[test]
    fn artifact_identifies_the_installed_machine_ir_code_object() {
        let output = compile_output(
            &identity_view(),
            Some(ArtifactRequest {
                identity: JitArtifactIdentity {
                    function_name: "numericMachineLeaf".to_string(),
                    module: "test:numeric-machine-leaf".to_string(),
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
                .starts_with("; backend=otter-machine-ir numeric-leaf\n")
        );
        assert!(text(JitArtifactFileName::CodeMap).contains("\"kind\": \"machineNumericLeaf\""));
        assert_eq!(
            artifact
                .file(JitArtifactFileName::Code)
                .expect("exact code artifact")
                .contents(),
            output.code.compiled_code().bytes(),
        );
    }
}
