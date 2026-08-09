//! Production scalar-function lowering through the shared Machine IR pipeline.
//!
//! # Contents
//! - `hir` — typed, side-effect-free scalar semantic graph.
//! - `arm64` — allocation-driven AArch64 emission.
//! - [`try_compile`] — production optimizing-tier entry for this vertical slice.
//!
//! # Invariants
//! - Bytecode is inspected only while building HIR; Machine IR and the emitter
//!   contain no bytecode operations.
//! - Parameter guards bail at logical PC zero before observable effects.
//! - Machine locations, edits, and frame size come only from regalloc2 output.
//! - Reducible loop headers publish one representation-checked OSR trampoline
//!   that fills only live block parameters and never mutates the VM window.
//! - Allocating calls save every live tagged value from its exact late-use
//!   location into the frame's collector-visible root area and reload it after
//!   moving GC; no interpreter-window shuttle or emitter-local map exists.

mod arm64;
mod hir;

use otter_vm::{
    JitArtifactFileName, JitCompileSnapshot,
    deopt::{DeoptExitDescriptor, DeoptExitId, DeoptRuntime},
    native_abi::{STUB_JIT_BACKEDGE_POLL, STUB_JIT_DEOPT_WRITEBACK},
};
use std::collections::{BTreeMap, BTreeSet};

use self::hir::{NumericFramePoint, NumericFunction, NumericNode, NumericTerminator, NumericType};
use super::{
    CallDescriptor, CallEffects, CallTarget, ControlFlow, DeoptId, ExceptionalEdge,
    InstructionSequence, MachineBlock, MachineBlockData, MachineInstruction, MachineInstructionId,
    MachineOpcode, MachineOperand, MachineOsrInput, MachineOsrType, MachineRepresentation,
    MachineValue, PhysicalRegister, SafepointKind, TargetRegisterFile, lower_deopt_table,
    lower_safepoints,
};
use crate::{
    Unsupported,
    artifact::{ArtifactRequest, CodeMapCapture, CodeRegion, NativeCompileOutput, build_bundle},
    entry::TransitionTable,
    optimizing::{OptimizedCode, OptimizedMetadata},
};

pub(crate) fn try_compile(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &TransitionTable,
    artifact_request: Option<ArtifactRequest>,
) -> Result<Option<NativeCompileOutput<OptimizedCode>>, Unsupported> {
    let Some(hir) = NumericFunction::build(view) else {
        return Ok(None);
    };
    let sequence = select(&hir)
        .map_err(|_| Unsupported::OperandShape("scalar HIR to Machine IR selection"))?;
    let allocation = sequence
        .allocate(&TargetRegisterFile::aarch64_scalar_function())
        .map_err(|_| Unsupported::OperandShape("scalar Machine IR allocation"))?;
    let machine_safepoints = lower_safepoints(&sequence, &allocation)
        .map_err(|_| Unsupported::OperandShape("scalar Machine IR safepoint lowering"))?;
    let parameter_prefix_entry = machine_safepoints.is_empty()
        && !hir
            .frame_states
            .iter()
            .any(|state| matches!(state.point, NumericFramePoint::Backedge { .. }));
    let frame = arm64::frame_layout(&allocation, machine_safepoints.root_slot_count())?;
    let deopt_table = lower_deopt_table(
        &sequence,
        &allocation,
        frame,
        arm64::GPR_BUDGET,
        arm64::FP_BUDGET,
        &machine_frame_states(&hir),
    )
    .map_err(|_| Unsupported::OperandShape("scalar Machine IR deopt lowering"))?;
    let mut exits = Vec::with_capacity(hir.frame_states.len());
    for (index, state) in hir.frame_states.iter().enumerate() {
        let logical_pc = view
            .instructions
            .iter()
            .position(|instruction| instruction.byte_pc == state.byte_pc)
            .and_then(|pc| u32::try_from(pc).ok())
            .ok_or(Unsupported::OperandShape("scalar deopt resume PC"))?;
        exits.push(DeoptExitDescriptor {
            state: DeoptExitId(index as u32),
            resume_pcs: vec![logical_pc].into_boxed_slice(),
        });
    }
    let deopt_runtime = Box::new(DeoptRuntime {
        table: deopt_table,
        exits: exits.into_boxed_slice(),
        gpr_budget: arm64::GPR_BUDGET,
    });
    let emission = arm64::emit(
        &sequence,
        &allocation,
        frame,
        &deopt_runtime,
        &machine_safepoints,
        transitions.entry(STUB_JIT_BACKEDGE_POLL),
        transitions.variadic_entry(STUB_JIT_DEOPT_WRITEBACK),
        otter_vm::runtime_stubs::STRING_CONCAT_ALLOC
            .entry_addr()
            .ok_or(Unsupported::OperandShape(
                "scalar string concat runtime entry",
            ))? as u64,
        otter_vm::runtime_stubs::NUMBER_REM_F64_LEAF.entry_addr() as u64,
        otter_vm::runtime_stubs::NUMBER_POW_F64_LEAF.entry_addr() as u64,
        otter_vm::runtime_stubs::NUMBER_TO_INT32_F64_LEAF.entry_addr() as u64,
        otter_vm::runtime_stubs::STRICT_EQ_LEAF.entry_addr() as u64,
        otter_vm::runtime_stubs::TO_BOOLEAN_LEAF.entry_addr() as u64,
        view.code_block.register_count,
        artifact_request.is_some(),
    )?;
    let machine_register_count = u8::try_from(allocation.used_register_count())
        .map_err(|_| Unsupported::OperandShape("scalar machine register count"))?;
    let safepoints = machine_safepoints.records().to_vec().into_boxed_slice();
    let frame_maps = Box::default();
    let frame_map_bitmap_words = Box::default();

    let arm64::Emission {
        code: emitted_code,
        generated_stack_frame_bytes,
        relocations,
        osr_entries,
        osr_regions,
    } = emission;

    let artifact = artifact_request.map(|request| {
        let mut tier_input = format!(
            "; backend=otter-machine-ir scalar-function\n; parameters={} registers={} blocks={} arithmetic-ops={}\n",
            hir.parameter_count,
            hir.register_count,
            hir.blocks.len(),
            hir.arithmetic_op_count
        );
        tier_input.push_str(&sequence.normalized());
        tier_input.push_str(&allocation.normalized());
        tier_input.push_str(&machine_safepoints.normalized());
        let mut code_map = CodeMapCapture::default();
        code_map.record(CodeRegion::structural(
            "machineScalarFunction",
            0,
            emitted_code.len(),
        ));
        for &(logical_pc, start, end) in &osr_regions {
            code_map.record_osr(logical_pc, start, end);
        }
        build_bundle(
            request,
            view,
            code_object_id,
            &emitted_code,
            JitArtifactFileName::OptimizedIr,
            tier_input,
            code_map,
            relocations,
            Some(&deopt_runtime.table),
            &safepoints,
        )
    });

    let code = OptimizedCode::new(
        emitted_code,
        Some(generated_stack_frame_bytes),
        deopt_runtime,
        safepoints,
        frame_maps,
        frame_map_bitmap_words,
        osr_entries,
        Box::default(),
        Box::default(),
        Box::default(),
        OptimizedMetadata {
            code_object_id,
            function_id: view.code_block.id,
            param_count: view.code_block.param_count,
            register_count: view.code_block.register_count,
            parameter_prefix_entry,
            machine_register_count,
            linear_scan_spill_slot_count: allocation.spill_slots(),
            spill_slot_count: allocation
                .spill_slots()
                .saturating_add(u32::from(machine_safepoints.root_slot_count())),
        },
    );
    Ok(Some(NativeCompileOutput {
        code,
        artifact,
        diagnostics: Box::default(),
    }))
}

fn select(hir: &NumericFunction) -> Result<InstructionSequence, super::VerificationError> {
    let mut representations = hir
        .nodes
        .iter()
        .map(|node| match node.value_type() {
            NumericType::Tagged => MachineRepresentation::Tagged,
            NumericType::Int32 => MachineRepresentation::Int32,
            NumericType::Uint32 => MachineRepresentation::Uint32,
            NumericType::Number => MachineRepresentation::Float64,
            NumericType::Boolean => MachineRepresentation::Int32,
        })
        .collect::<Vec<_>>();
    let values = (0..hir.nodes.len())
        .map(|index| MachineValue(index as u32))
        .collect::<Vec<_>>();
    let mut tagged_parameters = vec![None; hir.parameter_count as usize];
    for (index, node) in hir.nodes.iter().enumerate() {
        let NumericNode::Parameter {
            register,
            value_type,
        } = node
        else {
            continue;
        };
        tagged_parameters[usize::from(*register)] = Some(if *value_type == NumericType::Tagged {
            values[index]
        } else {
            push_value(&mut representations, MachineRepresentation::Tagged)
        });
    }

    let selection_cfg = SelectionCfg::build(hir);
    let mut instructions = Vec::with_capacity(hir.nodes.len() + hir.parameter_count as usize + 4);
    let mut call_descriptors = Vec::<CallDescriptor>::new();
    let mut next_safepoint = 0_u32;
    let mut blocks = Vec::with_capacity(selection_cfg.order.len());
    let frame_state_ids = hir
        .frame_states
        .iter()
        .enumerate()
        .map(|(index, state)| (state.point, DeoptId(index as u32)))
        .collect::<BTreeMap<_, _>>();
    for selected in &selection_cfg.order {
        let first = MachineInstructionId(instructions.len() as u32);
        let SelectedBlock::Original(block_index) = *selected else {
            let SelectedBlock::SplitEdge {
                predecessor,
                edge,
                successor,
            } = *selected
            else {
                unreachable!("selected block is original or split edge")
            };
            if successor <= predecessor {
                let point = NumericFramePoint::Backedge { predecessor, edge };
                let deopt = frame_state_ids[&point];
                let mut poll = MachineInstruction::plain(MachineOpcode::BackedgePoll, Vec::new());
                attach_frame_state(hir, &values, deopt, &mut poll);
                poll.clobbers = TargetRegisterFile::aarch64_scalar_call_clobbers();
                instructions.push(poll);
            }
            let mut jump = MachineInstruction::plain(MachineOpcode::Jump, Vec::new());
            jump.control = ControlFlow::Branch;
            instructions.push(jump);
            let end = MachineInstructionId(instructions.len() as u32);
            blocks.push(MachineBlockData {
                first,
                end,
                predecessors: vec![selection_cfg.originals[predecessor]],
                successors: vec![selection_cfg.originals[successor]],
                parameters: Vec::new(),
                successor_arguments: vec![
                    hir.blocks[predecessor].successor_arguments[edge]
                        .iter()
                        .map(|&value| machine_value(&values, value))
                        .collect(),
                ],
            });
            continue;
        };
        let block = &hir.blocks[block_index];
        if block_index == 0 {
            for (parameter, &tagged) in tagged_parameters.iter().enumerate() {
                let Some(tagged) = tagged else {
                    continue;
                };
                instructions.push(MachineInstruction::plain(
                    MachineOpcode::EntryValue(parameter as u16),
                    vec![MachineOperand::register_output(tagged)],
                ));
            }
        }
        if block
            .predecessors
            .iter()
            .any(|&predecessor| predecessor >= block_index)
        {
            debug_assert_eq!(block.parameters.len(), block.parameter_registers.len());
            let inputs = block
                .parameters
                .iter()
                .zip(&block.parameter_registers)
                .map(|(&parameter, &frame_register)| MachineOsrInput {
                    frame_register,
                    value_type: match hir.nodes[parameter.0].value_type() {
                        NumericType::Tagged => MachineOsrType::Tagged,
                        NumericType::Int32 => MachineOsrType::Int32,
                        NumericType::Uint32 => MachineOsrType::Uint32,
                        NumericType::Number => MachineOsrType::Float64,
                        NumericType::Boolean => MachineOsrType::Boolean,
                    },
                })
                .collect();
            instructions.push(MachineInstruction::plain(
                MachineOpcode::OsrEntry {
                    logical_pc: block.logical_pc,
                    inputs,
                },
                block
                    .parameters
                    .iter()
                    .map(|&parameter| {
                        MachineOperand::location_input(machine_value(&values, parameter))
                    })
                    .collect(),
            ));
        }
        for &node_value in &block.nodes {
            let result = values[node_value.0];
            let node = hir.nodes[node_value.0];
            if let NumericNode::FloatToInt32(source) = node {
                let mut call = MachineInstruction::plain(
                    MachineOpcode::Float64ToInt32,
                    vec![
                        MachineOperand::fixed_register_input(
                            machine_value(&values, source),
                            PhysicalRegister::float(0),
                        ),
                        MachineOperand::fixed_register_output(result, PhysicalRegister::integer(0)),
                    ],
                );
                call.clobbers = TargetRegisterFile::aarch64_scalar_call_clobbers();
                call.clobbers
                    .retain(|register| *register != PhysicalRegister::integer(0));
                instructions.push(call);
                continue;
            }
            if let NumericNode::Rem(left, right) | NumericNode::Pow(left, right) = node {
                let opcode = if matches!(node, NumericNode::Rem(..)) {
                    MachineOpcode::FloatRem
                } else {
                    MachineOpcode::FloatPow
                };
                let mut call = MachineInstruction::plain(
                    opcode,
                    vec![
                        MachineOperand::fixed_register_input(
                            machine_value(&values, left),
                            PhysicalRegister::float(0),
                        ),
                        MachineOperand::fixed_register_input(
                            machine_value(&values, right),
                            PhysicalRegister::float(1),
                        ),
                        MachineOperand::fixed_register_output(result, PhysicalRegister::float(0)),
                    ],
                );
                call.clobbers = TargetRegisterFile::aarch64_scalar_call_clobbers();
                call.clobbers
                    .retain(|register| *register != PhysicalRegister::float(0));
                instructions.push(call);
                continue;
            }
            let mut instruction = match node {
                NumericNode::Parameter {
                    register,
                    value_type,
                } => {
                    if value_type == NumericType::Tagged {
                        continue;
                    }
                    let tagged = tagged_parameters[usize::from(register)]
                        .expect("live HIR parameter has an entry value");
                    let opcode = match value_type {
                        NumericType::Int32 => MachineOpcode::DecodeInt32,
                        NumericType::Number => MachineOpcode::DecodeNumber,
                        NumericType::Tagged | NumericType::Uint32 | NumericType::Boolean => {
                            unreachable!("parameter inference emits only Int32 or Number")
                        }
                    };
                    let output = if value_type == NumericType::Int32 {
                        MachineOperand::register_reuse_output(result, 0)
                    } else {
                        MachineOperand::register_output(result)
                    };
                    MachineInstruction::plain(
                        opcode,
                        vec![MachineOperand::register_input(tagged), output],
                    )
                }
                NumericNode::BlockParameter(_) => continue,
                NumericNode::TaggedConstant(bits) => MachineInstruction::plain(
                    MachineOpcode::TaggedConstant(bits),
                    vec![MachineOperand::register_output(result)],
                ),
                NumericNode::This => MachineInstruction::plain(
                    MachineOpcode::EntryThis,
                    vec![MachineOperand::register_output(result)],
                ),
                NumericNode::IntegerConstant(value) => MachineInstruction::plain(
                    MachineOpcode::IntegerConstant(i64::from(value)),
                    vec![MachineOperand::register_output(result)],
                ),
                NumericNode::BooleanConstant(value) => MachineInstruction::plain(
                    MachineOpcode::IntegerConstant(i64::from(value)),
                    vec![MachineOperand::register_output(result)],
                ),
                NumericNode::Constant(value) => MachineInstruction::plain(
                    MachineOpcode::FloatConstant(value.to_bits()),
                    vec![MachineOperand::register_output(result)],
                ),
                NumericNode::WidenInt32(source) => MachineInstruction::plain(
                    MachineOpcode::Int32ToFloat64,
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::WidenUint32(source) => MachineInstruction::plain(
                    MachineOpcode::Uint32ToFloat64,
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::BooleanToInt32(source) => MachineInstruction::plain(
                    MachineOpcode::BooleanToInt32,
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::FloatToInt32(_) => {
                    unreachable!("Float64 ToInt32 selected as a typed leaf call")
                }
                NumericNode::IntegerAdd(left, right) => MachineInstruction::plain(
                    MachineOpcode::IntegerAdd,
                    vec![
                        MachineOperand::register_input(machine_value(&values, left)),
                        MachineOperand::register_input(machine_value(&values, right)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::IntegerSub(left, right) => MachineInstruction::plain(
                    MachineOpcode::IntegerSub,
                    vec![
                        MachineOperand::register_input(machine_value(&values, left)),
                        MachineOperand::register_input(machine_value(&values, right)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::IntegerMul(left, right) => MachineInstruction::plain(
                    MachineOpcode::IntegerMul,
                    vec![
                        MachineOperand::register_input(machine_value(&values, left)),
                        MachineOperand::register_input(machine_value(&values, right)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::IntegerNeg(source) => MachineInstruction::plain(
                    MachineOpcode::IntegerNeg,
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::IntegerAnd(left, right)
                | NumericNode::IntegerOr(left, right)
                | NumericNode::IntegerXor(left, right)
                | NumericNode::IntegerShiftLeft(left, right)
                | NumericNode::IntegerShiftRight(left, right) => {
                    let opcode = match node {
                        NumericNode::IntegerAnd(..) => MachineOpcode::IntegerAnd,
                        NumericNode::IntegerOr(..) => MachineOpcode::IntegerOr,
                        NumericNode::IntegerXor(..) => MachineOpcode::IntegerXor,
                        NumericNode::IntegerShiftLeft(..) => MachineOpcode::IntegerShiftLeft,
                        NumericNode::IntegerShiftRight(..) => MachineOpcode::IntegerShiftRight,
                        _ => unreachable!("matched binary int32 node"),
                    };
                    MachineInstruction::plain(
                        opcode,
                        vec![
                            MachineOperand::register_input(machine_value(&values, left)),
                            MachineOperand::register_input(machine_value(&values, right)),
                            MachineOperand::register_output(result),
                        ],
                    )
                }
                NumericNode::IntegerShiftRightLogical(left, right) => MachineInstruction::plain(
                    MachineOpcode::IntegerShiftRightLogical,
                    vec![
                        MachineOperand::register_input(machine_value(&values, left)),
                        MachineOperand::register_input(machine_value(&values, right)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::IntegerNot(source) => MachineInstruction::plain(
                    MachineOpcode::IntegerNot,
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::IntegerEqual(left, right)
                | NumericNode::IntegerNotEqual(left, right)
                | NumericNode::IntegerLessThan(left, right)
                | NumericNode::IntegerLessEqual(left, right)
                | NumericNode::IntegerGreaterThan(left, right)
                | NumericNode::IntegerGreaterEqual(left, right) => {
                    let opcode = match node {
                        NumericNode::IntegerEqual(..) => MachineOpcode::IntegerEqual,
                        NumericNode::IntegerNotEqual(..) => MachineOpcode::IntegerNotEqual,
                        NumericNode::IntegerLessThan(..) => MachineOpcode::IntegerLessThan,
                        NumericNode::IntegerLessEqual(..) => MachineOpcode::IntegerLessEqual,
                        NumericNode::IntegerGreaterThan(..) => MachineOpcode::IntegerGreaterThan,
                        NumericNode::IntegerGreaterEqual(..) => MachineOpcode::IntegerGreaterEqual,
                        _ => unreachable!("matched int32 comparison node"),
                    };
                    MachineInstruction::plain(
                        opcode,
                        vec![
                            MachineOperand::register_input(machine_value(&values, left)),
                            MachineOperand::register_input(machine_value(&values, right)),
                            MachineOperand::register_output(result),
                        ],
                    )
                }
                NumericNode::IntegerAddImmediate(source, immediate)
                | NumericNode::IntegerSubImmediate(source, immediate)
                | NumericNode::IntegerAndImmediate(source, immediate)
                | NumericNode::IntegerLessThanImmediate(source, immediate)
                | NumericNode::IntegerEqualImmediate(source, immediate)
                | NumericNode::IntegerNotEqualImmediate(source, immediate) => {
                    let opcode = match node {
                        NumericNode::IntegerAddImmediate(..) => {
                            MachineOpcode::IntegerAddImmediate(immediate)
                        }
                        NumericNode::IntegerSubImmediate(..) => {
                            MachineOpcode::IntegerSubImmediate(immediate)
                        }
                        NumericNode::IntegerAndImmediate(..) => {
                            MachineOpcode::IntegerAndImmediate(immediate)
                        }
                        NumericNode::IntegerLessThanImmediate(..) => {
                            MachineOpcode::IntegerLessThanImmediate(immediate)
                        }
                        NumericNode::IntegerEqualImmediate(..) => {
                            MachineOpcode::IntegerEqualImmediate(immediate)
                        }
                        NumericNode::IntegerNotEqualImmediate(..) => {
                            MachineOpcode::IntegerNotEqualImmediate(immediate)
                        }
                        _ => unreachable!("matched immediate integer node"),
                    };
                    MachineInstruction::plain(
                        opcode,
                        vec![
                            MachineOperand::register_input(machine_value(&values, source)),
                            MachineOperand::register_output(result),
                        ],
                    )
                }
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
                            MachineOperand::register_input(machine_value(&values, left)),
                            MachineOperand::register_input(machine_value(&values, right)),
                            MachineOperand::register_output(result),
                        ],
                    )
                }
                NumericNode::Neg(source) => MachineInstruction::plain(
                    MachineOpcode::FloatNeg,
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::IntegerToBoolean(source) => MachineInstruction::plain(
                    MachineOpcode::IntegerToBoolean,
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::TaggedToBoolean(source) => {
                    let padding = push_value(&mut representations, MachineRepresentation::Tagged);
                    instructions.push(MachineInstruction::plain(
                        MachineOpcode::TaggedConstant(otter_vm::Value::undefined().to_bits()),
                        vec![MachineOperand::register_output(padding)],
                    ));
                    let descriptor_index = intern_leaf_boolean_call_descriptor(
                        &mut call_descriptors,
                        otter_vm::native_abi::STUB_TO_BOOLEAN_LEAF,
                        2,
                    );
                    let mut call = MachineInstruction::plain(
                        MachineOpcode::Call(descriptor_index as u32),
                        vec![
                            MachineOperand::fixed_register_input(
                                machine_value(&values, source),
                                PhysicalRegister::integer(1),
                            ),
                            MachineOperand::fixed_register_input(
                                padding,
                                PhysicalRegister::integer(2),
                            ),
                            MachineOperand::fixed_register_output(
                                result,
                                PhysicalRegister::integer(0),
                            ),
                        ],
                    );
                    call.clobbers = call_descriptors[descriptor_index].clobbers.clone();
                    call
                }
                NumericNode::TaggedStrictEqual(left, right) => {
                    let left = tagged_call_argument(
                        hir,
                        &values,
                        &mut representations,
                        &mut instructions,
                        left,
                    );
                    let right = tagged_call_argument(
                        hir,
                        &values,
                        &mut representations,
                        &mut instructions,
                        right,
                    );
                    let descriptor_index = intern_leaf_boolean_call_descriptor(
                        &mut call_descriptors,
                        otter_vm::native_abi::STUB_STRICT_EQ_LEAF,
                        2,
                    );
                    let mut call = MachineInstruction::plain(
                        MachineOpcode::Call(descriptor_index as u32),
                        vec![
                            MachineOperand::fixed_register_input(
                                left,
                                PhysicalRegister::integer(1),
                            ),
                            MachineOperand::fixed_register_input(
                                right,
                                PhysicalRegister::integer(2),
                            ),
                            MachineOperand::fixed_register_output(
                                result,
                                PhysicalRegister::integer(0),
                            ),
                        ],
                    );
                    call.clobbers = call_descriptors[descriptor_index].clobbers.clone();
                    call
                }
                NumericNode::TaggedStringConcat(left, right) => {
                    let left = tagged_call_argument(
                        hir,
                        &values,
                        &mut representations,
                        &mut instructions,
                        left,
                    );
                    let right = tagged_call_argument(
                        hir,
                        &values,
                        &mut representations,
                        &mut instructions,
                        right,
                    );
                    let padding = push_value(&mut representations, MachineRepresentation::Tagged);
                    instructions.push(MachineInstruction::plain(
                        MachineOpcode::TaggedConstant(otter_vm::Value::undefined().to_bits()),
                        vec![MachineOperand::register_output(padding)],
                    ));
                    let descriptor_index = intern_call_descriptor(
                        &mut call_descriptors,
                        string_concat_call_descriptor(),
                    );
                    let mut call = MachineInstruction::plain(
                        MachineOpcode::Call(descriptor_index as u32),
                        vec![
                            MachineOperand::fixed_register_input(
                                left,
                                PhysicalRegister::integer(2),
                            ),
                            MachineOperand::fixed_register_input(
                                right,
                                PhysicalRegister::integer(3),
                            ),
                            MachineOperand::fixed_register_input(
                                padding,
                                PhysicalRegister::integer(4),
                            ),
                            MachineOperand::fixed_register_output(
                                result,
                                PhysicalRegister::integer(0),
                            ),
                        ],
                    );
                    call.clobbers = call_descriptors[descriptor_index].clobbers.clone();
                    call.safepoint = Some(super::SafepointId(next_safepoint));
                    next_safepoint = next_safepoint
                        .checked_add(1)
                        .expect("bounded scalar function safepoint count");
                    call
                }
                NumericNode::FloatToBoolean(source) => MachineInstruction::plain(
                    MachineOpcode::FloatToBoolean,
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::BooleanNot(source) => MachineInstruction::plain(
                    MachineOpcode::BooleanNot,
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::Equal(left, right)
                | NumericNode::NotEqual(left, right)
                | NumericNode::LessThan(left, right)
                | NumericNode::LessEqual(left, right)
                | NumericNode::GreaterThan(left, right)
                | NumericNode::GreaterEqual(left, right) => {
                    let opcode = match node {
                        NumericNode::Equal(..) => MachineOpcode::FloatEqual,
                        NumericNode::NotEqual(..) => MachineOpcode::FloatNotEqual,
                        NumericNode::LessThan(..) => MachineOpcode::FloatLessThan,
                        NumericNode::LessEqual(..) => MachineOpcode::FloatLessEqual,
                        NumericNode::GreaterThan(..) => MachineOpcode::FloatGreaterThan,
                        NumericNode::GreaterEqual(..) => MachineOpcode::FloatGreaterEqual,
                        _ => unreachable!("matched Float64 comparison node"),
                    };
                    MachineInstruction::plain(
                        opcode,
                        vec![
                            MachineOperand::register_input(machine_value(&values, left)),
                            MachineOperand::register_input(machine_value(&values, right)),
                            MachineOperand::register_output(result),
                        ],
                    )
                }
                NumericNode::Rem(..) | NumericNode::Pow(..) => {
                    unreachable!("numeric leaf calls are selected before ordinary nodes")
                }
            };
            if let Some(&deopt) = frame_state_ids.get(&NumericFramePoint::Node(node_value)) {
                attach_frame_state(hir, &values, deopt, &mut instruction);
            }
            attach_safepoint_roots(&representations, &mut instruction);
            instructions.push(instruction);
        }

        let mut terminator = match block.terminator {
            NumericTerminator::Jump => MachineInstruction::plain(MachineOpcode::Jump, Vec::new()),
            NumericTerminator::Branch {
                condition,
                when_true,
            } => MachineInstruction::plain(
                MachineOpcode::BranchIf(when_true),
                vec![MachineOperand::register_input(machine_value(
                    &values, condition,
                ))],
            ),
            NumericTerminator::Return(value) => {
                if hir.nodes[value.0].value_type() == NumericType::Tagged {
                    let mut ret = MachineInstruction::plain(
                        MachineOpcode::Return,
                        vec![MachineOperand::register_input(machine_value(
                            &values, value,
                        ))],
                    );
                    ret.control = ControlFlow::Return;
                    instructions.push(ret);
                    let end = MachineInstructionId(instructions.len() as u32);
                    blocks.push(machine_block(
                        hir,
                        &selection_cfg,
                        block_index,
                        &values,
                        first,
                        end,
                    ));
                    continue;
                }
                let boxed = push_value(&mut representations, MachineRepresentation::Tagged);
                let box_opcode = match hir.nodes[value.0].value_type() {
                    NumericType::Tagged => unreachable!("tagged returns bypass boxing"),
                    NumericType::Int32 => MachineOpcode::BoxInt32,
                    NumericType::Uint32 => MachineOpcode::BoxUint32,
                    NumericType::Number => MachineOpcode::BoxNumber,
                    NumericType::Boolean => MachineOpcode::BoxBoolean,
                };
                instructions.push(MachineInstruction::plain(
                    box_opcode,
                    vec![
                        MachineOperand::register_input(machine_value(&values, value)),
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
                blocks.push(machine_block(
                    hir,
                    &selection_cfg,
                    block_index,
                    &values,
                    first,
                    end,
                ));
                continue;
            }
        };
        terminator.control = ControlFlow::Branch;
        instructions.push(terminator);
        let end = MachineInstructionId(instructions.len() as u32);
        blocks.push(machine_block(
            hir,
            &selection_cfg,
            block_index,
            &values,
            first,
            end,
        ));
    }

    InstructionSequence::new(
        selection_cfg.originals[0],
        representations,
        call_descriptors,
        blocks,
        instructions,
    )
}

fn intern_leaf_boolean_call_descriptor(
    descriptors: &mut Vec<CallDescriptor>,
    target: otter_vm::native_abi::RuntimeStubDescriptor,
    argument_count: usize,
) -> usize {
    intern_call_descriptor(
        descriptors,
        leaf_boolean_call_descriptor(target, argument_count),
    )
}

fn intern_call_descriptor(
    descriptors: &mut Vec<CallDescriptor>,
    descriptor: CallDescriptor,
) -> usize {
    if let Some(index) = descriptors
        .iter()
        .position(|candidate| candidate.target == descriptor.target)
    {
        return index;
    }
    descriptors.push(descriptor);
    descriptors.len() - 1
}

fn string_concat_call_descriptor() -> CallDescriptor {
    let mut clobbers = TargetRegisterFile::aarch64_scalar_call_clobbers();
    clobbers.retain(|register| *register != PhysicalRegister::integer(0));
    CallDescriptor {
        target: CallTarget::RuntimeStub(otter_vm::native_abi::STUB_STRING_CONCAT_ALLOC),
        arguments: vec![MachineRepresentation::Tagged; 3],
        result: Some(MachineRepresentation::Tagged),
        effects: CallEffects::READS_HEAP,
        clobbers,
        exceptional: ExceptionalEdge::None,
        safepoint: SafepointKind::Gc,
    }
}

fn attach_safepoint_roots(
    representations: &[MachineRepresentation],
    instruction: &mut MachineInstruction,
) {
    if instruction.safepoint.is_none() {
        return;
    }
    let roots = instruction
        .operands
        .iter()
        .filter(|operand| operand.purpose == super::OperandPurpose::Deopt)
        .filter(|operand| {
            representations[operand.value.0 as usize] == MachineRepresentation::Tagged
        })
        .map(|operand| operand.value)
        .collect::<BTreeSet<_>>();
    instruction
        .operands
        .extend(roots.into_iter().map(MachineOperand::tagged_root));
}

fn leaf_boolean_call_descriptor(
    target: otter_vm::native_abi::RuntimeStubDescriptor,
    argument_count: usize,
) -> CallDescriptor {
    let mut clobbers = TargetRegisterFile::aarch64_scalar_call_clobbers();
    clobbers.retain(|register| *register != PhysicalRegister::integer(0));
    CallDescriptor {
        target: CallTarget::RuntimeStub(target),
        arguments: vec![MachineRepresentation::Tagged; argument_count],
        result: Some(MachineRepresentation::Int32),
        effects: CallEffects::READS_HEAP,
        clobbers,
        exceptional: ExceptionalEdge::None,
        safepoint: SafepointKind::None,
    }
}

fn tagged_call_argument(
    hir: &NumericFunction,
    values: &[MachineValue],
    representations: &mut Vec<MachineRepresentation>,
    instructions: &mut Vec<MachineInstruction>,
    source: hir::NumericValue,
) -> MachineValue {
    if hir.nodes[source.0].value_type() == NumericType::Tagged {
        return machine_value(values, source);
    }
    let tagged = push_value(representations, MachineRepresentation::Tagged);
    let opcode = match hir.nodes[source.0].value_type() {
        NumericType::Tagged => unreachable!("tagged call argument returned early"),
        NumericType::Int32 => MachineOpcode::BoxInt32,
        NumericType::Uint32 => MachineOpcode::BoxUint32,
        NumericType::Number => MachineOpcode::BoxNumber,
        NumericType::Boolean => MachineOpcode::BoxBoolean,
    };
    instructions.push(MachineInstruction::plain(
        opcode,
        vec![
            MachineOperand::register_input(machine_value(values, source)),
            MachineOperand::register_output(tagged),
        ],
    ));
    tagged
}

fn attach_frame_state(
    hir: &NumericFunction,
    values: &[MachineValue],
    deopt: DeoptId,
    instruction: &mut MachineInstruction,
) {
    let state = &hir.frame_states[deopt.0 as usize];
    let mut values_at_exit = BTreeSet::new();
    for slot in &state.slots {
        let hir::NumericFrameSlot::Value(value) = slot else {
            continue;
        };
        if values_at_exit.insert(*value) {
            instruction
                .operands
                .push(MachineOperand::deopt(machine_value(values, *value)));
        }
    }
    instruction.deopt = Some(deopt);
}

fn machine_frame_states(hir: &NumericFunction) -> Vec<super::MachineFrameState> {
    hir.frame_states
        .iter()
        .enumerate()
        .map(|(index, state)| super::MachineFrameState {
            id: DeoptId(index as u32),
            function_id: state.function_id,
            byte_pc: state.byte_pc,
            slots: state
                .slots
                .iter()
                .map(|slot| match slot {
                    hir::NumericFrameSlot::Value(value) => {
                        super::MachineFrameSlot::Value(MachineValue(value.0 as u32))
                    }
                    hir::NumericFrameSlot::Undefined => super::undefined_slot(),
                })
                .collect(),
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectedBlock {
    Original(usize),
    SplitEdge {
        predecessor: usize,
        edge: usize,
        successor: usize,
    },
}

struct SelectionCfg {
    order: Vec<SelectedBlock>,
    originals: Vec<MachineBlock>,
    split_edges: BTreeMap<(usize, usize), MachineBlock>,
}

impl SelectionCfg {
    fn build(hir: &NumericFunction) -> Self {
        let mut order = Vec::with_capacity(hir.blocks.len());
        let mut originals = vec![MachineBlock(u32::MAX); hir.blocks.len()];
        let mut split_edges = BTreeMap::new();
        for (successor, original) in originals.iter_mut().enumerate() {
            for (predecessor, edge) in incoming_edges(hir, successor) {
                if is_critical_edge(hir, predecessor, successor) || successor <= predecessor {
                    let block = MachineBlock(order.len() as u32);
                    split_edges.insert((predecessor, edge), block);
                    order.push(SelectedBlock::SplitEdge {
                        predecessor,
                        edge,
                        successor,
                    });
                }
            }
            *original = MachineBlock(order.len() as u32);
            order.push(SelectedBlock::Original(successor));
        }
        Self {
            order,
            originals,
            split_edges,
        }
    }
}

fn incoming_edges(hir: &NumericFunction, successor: usize) -> Vec<(usize, usize)> {
    hir.blocks
        .iter()
        .enumerate()
        .flat_map(|(predecessor, block)| {
            block
                .successors
                .iter()
                .enumerate()
                .filter_map(move |(edge, &target)| {
                    (target == successor).then_some((predecessor, edge))
                })
        })
        .collect()
}

fn is_critical_edge(hir: &NumericFunction, predecessor: usize, successor: usize) -> bool {
    hir.blocks[predecessor].successors.len() > 1 && hir.blocks[successor].predecessors.len() > 1
}

fn machine_block(
    hir: &NumericFunction,
    selection_cfg: &SelectionCfg,
    block_index: usize,
    values: &[MachineValue],
    first: MachineInstructionId,
    end: MachineInstructionId,
) -> MachineBlockData {
    let block = &hir.blocks[block_index];
    let mut predecessors = incoming_edges(hir, block_index)
        .into_iter()
        .map(|(predecessor, edge)| {
            selection_cfg
                .split_edges
                .get(&(predecessor, edge))
                .copied()
                .unwrap_or(selection_cfg.originals[predecessor])
        })
        .collect::<Vec<_>>();
    predecessors.sort_unstable();
    MachineBlockData {
        first,
        end,
        predecessors,
        successors: block
            .successors
            .iter()
            .enumerate()
            .map(|(edge, &successor)| {
                selection_cfg
                    .split_edges
                    .get(&(block_index, edge))
                    .copied()
                    .unwrap_or(selection_cfg.originals[successor])
            })
            .collect(),
        parameters: block
            .parameters
            .iter()
            .map(|&value| machine_value(values, value))
            .collect(),
        successor_arguments: block
            .successor_arguments
            .iter()
            .enumerate()
            .map(|(edge, arguments)| {
                if selection_cfg.split_edges.contains_key(&(block_index, edge)) {
                    Vec::new()
                } else {
                    arguments
                        .iter()
                        .map(|&value| machine_value(values, value))
                        .collect()
                }
            })
            .collect(),
    }
}

fn push_value(
    representations: &mut Vec<MachineRepresentation>,
    representation: MachineRepresentation,
) -> MachineValue {
    let value = MachineValue(representations.len() as u32);
    representations.push(representation);
    value
}

fn machine_value(values: &[MachineValue], value: hir::NumericValue) -> MachineValue {
    values[value.0]
}

#[cfg(test)]
mod tests {
    use otter_bytecode::{Op, Operand};
    use otter_vm::{
        JitArtifactFileName, JitArtifactIdentity, JitCompileSnapshot, JitDebugTarget, JitDebugTier,
        JitFunctionCode, Value,
        jit::JitTestInstruction,
        jit_feedback::{ARITH_FLOAT64, ARITH_INT32, ARITH_STRING, ArithFeedback},
        native_abi::{NativeFrame, NativeFrameFlags, NativeFrameKind, VmFrameHeader, VmThread},
        value::tag,
    };

    use super::*;
    use crate::entry::{JitCtx, JitEntry, JitRet, STATUS_BAILED, STATUS_RETURNED};
    use crate::machine::{
        AllocatedLocation, OperandConstraint, OperandPurpose, OperandTiming, SafepointId,
        lower_deopt_table,
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
                Op::Add
                    | Op::Sub
                    | Op::Mul
                    | Op::Div
                    | Op::Rem
                    | Op::Pow
                    | Op::Neg
                    | Op::Equal
                    | Op::NotEqual
                    | Op::LessThan
                    | Op::LessEq
                    | Op::GreaterThan
                    | Op::GreaterEq
                    | Op::Increment
                    | Op::AddImm
                    | Op::SubImm
                    | Op::BitwiseAndImm
                    | Op::LessThanImm
                    | Op::EqualImm
                    | Op::NotEqualImm
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

    fn typed_parameter_leaf_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
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
                        Operand::Register(1),
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
                        Operand::Register(1),
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
                        Operand::Register(1),
                    ],
                ),
                (Op::Neg, vec![Operand::Register(9), Operand::Register(8)]),
                (Op::ReturnValue, vec![Operand::Register(9)]),
            ],
        );
        for pc in 0..=5 {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view
    }

    fn typed_parameter_overflow_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            2,
            3,
            vec![
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        view.seed_arith_feedback_for_test(0, ArithFeedback::from_bits(ARITH_INT32));
        view
    }

    fn typed_parameter_leaf_overflow_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            1,
            7,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(5)]),
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(2)]),
                (
                    Op::Div,
                    vec![
                        Operand::Register(3),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::Rem,
                    vec![
                        Operand::Register(4),
                        Operand::Register(3),
                        Operand::Register(2),
                    ],
                ),
                (Op::LoadInt32, vec![Operand::Register(5), Operand::Imm32(1)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(6),
                        Operand::Register(0),
                        Operand::Register(5),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(6)]),
            ],
        );
        view.seed_arith_feedback_for_test(5, ArithFeedback::from_bits(ARITH_INT32));
        view
    }

    fn typed_parameter_alias_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            1,
            4,
            vec![
                (
                    Op::StoreLocal,
                    vec![Operand::Register(0), Operand::Imm32(2)],
                ),
                (Op::LoadLocal, vec![Operand::Register(1), Operand::Imm32(2)]),
                (
                    Op::AddImm,
                    vec![
                        Operand::Register(3),
                        Operand::Register(1),
                        Operand::Imm32(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
        );
        view.seed_arith_feedback_for_test(2, ArithFeedback::from_bits(ARITH_INT32));
        view
    }

    fn typed_parameter_loop_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            2,
            8,
            vec![
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(0)]),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(4),
                        Operand::Register(3),
                        Operand::Register(0),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(5), Operand::Register(4)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(5),
                        Operand::Register(2),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(5), Operand::Imm32(2)],
                ),
                (
                    Op::AddImm,
                    vec![
                        Operand::Register(6),
                        Operand::Register(3),
                        Operand::Imm32(1),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(6), Operand::Imm32(3)],
                ),
                (Op::Jump, vec![Operand::Imm32(-7)]),
                (Op::LoadLocal, vec![Operand::Register(7), Operand::Imm32(2)]),
                (Op::ReturnValue, vec![Operand::Register(7)]),
            ],
        );
        for pc in [2_u32, 4, 6] {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view
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

    fn tagged_identity_view() -> JitCompileSnapshot {
        numeric_view(1, 1, vec![(Op::ReturnValue, vec![Operand::Register(0)])])
    }

    fn tagged_immediate_view(op: Op) -> JitCompileSnapshot {
        assert!(matches!(op, Op::LoadUndefined | Op::LoadNull));
        numeric_view(
            0,
            1,
            vec![
                (op, vec![Operand::Register(0)]),
                (Op::ReturnValue, vec![Operand::Register(0)]),
            ],
        )
    }

    fn tagged_this_view() -> JitCompileSnapshot {
        numeric_view(
            0,
            1,
            vec![
                (Op::LoadThis, vec![Operand::Register(0)]),
                (Op::Return, vec![Operand::Register(0)]),
            ],
        )
    }

    fn tagged_phi_view() -> JitCompileSnapshot {
        numeric_view(
            4,
            6,
            vec![
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(4),
                        Operand::Register(2),
                        Operand::Register(3),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(4)],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(0), Operand::Imm32(5)],
                ),
                (Op::Jump, vec![Operand::Imm32(1)]),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(1), Operand::Imm32(5)],
                ),
                (Op::ReturnValue, vec![Operand::Register(5)]),
            ],
        )
    }

    fn tagged_loop_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            2,
            5,
            vec![
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(4),
                        Operand::Register(2),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(4)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(2),
                        Operand::Register(3),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(-4)]),
                (Op::ReturnValue, vec![Operand::Register(0)]),
            ],
        );
        for pc in [2_u32, 4] {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view
    }

    fn tagged_truthiness_branch_view() -> JitCompileSnapshot {
        numeric_view(
            3,
            3,
            vec![
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(1), Operand::Register(0)],
                ),
                (Op::ReturnValue, vec![Operand::Register(1)]),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        )
    }

    fn tagged_logical_not_view() -> JitCompileSnapshot {
        numeric_view(
            1,
            2,
            vec![
                (
                    Op::LogicalNot,
                    vec![Operand::Register(1), Operand::Register(0)],
                ),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        )
    }

    fn tagged_strict_equality_view(op: Op) -> JitCompileSnapshot {
        JitCompileSnapshot::without_feedback(
            72,
            2,
            3,
            vec![
                JitTestInstruction::new(
                    op,
                    0,
                    0,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 1, 8, vec![Operand::Register(2)]),
            ],
        )
    }

    fn tagged_mixed_strict_equality_view() -> JitCompileSnapshot {
        JitCompileSnapshot::without_feedback(
            73,
            1,
            3,
            vec![
                JitTestInstruction::new(
                    Op::LoadInt32,
                    0,
                    0,
                    vec![Operand::Register(1), Operand::Imm32(7)],
                ),
                JitTestInstruction::new(
                    Op::Equal,
                    1,
                    8,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 2, 16, vec![Operand::Register(2)]),
            ],
        )
    }

    fn tagged_string_concat_view(parameter_count: u16) -> JitCompileSnapshot {
        assert!(parameter_count >= 2);
        let accumulator = parameter_count;
        let mut instructions = Vec::with_capacity(usize::from(parameter_count));
        instructions.push((
            Op::Add,
            vec![
                Operand::Register(accumulator),
                Operand::Register(0),
                Operand::Register(1),
            ],
        ));
        for parameter in 2..parameter_count {
            instructions.push((
                Op::Add,
                vec![
                    Operand::Register(accumulator),
                    Operand::Register(accumulator),
                    Operand::Register(parameter),
                ],
            ));
        }
        instructions.push((Op::ReturnValue, vec![Operand::Register(accumulator)]));
        let mut view = JitCompileSnapshot::without_feedback(
            74,
            parameter_count,
            parameter_count + 1,
            instructions
                .into_iter()
                .enumerate()
                .map(|(pc, (op, operands))| {
                    JitTestInstruction::new(op, pc as u32, pc as u32 * 8, operands)
                })
                .collect(),
        );
        for pc in 0..u32::from(parameter_count - 1) {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_STRING));
        }
        view
    }

    fn unused_parameter_view() -> JitCompileSnapshot {
        numeric_view(
            2,
            3,
            vec![
                (Op::Neg, vec![Operand::Register(2), Operand::Register(1)]),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        )
    }

    fn float_bitwise_view(op: Op) -> JitCompileSnapshot {
        assert!(matches!(
            op,
            Op::BitwiseAnd | Op::BitwiseOr | Op::BitwiseXor | Op::Shl | Op::Shr | Op::Ushr
        ));
        numeric_view(
            2,
            3,
            vec![
                (
                    op,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        )
    }

    fn boolean_bitwise_view() -> JitCompileSnapshot {
        numeric_view(
            0,
            4,
            vec![
                (Op::Nop, vec![]),
                (Op::LoadTrue, vec![Operand::Register(0)]),
                (Op::LoadFalse, vec![Operand::Register(1)]),
                (
                    Op::BitwiseAndImm,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Imm32(3),
                    ],
                ),
                (
                    Op::BitwiseOr,
                    vec![
                        Operand::Register(3),
                        Operand::Register(2),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
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

    fn diamond_view(branch: Op) -> JitCompileSnapshot {
        assert!(matches!(branch, Op::JumpIfTrue | Op::JumpIfFalse));
        numeric_view(
            2,
            4,
            vec![
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (branch, vec![Operand::Imm32(2), Operand::Register(2)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(1)]),
                (
                    Op::Sub,
                    vec![
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
        )
    }

    fn critical_edge_view() -> JitCompileSnapshot {
        numeric_view(
            2,
            4,
            vec![
                (Op::LoadLocal, vec![Operand::Register(3), Operand::Imm32(0)]),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(2)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(0)]),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
        )
    }

    fn loop_view() -> JitCompileSnapshot {
        numeric_view(
            1,
            3,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(1)]),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(2)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(0),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(-4)]),
                (Op::ReturnValue, vec![Operand::Register(0)]),
            ],
        )
    }

    fn branch_phi_loop_view() -> JitCompileSnapshot {
        branch_phi_loop_view_with(0, 0, 1_000_000, 1)
    }

    fn countdown_loop_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            0,
            7,
            vec![
                (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(5)]),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
                (
                    Op::NotEqualImm,
                    vec![
                        Operand::Register(4),
                        Operand::Register(0),
                        Operand::Imm32(0),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(7), Operand::Register(4)],
                ),
                (
                    Op::Sub,
                    vec![
                        Operand::Register(5),
                        Operand::Register(0),
                        Operand::Register(3),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(5), Operand::Imm32(0)],
                ),
                (
                    Op::SubImm,
                    vec![
                        Operand::Register(6),
                        Operand::Register(1),
                        Operand::Imm32(-3),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(6), Operand::Imm32(1)],
                ),
                (
                    Op::Increment,
                    vec![
                        Operand::Register(6),
                        Operand::Register(2),
                        Operand::Imm32(1),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(6), Operand::Imm32(2)],
                ),
                (Op::Jump, vec![Operand::Imm32(-9)]),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        for pc in [4_u32, 6, 8, 10] {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view
    }

    fn bitwise_loop_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            0,
            13,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(0), Operand::Imm32(0x1234_5678)],
                ),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(2), Operand::Imm32(-1)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(3), Operand::Imm32(i32::MAX)],
                ),
                (
                    Op::LessThanImm,
                    vec![
                        Operand::Register(4),
                        Operand::Register(1),
                        Operand::Imm32(35),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(10), Operand::Register(4)],
                ),
                (
                    Op::Shl,
                    vec![
                        Operand::Register(5),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Shr,
                    vec![
                        Operand::Register(7),
                        Operand::Register(0),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::BitwiseXor,
                    vec![
                        Operand::Register(8),
                        Operand::Register(5),
                        Operand::Register(7),
                    ],
                ),
                (
                    Op::BitwiseOr,
                    vec![
                        Operand::Register(9),
                        Operand::Register(8),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::BitwiseAnd,
                    vec![
                        Operand::Register(10),
                        Operand::Register(9),
                        Operand::Register(3),
                    ],
                ),
                (
                    Op::BitwiseNot,
                    vec![Operand::Register(11), Operand::Register(10)],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(11), Operand::Imm32(0)],
                ),
                (
                    Op::AddImm,
                    vec![
                        Operand::Register(12),
                        Operand::Register(1),
                        Operand::Imm32(1),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(12), Operand::Imm32(1)],
                ),
                (Op::Jump, vec![Operand::Imm32(-12)]),
                (Op::ReturnValue, vec![Operand::Register(0)]),
            ],
        );
        for pc in [4_u32, 13] {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view
    }

    fn checked_binary_view(op: Op, left: i32, right: i32) -> JitCompileSnapshot {
        assert!(matches!(op, Op::Add | Op::Sub | Op::Mul));
        let mut view = numeric_view(
            0,
            3,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(0), Operand::Imm32(left)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(1), Operand::Imm32(right)],
                ),
                (
                    op,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        view.seed_arith_feedback_for_test(2, ArithFeedback::from_bits(ARITH_INT32));
        view
    }

    fn checked_immediate_view(op: Op, source: i32, immediate: i32) -> JitCompileSnapshot {
        assert!(matches!(op, Op::SubImm | Op::Increment));
        let mut view = numeric_view(
            0,
            2,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(0), Operand::Imm32(source)],
                ),
                (
                    op,
                    vec![
                        Operand::Register(1),
                        Operand::Register(0),
                        Operand::Imm32(immediate),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        view.seed_arith_feedback_for_test(1, ArithFeedback::from_bits(ARITH_INT32));
        view
    }

    fn checked_neg_view(source: i32) -> JitCompileSnapshot {
        let mut view = numeric_view(
            0,
            2,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(0), Operand::Imm32(source)],
                ),
                (Op::Neg, vec![Operand::Register(1), Operand::Register(0)]),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        view.seed_arith_feedback_for_test(1, ArithFeedback::from_bits(ARITH_INT32));
        view
    }

    fn float_binary_view(op: Op) -> JitCompileSnapshot {
        assert!(matches!(op, Op::Rem | Op::Pow));
        let mut view = numeric_view(
            2,
            3,
            vec![
                (
                    op,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        view.seed_arith_feedback_for_test(0, ArithFeedback::from_bits(ARITH_FLOAT64));
        view
    }

    fn float_truthiness_view() -> JitCompileSnapshot {
        numeric_view(
            1,
            4,
            vec![
                (
                    Op::ToNumber,
                    vec![Operand::Register(1), Operand::Register(0)],
                ),
                (
                    Op::ToBoolean,
                    vec![Operand::Register(2), Operand::Register(1)],
                ),
                (
                    Op::LogicalNot,
                    vec![Operand::Register(3), Operand::Register(2)],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
        )
    }

    fn integer_truthiness_view(value: i32) -> JitCompileSnapshot {
        numeric_view(
            0,
            3,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(0), Operand::Imm32(value)],
                ),
                (
                    Op::ToBoolean,
                    vec![Operand::Register(1), Operand::Register(0)],
                ),
                (
                    Op::LogicalNot,
                    vec![Operand::Register(2), Operand::Register(1)],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        )
    }

    fn ushr_view(left: i32, shift: i32) -> JitCompileSnapshot {
        numeric_view(
            0,
            3,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(0), Operand::Imm32(left)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(1), Operand::Imm32(shift)],
                ),
                (
                    Op::Ushr,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        )
    }

    fn ushr_comparison_view() -> JitCompileSnapshot {
        numeric_view(
            0,
            5,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(0), Operand::Imm32(-1)],
                ),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (
                    Op::Ushr,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(3), Operand::Imm32(i32::MAX)],
                ),
                (
                    Op::GreaterThan,
                    vec![
                        Operand::Register(4),
                        Operand::Register(2),
                        Operand::Register(3),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(4)]),
            ],
        )
    }

    fn ushr_backedge_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            0,
            8,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(0), Operand::Imm32(-1)],
                ),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (
                    Op::Ushr,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(2), Operand::Imm32(0)],
                ),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(0)]),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(3), Operand::Imm32(1)],
                ),
                (
                    Op::LessThanImm,
                    vec![
                        Operand::Register(4),
                        Operand::Register(1),
                        Operand::Imm32(2),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(5), Operand::Register(4)],
                ),
                (
                    Op::Ushr,
                    vec![
                        Operand::Register(5),
                        Operand::Register(0),
                        Operand::Register(3),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(5), Operand::Imm32(0)],
                ),
                (
                    Op::AddImm,
                    vec![
                        Operand::Register(6),
                        Operand::Register(1),
                        Operand::Imm32(1),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(6), Operand::Imm32(1)],
                ),
                (Op::Jump, vec![Operand::Imm32(-7)]),
                (Op::ReturnValue, vec![Operand::Register(0)]),
            ],
        );
        for pc in [6_u32, 10] {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view
    }

    fn integer_comparison_view(op: Op, left: i32, right: i32) -> JitCompileSnapshot {
        assert!(matches!(
            op,
            Op::Equal | Op::NotEqual | Op::LessThan | Op::LessEq | Op::GreaterThan | Op::GreaterEq
        ));
        let mut view = numeric_view(
            0,
            3,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(0), Operand::Imm32(left)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(1), Operand::Imm32(right)],
                ),
                (
                    op,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        view.seed_arith_feedback_for_test(2, ArithFeedback::from_bits(ARITH_INT32));
        view
    }

    fn float_comparison_view(op: Op) -> JitCompileSnapshot {
        assert!(matches!(
            op,
            Op::Equal | Op::NotEqual | Op::LessThan | Op::LessEq | Op::GreaterThan | Op::GreaterEq
        ));
        let mut view = numeric_view(
            2,
            3,
            vec![
                (
                    op,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        view.seed_arith_feedback_for_test(0, ArithFeedback::from_bits(ARITH_FLOAT64));
        view
    }

    fn integer_scalar_loop_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            0,
            13,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(7), Operand::Imm32(-1)],
                ),
                (Op::LoadInt32, vec![Operand::Register(8), Operand::Imm32(0)]),
                (
                    Op::Ushr,
                    vec![
                        Operand::Register(0),
                        Operand::Register(7),
                        Operand::Register(8),
                    ],
                ),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(2), Operand::Imm32(1_000_000)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(3), Operand::Imm32(1023)],
                ),
                (Op::LoadInt32, vec![Operand::Register(4), Operand::Imm32(3)]),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(7),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(15), Operand::Register(7)],
                ),
                (
                    Op::BitwiseAnd,
                    vec![
                        Operand::Register(8),
                        Operand::Register(1),
                        Operand::Register(3),
                    ],
                ),
                (Op::LoadLocal, vec![Operand::Register(9), Operand::Imm32(4)]),
                (
                    Op::Mul,
                    vec![
                        Operand::Register(5),
                        Operand::Register(8),
                        Operand::Register(9),
                    ],
                ),
                (
                    Op::BitwiseAndImm,
                    vec![
                        Operand::Register(6),
                        Operand::Register(1),
                        Operand::Imm32(7),
                    ],
                ),
                (
                    Op::BitwiseXor,
                    vec![
                        Operand::Register(8),
                        Operand::Register(0),
                        Operand::Register(5),
                    ],
                ),
                (Op::LoadLocal, vec![Operand::Register(9), Operand::Imm32(6)]),
                (
                    Op::Ushr,
                    vec![
                        Operand::Register(10),
                        Operand::Register(8),
                        Operand::Register(9),
                    ],
                ),
                (
                    Op::LoadLocal,
                    vec![Operand::Register(11), Operand::Imm32(5)],
                ),
                (
                    Op::BitwiseOr,
                    vec![
                        Operand::Register(8),
                        Operand::Register(10),
                        Operand::Register(11),
                    ],
                ),
                (Op::LoadInt32, vec![Operand::Register(9), Operand::Imm32(0)]),
                (
                    Op::Ushr,
                    vec![
                        Operand::Register(10),
                        Operand::Register(8),
                        Operand::Register(9),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(10), Operand::Imm32(0)],
                ),
                (
                    Op::AddImm,
                    vec![
                        Operand::Register(11),
                        Operand::Register(1),
                        Operand::Imm32(1),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(11), Operand::Imm32(1)],
                ),
                (Op::Jump, vec![Operand::Imm32(-17)]),
                (
                    Op::LoadLocal,
                    vec![Operand::Register(12), Operand::Imm32(0)],
                ),
                (Op::ReturnValue, vec![Operand::Register(12)]),
            ],
        );
        for pc in [7_u32, 11, 12, 21] {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view
    }

    fn float_leaf_loop_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            0,
            19,
            vec![
                (Op::LoadInt32, vec![Operand::Register(9), Operand::Imm32(1)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(10), Operand::Imm32(2)],
                ),
                (
                    Op::Div,
                    vec![
                        Operand::Register(0),
                        Operand::Register(9),
                        Operand::Register(10),
                    ],
                ),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(1)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(3), Operand::Imm32(200_000)],
                ),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(9),
                        Operand::Register(2),
                        Operand::Register(3),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(28), Operand::Register(9)],
                ),
                (
                    Op::LoadLocal,
                    vec![Operand::Register(10), Operand::Imm32(2)],
                ),
                (
                    Op::ToPrimitive,
                    vec![
                        Operand::Register(11),
                        Operand::Register(10),
                        Operand::ConstIndex(1),
                    ],
                ),
                (Op::Neg, vec![Operand::Register(10), Operand::Register(11)]),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(10), Operand::Imm32(4)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(11), Operand::Imm32(17)],
                ),
                (
                    Op::Rem,
                    vec![
                        Operand::Register(5),
                        Operand::Register(4),
                        Operand::Register(11),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(6),
                        Operand::Register(5),
                        Operand::Register(0),
                    ],
                ),
                (
                    Op::Mul,
                    vec![
                        Operand::Register(11),
                        Operand::Register(6),
                        Operand::Register(6),
                    ],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(12), Operand::Imm32(1)],
                ),
                (
                    Op::Pow,
                    vec![
                        Operand::Register(7),
                        Operand::Register(11),
                        Operand::Register(12),
                    ],
                ),
                (
                    Op::Sub,
                    vec![
                        Operand::Register(11),
                        Operand::Register(7),
                        Operand::Register(7),
                    ],
                ),
                (
                    Op::ToNumber,
                    vec![Operand::Register(11), Operand::Register(11)],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(11), Operand::Imm32(8)],
                ),
                (
                    Op::LoadLocal,
                    vec![Operand::Register(12), Operand::Imm32(8)],
                ),
                (
                    Op::LogicalNot,
                    vec![Operand::Register(12), Operand::Register(12)],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(12)],
                ),
                (
                    Op::AddImm,
                    vec![
                        Operand::Register(13),
                        Operand::Register(1),
                        Operand::Imm32(1),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(13), Operand::Imm32(1)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(14), Operand::Imm32(13)],
                ),
                (
                    Op::Rem,
                    vec![
                        Operand::Register(15),
                        Operand::Register(7),
                        Operand::Register(14),
                    ],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(16), Operand::Imm32(1)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(17), Operand::Imm32(2)],
                ),
                (
                    Op::Div,
                    vec![
                        Operand::Register(18),
                        Operand::Register(16),
                        Operand::Register(17),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(14),
                        Operand::Register(15),
                        Operand::Register(18),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(14), Operand::Imm32(0)],
                ),
                (
                    Op::AddImm,
                    vec![
                        Operand::Register(15),
                        Operand::Register(2),
                        Operand::Imm32(1),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(15), Operand::Imm32(2)],
                ),
                (Op::Jump, vec![Operand::Imm32(-30)]),
                (
                    Op::LoadLocal,
                    vec![Operand::Register(16), Operand::Imm32(1)],
                ),
                (Op::ReturnValue, vec![Operand::Register(16)]),
            ],
        );
        for pc in [6_u32, 10, 24, 33] {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view
    }

    fn float_bitwise_loop_view() -> JitCompileSnapshot {
        let r = Operand::Register;
        let i = Operand::Imm32;
        let c = Operand::ConstIndex;
        let mut view = numeric_view(
            0,
            12,
            vec![
                (Op::LoadNumber, vec![r(0), c(1)]),
                (Op::LoadInt32, vec![r(1), i(0)]),
                (Op::LoadInt32, vec![r(2), i(0)]),
                (Op::LoadInt32, vec![r(3), i(200_000)]),
                (Op::LessThan, vec![r(6), r(2), r(3)]),
                (Op::JumpIfFalse, vec![i(17), r(6)]),
                (Op::LoadInt32, vec![r(7), i(0)]),
                (Op::BitwiseOr, vec![r(4), r(0), r(7)]),
                (Op::LoadLocal, vec![r(7), i(0)]),
                (Op::BitwiseAndImm, vec![r(8), r(2), i(7)]),
                (Op::Ushr, vec![r(5), r(7), r(8)]),
                (Op::BitwiseXor, vec![r(7), r(1), r(4)]),
                (Op::LoadLocal, vec![r(8), i(5)]),
                (Op::BitwiseXor, vec![r(9), r(7), r(8)]),
                (Op::LoadInt32, vec![r(10), i(0)]),
                (Op::BitwiseOr, vec![r(7), r(9), r(10)]),
                (Op::StoreLocal, vec![r(7), i(1)]),
                (Op::LoadNumber, vec![r(8), c(2)]),
                (Op::Add, vec![r(9), r(0), r(8)]),
                (Op::StoreLocal, vec![r(9), i(0)]),
                (Op::AddImm, vec![r(10), r(2), i(1)]),
                (Op::StoreLocal, vec![r(10), i(2)]),
                (Op::Jump, vec![i(-19)]),
                (Op::LoadLocal, vec![r(11), i(1)]),
                (Op::ReturnValue, vec![r(11)]),
            ],
        );
        view.instructions[0].load_number = Some(4_294_967_297.75);
        view.instructions[17].load_number = Some(1.5);
        for pc in [4_u32, 20] {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view.seed_arith_feedback_for_test(18, ArithFeedback::from_bits(ARITH_FLOAT64));
        view
    }

    fn mixed_osr_loop_view() -> JitCompileSnapshot {
        let r = Operand::Register;
        let i = Operand::Imm32;
        let mut view = numeric_view(
            0,
            10,
            vec![
                (Op::LoadInt32, vec![r(7), i(-1)]),
                (Op::LoadInt32, vec![r(8), i(0)]),
                (Op::Ushr, vec![r(0), r(7), r(8)]),
                (Op::LoadTrue, vec![r(1)]),
                (Op::LoadInt32, vec![r(2), i(0)]),
                (Op::LoadInt32, vec![r(3), i(3)]),
                (Op::LessThan, vec![r(4), r(2), r(3)]),
                (Op::JumpIfFalse, vec![i(8), r(4)]),
                (Op::LoadInt32, vec![r(5), i(1)]),
                (Op::Ushr, vec![r(6), r(0), r(5)]),
                (Op::StoreLocal, vec![r(6), i(0)]),
                (Op::LogicalNot, vec![r(7), r(1)]),
                (Op::StoreLocal, vec![r(7), i(1)]),
                (Op::AddImm, vec![r(8), r(2), i(1)]),
                (Op::StoreLocal, vec![r(8), i(2)]),
                (Op::Jump, vec![i(-10)]),
                (Op::ReturnValue, vec![r(0)]),
            ],
        );
        for pc in [6_u32, 13] {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view
    }

    fn osr_spill_pressure_loop_view() -> JitCompileSnapshot {
        const LIVE_VALUES: u16 = 30;
        let loop_index = LIVE_VALUES;
        let loop_limit = LIVE_VALUES + 1;
        let condition = LIVE_VALUES + 2;
        let accumulator = LIVE_VALUES + 3;
        let scratch = LIVE_VALUES + 4;
        let mut instructions = (0_u16..LIVE_VALUES)
            .map(|register| {
                (
                    Op::LoadInt32,
                    vec![
                        Operand::Register(register),
                        Operand::Imm32(i32::from(register) + 1),
                    ],
                )
            })
            .collect::<Vec<_>>();
        instructions.extend([
            (
                Op::LoadInt32,
                vec![Operand::Register(loop_index), Operand::Imm32(0)],
            ),
            (
                Op::LoadInt32,
                vec![Operand::Register(loop_limit), Operand::Imm32(1)],
            ),
            (
                Op::LoadInt32,
                vec![Operand::Register(accumulator), Operand::Imm32(0)],
            ),
            (
                Op::LessThan,
                vec![
                    Operand::Register(condition),
                    Operand::Register(loop_index),
                    Operand::Register(loop_limit),
                ],
            ),
            (
                Op::JumpIfFalse,
                vec![
                    Operand::Imm32(i32::from(LIVE_VALUES) * 2 + 3),
                    Operand::Register(condition),
                ],
            ),
        ]);
        for register in 0_u16..LIVE_VALUES {
            instructions.push((
                Op::Add,
                vec![
                    Operand::Register(scratch),
                    Operand::Register(accumulator),
                    Operand::Register(register),
                ],
            ));
            instructions.push((
                Op::StoreLocal,
                vec![
                    Operand::Register(scratch),
                    Operand::Imm32(i32::from(accumulator)),
                ],
            ));
        }
        instructions.extend([
            (
                Op::AddImm,
                vec![
                    Operand::Register(scratch),
                    Operand::Register(loop_index),
                    Operand::Imm32(1),
                ],
            ),
            (
                Op::StoreLocal,
                vec![
                    Operand::Register(scratch),
                    Operand::Imm32(i32::from(loop_index)),
                ],
            ),
            (
                Op::Jump,
                vec![Operand::Imm32(-(i32::from(LIVE_VALUES) * 2 + 5))],
            ),
            (Op::ReturnValue, vec![Operand::Register(accumulator)]),
        ]);
        let mut view = numeric_view(0, LIVE_VALUES + 5, instructions);
        let header_pc = u32::from(LIVE_VALUES + 3);
        let add_imm_pc = header_pc + u32::from(LIVE_VALUES) * 2 + 2;
        for pc in header_pc..=add_imm_pc {
            if pc == header_pc || (pc - header_pc >= 2 && (pc - header_pc).is_multiple_of(2)) {
                view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
            }
        }
        view
    }

    fn branch_phi_loop_view_with(
        initial_checksum: i32,
        initial_index: i32,
        limit: i32,
        increment: i32,
    ) -> JitCompileSnapshot {
        let mut view = numeric_view(
            0,
            12,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(0), Operand::Imm32(initial_checksum)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(3), Operand::Imm32(initial_index)],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(3), Operand::Imm32(1)],
                ),
                (
                    Op::LessThanImm,
                    vec![
                        Operand::Register(4),
                        Operand::Register(1),
                        Operand::Imm32(limit),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(13), Operand::Register(4)],
                ),
                (
                    Op::BitwiseAndImm,
                    vec![
                        Operand::Register(5),
                        Operand::Register(1),
                        Operand::Imm32(1),
                    ],
                ),
                (
                    Op::EqualImm,
                    vec![
                        Operand::Register(6),
                        Operand::Register(5),
                        Operand::Imm32(0),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(3), Operand::Register(6)],
                ),
                (Op::LoadInt32, vec![Operand::Register(7), Operand::Imm32(2)]),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(7), Operand::Imm32(2)],
                ),
                (Op::Jump, vec![Operand::Imm32(2)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(8), Operand::Imm32(-14)],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(8), Operand::Imm32(2)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(9),
                        Operand::Register(0),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(9), Operand::Imm32(0)],
                ),
                (
                    Op::AddImm,
                    vec![
                        Operand::Register(10),
                        Operand::Register(1),
                        Operand::Imm32(increment),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(10), Operand::Imm32(1)],
                ),
                (Op::Jump, vec![Operand::Imm32(-15)]),
                (
                    Op::LoadLocal,
                    vec![Operand::Register(11), Operand::Imm32(0)],
                ),
                (Op::ReturnValue, vec![Operand::Register(11)]),
            ],
        );
        for pc in [3_u32, 5, 6, 13, 15] {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view
    }

    fn compile_output(
        view: &JitCompileSnapshot,
        artifact_request: Option<ArtifactRequest>,
    ) -> NativeCompileOutput<OptimizedCode> {
        let transitions = TransitionTable::resolve();
        compile_output_with_transitions(view, &transitions, artifact_request)
    }

    fn compile_output_with_transitions(
        view: &JitCompileSnapshot,
        transitions: &TransitionTable,
        artifact_request: Option<ArtifactRequest>,
    ) -> NativeCompileOutput<OptimizedCode> {
        try_compile(view, 7001, transitions, artifact_request)
            .expect("numeric Machine IR code generation")
            .expect("eligible numeric function")
    }

    fn execute(code: &OptimizedCode, args: &[u64], initial_pc: u32) -> (JitRet, Vec<u64>, u32) {
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let result = execute_with_poll_cells(
            code,
            args,
            initial_pc,
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        let mut original_frame =
            vec![Value::undefined().to_bits(); code.metadata().register_count as usize];
        original_frame[..args.len()].copy_from_slice(args);
        assert_eq!(
            result.1, original_frame,
            "successful numeric function must not mutate VM slots"
        );
        result
    }

    fn execute_with_poll_cells(
        code: &OptimizedCode,
        args: &[u64],
        initial_pc: u32,
        interrupt: *const u8,
        fuel: &mut u64,
    ) -> (JitRet, Vec<u64>, u32) {
        assert!(args.len() <= code.metadata().register_count as usize);
        let entry: JitEntry = unsafe { std::mem::transmute(code.compiled_code().entry_ptr()) };
        let mut frame = vec![Value::undefined().to_bits(); code.metadata().register_count as usize];
        frame[..args.len()].copy_from_slice(args);
        execute_at(code, entry, frame, initial_pc, interrupt, fuel)
    }

    fn execute_osr_with_poll_cells(
        code: &OptimizedCode,
        logical_pc: u32,
        frame: Vec<u64>,
        interrupt: *const u8,
        fuel: &mut u64,
    ) -> (JitRet, Vec<u64>, u32) {
        // SAFETY: the code object owns the recorded trampoline throughout the call.
        let entry = unsafe {
            code.osr_entry_ptr_for_test(logical_pc)
                .expect("numeric OSR entry")
        };
        // SAFETY: the trampoline uses the same shared `JitEntry` ABI as main entry.
        let entry: JitEntry = unsafe { std::mem::transmute(entry) };
        execute_at(code, entry, frame, logical_pc, interrupt, fuel)
    }

    fn execute_at(
        code: &OptimizedCode,
        entry: JitEntry,
        frame: Vec<u64>,
        initial_pc: u32,
        interrupt: *const u8,
        fuel: &mut u64,
    ) -> (JitRet, Vec<u64>, u32) {
        let register_count = code.metadata().register_count;
        let (result, frame, pc, _) = execute_at_with_register_count(
            code,
            entry,
            frame,
            initial_pc,
            register_count,
            Value::undefined(),
            interrupt,
            fuel,
        );
        (result, frame, pc)
    }

    fn execute_at_with_register_count(
        code: &OptimizedCode,
        entry: JitEntry,
        frame: Vec<u64>,
        initial_pc: u32,
        initialized_register_count: u16,
        this_value: Value,
        interrupt: *const u8,
        fuel: &mut u64,
    ) -> (JitRet, Vec<u64>, u32, u16) {
        let heap = otter_gc::GcHeap::new().expect("execution-test heap");
        execute_at_with_heap(
            code,
            entry,
            frame,
            initial_pc,
            initialized_register_count,
            this_value,
            std::ptr::from_ref(&heap),
            interrupt,
            fuel,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_at_with_heap(
        code: &OptimizedCode,
        entry: JitEntry,
        mut frame: Vec<u64>,
        initial_pc: u32,
        initialized_register_count: u16,
        this_value: Value,
        heap: *const otter_gc::GcHeap,
        interrupt: *const u8,
        fuel: &mut u64,
    ) -> (JitRet, Vec<u64>, u32, u16) {
        assert_eq!(frame.len(), code.metadata().register_count as usize);
        let metadata = code.metadata();
        let mut native_frame = NativeFrame::new(
            VmFrameHeader {
                function_id: metadata.function_id,
                code_block_id: metadata.function_id,
                pc: initial_pc,
                register_count: initialized_register_count,
                kind: NativeFrameKind::Optimizing,
                flags: NativeFrameFlags::empty(),
            },
            frame.as_mut_ptr() as u64,
            Value::undefined(),
            this_value,
        );
        native_frame.set_materialized_activation(0);
        let mut thread = VmThread::empty();
        thread.current_frame = std::ptr::addr_of_mut!(native_frame) as u64;
        thread.current_code_object_id = metadata.code_object_id;
        thread.interrupt_cell = interrupt as u64;
        thread.gc_heap = heap as u64;
        thread.backedge_fuel_cell = std::ptr::from_mut(fuel) as u64;
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
        (
            result,
            frame,
            native_frame.header.pc,
            native_frame.header.register_count,
        )
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
        assert!(code.metadata().parameter_prefix_entry);
        let (ret, _, _) = execute(&code, &[tag::box_int32(9)], 0);

        assert_eq!(ret.status, STATUS_RETURNED);
        assert_eq!(ret.value, tag::box_int32(-9));
    }

    #[test]
    fn tagged_parameters_return_without_numeric_guards_or_boxing() {
        let view = tagged_identity_view();
        let hir = NumericFunction::build(&view).expect("tagged identity HIR");
        assert!(matches!(
            hir.nodes[0],
            NumericNode::Parameter {
                register: 0,
                value_type: NumericType::Tagged,
            }
        ));

        let sequence = select(&hir).expect("tagged identity Machine IR");
        assert_eq!(
            sequence
                .instructions()
                .iter()
                .map(|instruction| &instruction.opcode)
                .collect::<Vec<_>>(),
            [&MachineOpcode::EntryValue(0), &MachineOpcode::Return]
        );
        assert_eq!(sequence.representations(), &[MachineRepresentation::Tagged]);

        let code = compile_output(&view, None).code;
        for value in [Value::undefined(), Value::null(), Value::boolean(true)] {
            let (result, _, _) = execute(&code, &[value.to_bits()], 0);
            assert_eq!(result.status, STATUS_RETURNED);
            assert_eq!(result.value, value.to_bits());
        }
    }

    #[test]
    fn tagged_constants_this_and_bare_return_preserve_exact_value_bits() {
        for (op, expected) in [
            (Op::LoadUndefined, Value::undefined()),
            (Op::LoadNull, Value::null()),
        ] {
            let code = compile_output(&tagged_immediate_view(op), None).code;
            let (result, _, _) = execute(&code, &[], 0);
            assert_eq!(result.status, STATUS_RETURNED);
            assert_eq!(result.value, expected.to_bits());
        }

        let bare_return = numeric_view(0, 0, vec![(Op::ReturnUndefined, vec![])]);
        let code = compile_output(&bare_return, None).code;
        let (result, _, _) = execute(&code, &[], 0);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, Value::undefined().to_bits());

        let code = compile_output(&tagged_this_view(), None).code;
        let entry: JitEntry = unsafe { std::mem::transmute(code.compiled_code().entry_ptr()) };
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let (result, _, _, _) = execute_at_with_register_count(
            &code,
            entry,
            vec![Value::undefined().to_bits()],
            0,
            code.metadata().register_count,
            Value::null(),
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, Value::null().to_bits());
    }

    #[test]
    fn tagged_branch_phi_executes_both_edges_without_reencoding_values() {
        let view = tagged_phi_view();
        let hir = NumericFunction::build(&view).expect("tagged branch-phi HIR");
        assert!(hir.blocks.iter().any(|block| {
            block.predecessors.len() == 2
                && block
                    .parameters
                    .iter()
                    .any(|parameter| hir.nodes[parameter.0].value_type() == NumericType::Tagged)
        }));
        let sequence = select(&hir).expect("tagged branch-phi Machine IR");
        assert!(sequence.blocks().iter().any(|block| {
            block.predecessors.len() == 2
                && block.parameters.iter().any(|parameter| {
                    sequence.representations()[parameter.0 as usize]
                        == MachineRepresentation::Tagged
                })
        }));

        let code = compile_output(&view, None).code;
        let (left, _, _) = execute(
            &code,
            &[
                Value::null().to_bits(),
                Value::undefined().to_bits(),
                tag::box_int32(1),
                tag::box_int32(3),
            ],
            0,
        );
        assert_eq!(left.status, STATUS_RETURNED);
        assert_eq!(left.value, Value::null().to_bits());

        let (right, _, _) = execute(
            &code,
            &[
                Value::null().to_bits(),
                Value::undefined().to_bits(),
                tag::box_int32(3),
                tag::box_int32(1),
            ],
            0,
        );
        assert_eq!(right.status, STATUS_RETURNED);
        assert_eq!(right.value, Value::undefined().to_bits());
    }

    #[test]
    fn tagged_values_survive_loop_phis_osr_and_backedge_deopt() {
        let view = tagged_loop_view();
        let hir = NumericFunction::build(&view).expect("tagged loop HIR");
        let sequence = select(&hir).expect("tagged loop Machine IR");
        let osr_inputs = sequence
            .instructions()
            .iter()
            .find_map(|instruction| match &instruction.opcode {
                MachineOpcode::OsrEntry {
                    logical_pc: 2,
                    inputs,
                } => Some(inputs.as_slice()),
                _ => None,
            })
            .expect("tagged loop OSR marker");
        assert!(osr_inputs.contains(&MachineOsrInput {
            frame_register: 0,
            value_type: MachineOsrType::Tagged,
        }));

        let code = compile_output(&view, None).code;
        let (normal, _, _) = execute(&code, &[Value::null().to_bits(), tag::box_int32(4)], 0);
        assert_eq!(normal.status, STATUS_RETURNED);
        assert_eq!(normal.value, Value::null().to_bits());

        let mut frame = vec![Value::undefined().to_bits(); 5];
        frame[0] = Value::boolean(true).to_bits();
        frame[1] = tag::box_int32(4);
        frame[2] = tag::box_int32(0);
        frame[3] = tag::box_int32(1);
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let (osr, after, _) = execute_osr_with_poll_cells(
            &code,
            2,
            frame.clone(),
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(osr.status, STATUS_RETURNED);
        assert_eq!(osr.value, Value::boolean(true).to_bits());
        assert_eq!(after, frame);

        let interrupt = 1_u8;
        let mut fuel = i64::MAX as u64;
        let (bail, after, pc) =
            execute_osr_with_poll_cells(&code, 2, frame, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(bail.status, STATUS_BAILED);
        assert_eq!(pc, 2);
        assert_eq!(after[0], Value::boolean(true).to_bits());
    }

    #[test]
    fn tagged_truthiness_uses_the_verified_leaf_call_descriptor() {
        let view = tagged_truthiness_branch_view();
        let hir = NumericFunction::build(&view).expect("tagged truthiness HIR");
        assert_eq!(hir.frame_states.len(), 1);
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::TaggedToBoolean(_)))
        );

        let sequence = select(&hir).expect("tagged truthiness Machine IR");
        assert_eq!(sequence.call_descriptors().len(), 1);
        let descriptor = &sequence.call_descriptors()[0];
        assert_eq!(
            descriptor.target,
            CallTarget::RuntimeStub(otter_vm::native_abi::STUB_TO_BOOLEAN_LEAF)
        );
        assert_eq!(
            descriptor.arguments,
            [MachineRepresentation::Tagged, MachineRepresentation::Tagged]
        );
        assert_eq!(descriptor.result, Some(MachineRepresentation::Int32));
        assert_eq!(descriptor.effects, CallEffects::READS_HEAP);
        assert_eq!(descriptor.exceptional, ExceptionalEdge::None);
        assert_eq!(descriptor.safepoint, SafepointKind::None);

        let (call_id, call) = sequence
            .instructions()
            .iter()
            .enumerate()
            .find(|(_, instruction)| matches!(instruction.opcode, MachineOpcode::Call(0)))
            .map(|(index, instruction)| (MachineInstructionId(index as u32), instruction))
            .expect("tagged truthiness call");
        assert_eq!(
            call.operands[0].constraint,
            OperandConstraint::Fixed(PhysicalRegister::integer(1))
        );
        assert_eq!(
            call.operands[2].constraint,
            OperandConstraint::Fixed(PhysicalRegister::integer(0))
        );
        assert!(call.deopt.is_some());
        assert_eq!(
            call.operands
                .iter()
                .filter(|operand| operand.purpose == OperandPurpose::Deopt)
                .count(),
            3
        );
        let allocation = sequence
            .allocate(&TargetRegisterFile::aarch64_scalar_function())
            .expect("tagged truthiness allocation");
        let locations = allocation
            .instruction_locations(call_id)
            .expect("tagged truthiness locations");
        assert_eq!(
            locations[0],
            AllocatedLocation::Register(PhysicalRegister::integer(1))
        );
        assert_eq!(
            locations[2],
            AllocatedLocation::Register(PhysicalRegister::integer(0))
        );
    }

    #[test]
    fn tagged_truthiness_executes_all_immediate_classes_and_exact_miss_deopt() {
        let code = compile_output(&tagged_truthiness_branch_view(), None).code;
        let selected = Value::null().to_bits();
        let rejected = Value::undefined().to_bits();
        for condition in [
            Value::boolean(true).to_bits(),
            tag::box_int32(1),
            boxed_f64(2.5),
        ] {
            let (result, _, _) = execute(&code, &[condition, selected, rejected], 0);
            assert_eq!(result.status, STATUS_RETURNED);
            assert_eq!(result.value, selected);
        }
        for condition in [
            Value::boolean(false).to_bits(),
            Value::null().to_bits(),
            Value::undefined().to_bits(),
            tag::box_int32(0),
            boxed_f64(-0.0),
            boxed_f64(f64::NAN),
        ] {
            let (result, _, _) = execute(&code, &[condition, selected, rejected], 0);
            assert_eq!(result.status, STATUS_RETURNED);
            assert_eq!(result.value, rejected);
        }

        let logical_not = compile_output(&tagged_logical_not_view(), None).code;
        for (condition, expected) in [
            (Value::null().to_bits(), true),
            (Value::boolean(false).to_bits(), true),
            (tag::box_int32(7), false),
        ] {
            let (result, _, _) = execute(&logical_not, &[condition], 0);
            assert_eq!(result.status, STATUS_RETURNED);
            assert_eq!(result.value, Value::boolean(expected).to_bits());
        }

        let entry: JitEntry = unsafe { std::mem::transmute(code.compiled_code().entry_ptr()) };
        let frame = vec![Value::boolean(true).to_bits(), selected, rejected];
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let (result, after, pc, register_count) = execute_at_with_heap(
            &code,
            entry,
            frame.clone(),
            91,
            code.metadata().param_count,
            Value::undefined(),
            std::ptr::null(),
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(pc, 0);
        assert_eq!(register_count, code.metadata().register_count);
        assert_eq!(after, frame);
    }

    #[test]
    fn tagged_strict_equality_uses_verified_leaf_call_and_boxes_scalars() {
        let view = tagged_mixed_strict_equality_view();
        let hir = NumericFunction::build(&view).expect("tagged strict equality HIR");
        assert_eq!(hir.frame_states.len(), 1);
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::TaggedStrictEqual(..)))
        );

        let sequence = select(&hir).expect("tagged strict equality Machine IR");
        assert_eq!(sequence.call_descriptors().len(), 1);
        let descriptor = &sequence.call_descriptors()[0];
        assert_eq!(
            descriptor.target,
            CallTarget::RuntimeStub(otter_vm::native_abi::STUB_STRICT_EQ_LEAF)
        );
        assert_eq!(
            descriptor.arguments,
            [MachineRepresentation::Tagged, MachineRepresentation::Tagged]
        );
        assert_eq!(descriptor.result, Some(MachineRepresentation::Int32));
        assert_eq!(descriptor.effects, CallEffects::READS_HEAP);
        assert_eq!(descriptor.exceptional, ExceptionalEdge::None);
        assert_eq!(descriptor.safepoint, SafepointKind::None);
        assert!(
            sequence
                .instructions()
                .iter()
                .any(|instruction| instruction.opcode == MachineOpcode::BoxInt32)
        );

        let (call_id, call) = sequence
            .instructions()
            .iter()
            .enumerate()
            .find(|(_, instruction)| matches!(instruction.opcode, MachineOpcode::Call(0)))
            .map(|(index, instruction)| (MachineInstructionId(index as u32), instruction))
            .expect("tagged strict equality call");
        for (operand, register) in call.operands[..3]
            .iter()
            .zip([1_u8, 2, 0].map(PhysicalRegister::integer))
        {
            assert_eq!(operand.constraint, OperandConstraint::Fixed(register));
        }
        assert!(call.deopt.is_some());
        let allocation = sequence
            .allocate(&TargetRegisterFile::aarch64_scalar_function())
            .expect("tagged strict equality allocation");
        let locations = allocation
            .instruction_locations(call_id)
            .expect("tagged strict equality locations");
        for (&location, register) in locations[..3]
            .iter()
            .zip([1_u8, 2, 0].map(PhysicalRegister::integer))
        {
            assert_eq!(location, AllocatedLocation::Register(register));
        }
    }

    #[test]
    fn tagged_strict_equality_executes_full_number_semantics_and_exact_miss_deopt() {
        let code = compile_output(&tagged_strict_equality_view(Op::Equal), None).code;
        for (left, right, expected) in [
            (Value::null().to_bits(), Value::null().to_bits(), true),
            (
                Value::undefined().to_bits(),
                Value::undefined().to_bits(),
                true,
            ),
            (
                Value::boolean(true).to_bits(),
                Value::boolean(true).to_bits(),
                true,
            ),
            (tag::box_int32(7), tag::box_int32(7), true),
            (tag::box_int32(7), boxed_f64(7.0), true),
            (boxed_f64(0.0), boxed_f64(-0.0), true),
            (boxed_f64(f64::NAN), boxed_f64(f64::NAN), false),
            (Value::null().to_bits(), Value::undefined().to_bits(), false),
            (Value::boolean(true).to_bits(), tag::box_int32(1), false),
        ] {
            let (result, _, _) = execute(&code, &[left, right], 0);
            assert_eq!(result.status, STATUS_RETURNED);
            assert_eq!(result.value, Value::boolean(expected).to_bits());
        }

        let not_equal = compile_output(&tagged_strict_equality_view(Op::NotEqual), None).code;
        let (result, _, _) = execute(&not_equal, &[tag::box_int32(7), boxed_f64(8.0)], 0);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, Value::boolean(true).to_bits());

        let mixed = compile_output(&tagged_mixed_strict_equality_view(), None).code;
        let (result, _, _) = execute(&mixed, &[boxed_f64(7.0)], 0);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, Value::boolean(true).to_bits());

        let entry: JitEntry = unsafe { std::mem::transmute(code.compiled_code().entry_ptr()) };
        let frame = vec![
            tag::box_int32(3),
            tag::box_int32(4),
            Value::undefined().to_bits(),
        ];
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let (result, after, pc, register_count) = execute_at_with_heap(
            &code,
            entry,
            frame.clone(),
            17,
            code.metadata().param_count,
            Value::undefined(),
            std::ptr::null(),
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(pc, 0);
        assert_eq!(register_count, code.metadata().register_count);
        assert_eq!(after, frame);
    }

    #[test]
    fn tagged_string_concat_uses_allocator_driven_vm_safepoints() {
        let view = tagged_string_concat_view(3);
        let hir = NumericFunction::build(&view).expect("tagged string-concat HIR");
        assert_eq!(
            hir.nodes
                .iter()
                .filter(|node| matches!(node, NumericNode::TaggedStringConcat(..)))
                .count(),
            2
        );
        assert_eq!(hir.frame_states.len(), 2);

        let sequence = select(&hir).expect("tagged string-concat Machine IR");
        assert_eq!(sequence.call_descriptors().len(), 1);
        let descriptor = &sequence.call_descriptors()[0];
        assert_eq!(
            descriptor.target,
            CallTarget::RuntimeStub(otter_vm::native_abi::STUB_STRING_CONCAT_ALLOC)
        );
        assert_eq!(descriptor.arguments, [MachineRepresentation::Tagged; 3]);
        assert_eq!(descriptor.result, Some(MachineRepresentation::Tagged));
        assert_eq!(descriptor.safepoint, SafepointKind::Gc);

        let calls = sequence
            .instructions()
            .iter()
            .enumerate()
            .filter(|(_, instruction)| matches!(instruction.opcode, MachineOpcode::Call(0)))
            .map(|(index, instruction)| (MachineInstructionId(index as u32), instruction))
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1.safepoint, Some(SafepointId(0)));
        assert_eq!(calls[1].1.safepoint, Some(SafepointId(1)));
        assert!(calls.iter().all(|(_, call)| call.deopt.is_some()));
        assert!(calls.iter().all(|(_, call)| {
            call.operands
                .iter()
                .any(|operand| operand.purpose == OperandPurpose::TaggedRoot)
        }));

        let allocation = sequence
            .allocate(&TargetRegisterFile::aarch64_scalar_function())
            .expect("tagged string-concat allocation");
        let safepoints =
            lower_safepoints(&sequence, &allocation).expect("allocator-driven tagged safepoints");
        assert_eq!(safepoints.records().len(), 2);
        assert_eq!(safepoints.records()[0].id, 0);
        assert_eq!(safepoints.records()[0].frame_state, 0);
        assert_eq!(safepoints.records()[1].id, 1);
        assert_eq!(safepoints.records()[1].frame_state, 1);
        assert!(safepoints.records().iter().all(|record| {
            !record.tagged_locations.is_empty()
                && record.tagged_locations.iter().all(|location| {
                    location.kind == otter_vm::native_abi::TaggedLocationKind::SpillSlot
                })
        }));

        let code = compile_output(&view, None).code;
        assert!(!code.metadata().parameter_prefix_entry);
        assert_eq!(JitFunctionCode::safepoint_count(&code), 2);
        assert_eq!(code.deopt_table().entries()[0].outermost().byte_pc, 0);
        assert_eq!(code.deopt_table().entries()[1].outermost().byte_pc, 8);
    }

    #[test]
    fn tagged_string_concat_forces_roots_into_allocator_spills() {
        let hir = NumericFunction::build(&tagged_string_concat_view(16))
            .expect("pressure string-concat HIR");
        let sequence = select(&hir).expect("pressure string-concat Machine IR");
        let allocation = sequence
            .allocate(&TargetRegisterFile::aarch64_scalar_function())
            .expect("pressure string-concat allocation");
        let safepoints =
            lower_safepoints(&sequence, &allocation).expect("pressure allocator-driven safepoints");
        let first_call = sequence
            .instructions()
            .iter()
            .position(|instruction| matches!(instruction.opcode, MachineOpcode::Call(0)))
            .map(|index| MachineInstructionId(index as u32))
            .expect("first pressure concat call");
        let first_site = safepoints
            .site(first_call)
            .expect("first pressure safepoint");
        assert!(first_site.roots.len() > 9);
        assert!(
            first_site
                .roots
                .iter()
                .any(|root| matches!(root.source, AllocatedLocation::Stack(_))),
            "callee-saved GPR pressure must force at least one GC root to a spill"
        );
        let frame = arm64::frame_layout(&allocation, safepoints.root_slot_count())
            .expect("pressure root-save frame");
        assert_eq!(frame.root_slots(), safepoints.root_slot_count());
        assert!(frame.root_offset(0).expect("first root offset") >= allocation.spill_slots() * 8);
    }

    #[test]
    fn feedback_specializes_numeric_parameters_until_a_float64_boundary() {
        let view = typed_parameter_leaf_view();
        let hir = NumericFunction::build(&view).expect("typed parameter numeric HIR");
        assert!(matches!(
            hir.nodes[0],
            NumericNode::Parameter {
                register: 0,
                value_type: NumericType::Int32
            }
        ));
        assert!(matches!(
            hir.nodes[1],
            NumericNode::Parameter {
                register: 1,
                value_type: NumericType::Int32
            }
        ));
        assert_eq!(
            hir.nodes
                .iter()
                .filter(|node| matches!(node, NumericNode::WidenInt32(..)))
                .count(),
            2
        );

        let sequence = select(&hir).expect("typed parameter Machine IR");
        assert_eq!(
            sequence
                .instructions()
                .iter()
                .filter(|instruction| instruction.opcode == MachineOpcode::DecodeInt32)
                .count(),
            2
        );
        assert!(sequence.instructions().iter().all(|instruction| {
            instruction.opcode != MachineOpcode::DecodeInt32
                || instruction.operands[1].constraint == OperandConstraint::Reuse(0)
        }));
        assert_eq!(
            sequence
                .instructions()
                .iter()
                .filter(|instruction| {
                    matches!(
                        instruction.opcode,
                        MachineOpcode::IntegerAdd
                            | MachineOpcode::IntegerSub
                            | MachineOpcode::IntegerMul
                    )
                })
                .count(),
            6
        );

        let code = compile_output(&view, None).code;
        let (result, _, _) = execute(&code, &[tag::box_int32(2), tag::box_int32(2)], 0);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, tag::box_int32(-7));

        let (bail, frame, pc) = execute(&code, &[boxed_f64(2.5), tag::box_int32(2)], 77);
        assert_eq!(bail.status, STATUS_BAILED);
        assert_eq!(pc, 0);
        assert_eq!(frame[0], boxed_f64(2.5));
        assert_eq!(frame[1], tag::box_int32(2));
    }

    #[test]
    fn parameter_inference_tracks_copy_aliases_without_specializing_bitwise_coercions() {
        let alias = NumericFunction::build(&typed_parameter_alias_view())
            .expect("copy-alias typed parameter HIR");
        assert!(matches!(
            alias.nodes[0],
            NumericNode::Parameter {
                value_type: NumericType::Int32,
                ..
            }
        ));
        let (result, _, _) = execute(
            &compile_output(&typed_parameter_alias_view(), None).code,
            &[tag::box_int32(41)],
            0,
        );
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, tag::box_int32(42));

        let bitwise = NumericFunction::build(&float_bitwise_view(Op::BitwiseAnd))
            .expect("bitwise numeric HIR");
        assert!(matches!(
            bitwise.nodes[0],
            NumericNode::Parameter {
                value_type: NumericType::Number,
                ..
            }
        ));
        assert!(matches!(
            bitwise.nodes[1],
            NumericNode::Parameter {
                value_type: NumericType::Number,
                ..
            }
        ));
    }

    #[test]
    fn typed_parameter_overflow_reconstructs_the_exact_entry_frame() {
        let code = compile_output(&typed_parameter_overflow_view(), None).code;
        let (result, frame, pc) =
            execute(&code, &[tag::box_int32(i32::MAX), tag::box_int32(1)], 91);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(pc, 0);
        assert_eq!(
            frame,
            [
                tag::box_int32(i32::MAX),
                tag::box_int32(1),
                Value::undefined().to_bits()
            ]
        );
    }

    #[test]
    fn parameter_prefix_cold_exits_publish_a_complete_vm_window() {
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;

        let guard = compile_output(&small_leaf_view(), None).code;
        let guard_entry: JitEntry =
            unsafe { std::mem::transmute(guard.compiled_code().entry_ptr()) };
        let (result, frame, _, register_count) = execute_at_with_register_count(
            &guard,
            guard_entry,
            vec![tag::box_int32(9), 0xdead_beef_dead_beef],
            91,
            guard.metadata().param_count,
            Value::undefined(),
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, tag::box_int32(-9));
        assert_eq!(register_count, guard.metadata().param_count);
        assert_eq!(frame[1], 0xdead_beef_dead_beef);

        let (result, frame, pc, register_count) = execute_at_with_register_count(
            &guard,
            guard_entry,
            vec![Value::undefined().to_bits(), 0xdead_beef_dead_beef],
            91,
            guard.metadata().param_count,
            Value::undefined(),
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(pc, 0);
        assert_eq!(register_count, guard.metadata().register_count);
        assert_eq!(frame[1], Value::undefined().to_bits());

        let overflow = compile_output(&typed_parameter_overflow_view(), None).code;
        let overflow_entry: JitEntry =
            unsafe { std::mem::transmute(overflow.compiled_code().entry_ptr()) };
        let (result, frame, pc, register_count) = execute_at_with_register_count(
            &overflow,
            overflow_entry,
            vec![
                tag::box_int32(i32::MAX),
                tag::box_int32(1),
                0xdead_beef_dead_beef,
            ],
            91,
            overflow.metadata().param_count,
            Value::undefined(),
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(pc, 0);
        assert_eq!(register_count, overflow.metadata().register_count);
        assert_eq!(
            frame,
            [
                tag::box_int32(i32::MAX),
                tag::box_int32(1),
                Value::undefined().to_bits()
            ]
        );
    }

    #[test]
    fn dead_parameters_have_no_entry_load_guard_or_allocator_value() {
        let view = unused_parameter_view();
        let hir = NumericFunction::build(&view).expect("live-only parameter HIR");
        assert_eq!(
            hir.nodes
                .iter()
                .filter_map(|node| match node {
                    NumericNode::Parameter { register, .. } => Some(*register),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [1]
        );
        let sequence = select(&hir).expect("live-only parameter Machine IR");
        assert_eq!(
            sequence
                .instructions()
                .iter()
                .filter_map(|instruction| match instruction.opcode {
                    MachineOpcode::EntryValue(parameter) => Some(parameter),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [1]
        );

        let code = compile_output(&view, None).code;
        let (result, _, _) = execute(&code, &[Value::undefined().to_bits(), tag::box_int32(9)], 0);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, tag::box_int32(-9));
    }

    #[test]
    fn typed_parameters_publish_a_complete_integer_loop() {
        let view = typed_parameter_loop_view();
        let hir = NumericFunction::build(&view).expect("typed parameter loop HIR");
        assert!(hir.nodes[..2].iter().all(|node| matches!(
            node,
            NumericNode::Parameter {
                value_type: NumericType::Int32,
                ..
            }
        )));
        let sequence = select(&hir).expect("typed parameter loop Machine IR");
        sequence
            .allocate(&TargetRegisterFile::aarch64_scalar_function())
            .expect("typed parameter loop allocation");

        let code = compile_output(&view, None).code;
        let (result, _, _) = execute(&code, &[tag::box_int32(10), tag::box_int32(3)], 0);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, tag::box_int32(30));
        // SAFETY: the code object remains alive for the pointer lookup.
        assert!(unsafe { code.osr_entry_ptr_for_test(2) }.is_some());
    }

    #[test]
    fn inconsistent_loop_backedge_representations_decline_before_selection() {
        let mut view = typed_parameter_loop_view();
        view.seed_arith_feedback_for_test(4, ArithFeedback::from_bits(ARITH_INT32 | ARITH_FLOAT64));
        assert!(NumericFunction::build(&view).is_none());
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
    fn executes_numeric_diamond_with_allocator_block_parameter() {
        let view = diamond_view(Op::JumpIfFalse);
        let hir = NumericFunction::build(&view).expect("diamond numeric HIR");
        let sequence = select(&hir).expect("diamond Machine IR");
        let merge = sequence
            .blocks()
            .iter()
            .find(|block| block.predecessors.len() == 2)
            .expect("diamond merge block");
        assert_eq!(merge.parameters.len(), 1);
        assert!(
            sequence
                .blocks()
                .iter()
                .filter(|block| block.successors.contains(&MachineBlock(3)))
                .all(|block| block.successor_arguments[0].len() == 1)
        );

        let code = compile_output(&view, None).code;

        let (taken, _, _) = execute(&code, &[tag::box_int32(1), tag::box_int32(3)], 0);
        assert_eq!(taken.status, STATUS_RETURNED);
        assert_eq!(taken.value, tag::box_int32(4));

        let (fallthrough, _, _) = execute(&code, &[tag::box_int32(3), tag::box_int32(1)], 0);
        assert_eq!(fallthrough.status, STATUS_RETURNED);
        assert_eq!(fallthrough.value, tag::box_int32(2));

        let (unordered, _, _) = execute(&code, &[boxed_f64(f64::NAN), tag::box_int32(1)], 0);
        assert_eq!(unordered.status, STATUS_RETURNED);
        assert!(unbox_number(unordered.value).is_nan());

        let (bail, frame, pc) = execute(
            &code,
            &[tag::box_int32(1), Value::undefined().to_bits()],
            19,
        );
        assert_eq!(bail.status, STATUS_BAILED);
        assert_eq!(pc, 0);
        assert_eq!(frame[1], Value::undefined().to_bits());

        let branch_true = compile_output(&diamond_view(Op::JumpIfTrue), None).code;
        let (true_edge, _, _) = execute(&branch_true, &[tag::box_int32(1), tag::box_int32(3)], 0);
        assert_eq!(true_edge.value, tag::box_int32(-2));
        let (false_edge, _, _) = execute(&branch_true, &[tag::box_int32(3), tag::box_int32(1)], 0);
        assert_eq!(false_edge.value, tag::box_int32(4));
    }

    #[test]
    fn splits_and_executes_critical_edge_with_block_parameter() {
        let view = critical_edge_view();
        let hir = NumericFunction::build(&view).expect("critical-edge numeric HIR");
        let sequence = select(&hir).expect("split critical-edge Machine IR");
        assert_eq!(sequence.blocks().len(), hir.blocks.len() + 1);
        let split = sequence
            .blocks()
            .iter()
            .find(|block| {
                block.predecessors.len() == 1
                    && block.successors.len() == 1
                    && block.parameters.is_empty()
                    && block.successor_arguments[0].len() == 1
            })
            .expect("critical edge split block");
        assert!(
            sequence.blocks()[split.successors[0].0 as usize]
                .parameters
                .len()
                == 1
        );

        let code = compile_output(&view, None).code;
        let (direct_edge, _, _) = execute(&code, &[tag::box_int32(3), tag::box_int32(1)], 0);
        assert_eq!(direct_edge.status, STATUS_RETURNED);
        assert_eq!(direct_edge.value, tag::box_int32(3));

        let (arm_edge, _, _) = execute(&code, &[tag::box_int32(1), tag::box_int32(3)], 0);
        assert_eq!(arm_edge.status, STATUS_RETURNED);
        assert_eq!(arm_edge.value, tag::box_int32(4));
    }

    #[test]
    fn executes_loop_header_parameters_through_native_publication() {
        let view = loop_view();
        let hir = NumericFunction::build(&view).expect("loop numeric HIR");
        assert!(
            hir.frame_states
                .iter()
                .any(|state| matches!(state.point, NumericFramePoint::Backedge { .. }))
        );
        let sequence = select(&hir).expect("loop Machine IR");
        let (header_index, header) = sequence
            .blocks()
            .iter()
            .enumerate()
            .find(|(_, block)| block.predecessors.len() == 2 && !block.parameters.is_empty())
            .expect("loop header block parameters");
        assert_eq!(header.parameters.len(), 2);
        for &predecessor in &header.predecessors {
            let predecessor = &sequence.blocks()[predecessor.0 as usize];
            let edge = predecessor
                .successors
                .iter()
                .position(|successor| successor.0 as usize == header_index)
                .expect("incoming loop edge");
            assert_eq!(predecessor.successor_arguments[edge].len(), 2);
        }
        sequence
            .allocate(&TargetRegisterFile::aarch64_scalar_function())
            .expect("loop Machine IR allocation");

        let code = compile_output(&view, None).code;
        assert!(!code.metadata().parameter_prefix_entry);
        for (input, expected) in [(-2, 1), (0, 1), (1, 1), (3, 3)] {
            let (result, _, _) = execute(&code, &[tag::box_int32(input)], 0);
            assert_eq!(result.status, STATUS_RETURNED);
            assert_eq!(result.value, tag::box_int32(expected));
        }
    }

    #[test]
    fn executes_branch_phi_integer_loop_through_machine_ir_backend() {
        let view = branch_phi_loop_view();
        let hir = NumericFunction::build(&view).expect("branch-phi numeric HIR");
        assert_eq!(hir.frame_states.len(), 3);
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerAndImmediate(_, 1)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerAddImmediate(_, 1)))
        );
        let sequence = select(&hir).expect("branch-phi Machine IR");
        let (osr_logical_pc, osr_inputs, osr_operands) = sequence
            .instructions()
            .iter()
            .find_map(|instruction| match &instruction.opcode {
                MachineOpcode::OsrEntry { logical_pc, inputs } => Some((
                    *logical_pc,
                    inputs.as_slice(),
                    instruction.operands.as_slice(),
                )),
                _ => None,
            })
            .expect("branch-phi OSR marker");
        assert_eq!(osr_logical_pc, 3);
        assert_eq!(
            osr_inputs,
            [
                MachineOsrInput {
                    frame_register: 0,
                    value_type: MachineOsrType::Int32,
                },
                MachineOsrInput {
                    frame_register: 1,
                    value_type: MachineOsrType::Int32,
                },
            ]
        );
        assert_eq!(osr_operands.len(), osr_inputs.len());
        assert!(osr_operands.iter().all(|operand| {
            operand.constraint == OperandConstraint::Any && operand.timing == OperandTiming::Late
        }));
        let polls = sequence
            .blocks()
            .iter()
            .filter_map(|block| {
                let instructions =
                    &sequence.instructions()[block.first.0 as usize..block.end.0 as usize];
                instructions
                    .iter()
                    .any(|instruction| instruction.opcode == MachineOpcode::BackedgePoll)
                    .then_some((block, instructions))
            })
            .collect::<Vec<_>>();
        assert_eq!(polls.len(), 1);
        let (poll_block, poll_instructions) = polls[0];
        assert_eq!(poll_block.predecessors.len(), 1);
        assert_eq!(poll_block.successors.len(), 1);
        assert!(poll_block.parameters.is_empty());
        assert!(matches!(
            poll_instructions
                .last()
                .map(|instruction| &instruction.opcode),
            Some(MachineOpcode::Jump)
        ));
        let poll = poll_instructions
            .iter()
            .find(|instruction| instruction.opcode == MachineOpcode::BackedgePoll)
            .expect("backedge poll instruction");
        assert_eq!(poll.deopt, Some(DeoptId(2)));
        assert_eq!(poll.operands.len(), 2);
        assert!(sequence.blocks().iter().any(|block| {
            block.parameters.len() >= 2
                && block.parameters.iter().all(|parameter| {
                    sequence.representations()[parameter.0 as usize] == MachineRepresentation::Int32
                })
        }));
        let allocation = sequence
            .allocate(&TargetRegisterFile::aarch64_scalar_function())
            .expect("branch-phi Machine IR allocation");
        assert!(
            allocation
                .metadata()
                .iter()
                .filter(|metadata| metadata.deopt == Some(DeoptId(2)))
                .all(|metadata| match metadata.location {
                    AllocatedLocation::Stack(_) => true,
                    AllocatedLocation::Register(register) => {
                        register.is_integer() && (20..=28).contains(&register.encoding())
                    }
                }),
            "poll operands must survive the leaf call outside caller-saved registers"
        );
        assert!(allocation.metadata().iter().any(|metadata| {
            metadata.deopt == Some(DeoptId(2))
                && matches!(metadata.location, AllocatedLocation::Register(register)
                    if register.is_integer() && (20..=28).contains(&register.encoding()))
        }));
        let layout = arm64::frame_layout(&allocation, 0).expect("branch-phi frame layout");
        let deopt_table = lower_deopt_table(
            &sequence,
            &allocation,
            layout,
            arm64::GPR_BUDGET,
            arm64::FP_BUDGET,
            &machine_frame_states(&hir),
        )
        .expect("branch-phi allocator-driven FrameState");
        assert_eq!(deopt_table.len(), 3);
        assert_eq!(deopt_table.entries()[0].outermost().slots.len(), 12);
        assert_eq!(deopt_table.entries()[1].outermost().slots.len(), 12);
        assert_eq!(deopt_table.entries()[2].outermost().slots.len(), 12);
        let live_slot_counts = deopt_table
            .entries()
            .iter()
            .map(|state| {
                state
                    .outermost()
                    .slots
                    .iter()
                    .filter(|slot| {
                        !matches!(slot.location, otter_vm::deopt::DeoptLocation::Literal(_))
                    })
                    .count()
            })
            .collect::<Vec<_>>();
        assert_eq!(live_slot_counts, [3, 2, 2]);

        for (limit, expected) in [(1, 2), (2, -12), (5, -22)] {
            let code = compile_output(&branch_phi_loop_view_with(0, 0, limit, 1), None).code;
            let (result, _, _) = execute(&code, &[], 0);
            assert_eq!(result.status, STATUS_RETURNED);
            assert_eq!(result.value, tag::box_int32(expected));
        }

        let transitions = TransitionTable::resolve();
        let exact = crate::optimizing::compile_optimized_with_artifacts(
            &view,
            7003,
            &transitions,
            Some(ArtifactRequest {
                identity: JitArtifactIdentity {
                    function_name: "engineKernel".to_string(),
                    module: "benchmarks/scripts/branch-phi.js".to_string(),
                },
                tier: JitDebugTier::Optimizing,
                entry: JitDebugTarget::Entry,
            }),
            false,
        )
        .expect("production optimizing selector compiles exact branch-phi");
        let artifact = exact.artifact.expect("exact branch-phi artifact");
        let optimized_ir = std::str::from_utf8(
            artifact
                .file(JitArtifactFileName::OptimizedIr)
                .expect("optimized IR artifact")
                .contents(),
        )
        .expect("UTF-8 optimized IR");
        assert!(optimized_ir.starts_with("; backend=otter-machine-ir scalar-function\n"));
        assert!(optimized_ir.contains("OsrEntry { logical_pc: 3"));
        let code_map = std::str::from_utf8(
            artifact
                .file(JitArtifactFileName::CodeMap)
                .expect("branch-phi code map")
                .contents(),
        )
        .expect("UTF-8 branch-phi code map");
        assert!(code_map.contains("\"logicalPc\": 3"));
        let (result, _, _) = execute(&exact.code, &[], 0);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, tag::box_int32(-6_000_000));
    }

    #[test]
    fn executes_countdown_integer_loop_through_machine_ir_backend() {
        let view = countdown_loop_view();
        let hir = NumericFunction::build(&view).expect("countdown numeric HIR");
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerSub(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerSubImmediate(_, -3)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerAddImmediate(_, 1)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerNotEqualImmediate(_, 0)))
        );
        assert_eq!(hir.frame_states.len(), 4);

        let output = crate::optimizing::compile_optimized_with_artifacts(
            &view,
            7004,
            &TransitionTable::resolve(),
            Some(ArtifactRequest {
                identity: JitArtifactIdentity {
                    function_name: "countdownKernel".to_string(),
                    module: "test:countdown-machine-loop".to_string(),
                },
                tier: JitDebugTier::Optimizing,
                entry: JitDebugTarget::Entry,
            }),
            false,
        )
        .expect("production optimizing selector compiles countdown loop");
        let optimized_ir = std::str::from_utf8(
            output
                .artifact
                .as_ref()
                .expect("countdown artifact")
                .file(JitArtifactFileName::OptimizedIr)
                .expect("countdown optimized IR")
                .contents(),
        )
        .expect("UTF-8 optimized IR");
        assert!(optimized_ir.starts_with("; backend=otter-machine-ir scalar-function\n"));

        let (result, _, _) = execute(&output.code, &[], 0);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, tag::box_int32(15));
    }

    #[test]
    fn executes_register_bitwise_loop_through_machine_ir_backend() {
        let view = bitwise_loop_view();
        let hir = NumericFunction::build(&view).expect("bitwise-loop numeric HIR");
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerAnd(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerOr(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerXor(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerShiftLeft(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerShiftRight(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerNot(..)))
        );

        let sequence = select(&hir).expect("bitwise-loop Machine IR");
        sequence
            .allocate(&TargetRegisterFile::aarch64_scalar_function())
            .expect("bitwise-loop Machine IR allocation");

        let output = crate::optimizing::compile_optimized_with_artifacts(
            &view,
            7005,
            &TransitionTable::resolve(),
            Some(ArtifactRequest {
                identity: JitArtifactIdentity {
                    function_name: "bitwiseKernel".to_string(),
                    module: "benchmarks/scripts/bitwise-mix.js".to_string(),
                },
                tier: JitDebugTier::Optimizing,
                entry: JitDebugTarget::Entry,
            }),
            false,
        )
        .expect("production optimizing selector compiles bitwise loop");
        let optimized_ir = std::str::from_utf8(
            output
                .artifact
                .as_ref()
                .expect("bitwise-loop artifact")
                .file(JitArtifactFileName::OptimizedIr)
                .expect("bitwise-loop optimized IR")
                .contents(),
        )
        .expect("UTF-8 optimized IR");
        assert!(optimized_ir.starts_with("; backend=otter-machine-ir scalar-function\n"));

        let (result, _, _) = execute(&output.code, &[], 0);
        let mut expected = 0x1234_5678_i32;
        for index in 0_i32..35 {
            let left = expected.wrapping_shl(index as u32 & 31);
            let right = expected >> 31;
            expected = !(((left ^ right) | index) & i32::MAX);
        }
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, tag::box_int32(expected));
    }

    #[test]
    fn float64_bitwise_inputs_use_exact_to_int32_semantics() {
        for (value, expected) in [
            (0.0, 0),
            (-0.0, 0),
            (1.9, 1),
            (-1.9, -1),
            (f64::NAN, 0),
            (f64::INFINITY, 0),
            (f64::NEG_INFINITY, 0),
            (2_147_483_648.0, i32::MIN),
            (4_294_967_295.0, -1),
            (4_294_967_297.0, 1),
            (-4_294_967_297.0, -1),
            (9_007_199_254_740_992.0, 0),
        ] {
            let code = compile_output(&float_bitwise_view(Op::BitwiseOr), None).code;
            let (result, _, _) = execute(&code, &[boxed_f64(value), tag::box_int32(0)], 0);
            assert_eq!(result.status, STATUS_RETURNED);
            assert_eq!(result.value, tag::box_int32(expected));
        }

        let code = compile_output(&float_bitwise_view(Op::Ushr), None).code;
        let (result, _, _) = execute(&code, &[boxed_f64(-1.9), tag::box_int32(0)], 0);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(unbox_number(result.value), 4_294_967_295.0);

        let code = compile_output(&float_bitwise_view(Op::Shl), None).code;
        let (result, _, _) = execute(&code, &[boxed_f64(1.9), boxed_f64(33.9)], 0);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, tag::box_int32(2));

        let view = boolean_bitwise_view();
        let hir = NumericFunction::build(&view).expect("Boolean constants numeric HIR");
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::BooleanToInt32(..)))
        );
        let (result, _, _) = execute(&compile_output(&view, None).code, &[], 0);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, tag::box_int32(1));
    }

    #[test]
    fn checked_integer_subtraction_reconstructs_pre_operation_frames() {
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let register = compile_output(&checked_binary_view(Op::Sub, i32::MIN, 1), None).code;
        let (result, frame, pc) =
            execute_with_poll_cells(&register, &[], 0, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(pc, 2);
        assert_eq!(
            frame,
            [
                tag::box_int32(i32::MIN),
                tag::box_int32(1),
                Value::undefined().to_bits()
            ]
        );

        let immediate =
            compile_output(&checked_immediate_view(Op::SubImm, i32::MAX, -1), None).code;
        let (result, frame, pc) =
            execute_with_poll_cells(&immediate, &[], 0, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(pc, 1);
        assert_eq!(
            frame,
            [tag::box_int32(i32::MAX), Value::undefined().to_bits()]
        );

        let increment =
            compile_output(&checked_immediate_view(Op::Increment, i32::MAX, 1), None).code;
        let (result, frame, pc) =
            execute_with_poll_cells(&increment, &[], 0, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(pc, 1);
        assert_eq!(
            frame,
            [tag::box_int32(i32::MAX), Value::undefined().to_bits()]
        );
    }

    #[test]
    fn checked_integer_multiply_deopts_on_overflow_and_negative_zero() {
        let success = compile_output(&checked_binary_view(Op::Mul, 12_345, -17), None).code;
        let (result, _, _) = execute(&success, &[], 0);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, tag::box_int32(-209_865));

        let interrupt = 0_u8;
        for (left, right) in [(i32::MAX, 2), (0, -1)] {
            let code = compile_output(&checked_binary_view(Op::Mul, left, right), None).code;
            let mut fuel = i64::MAX as u64;
            let (result, frame, pc) =
                execute_with_poll_cells(&code, &[], 0, std::ptr::addr_of!(interrupt), &mut fuel);
            assert_eq!(result.status, STATUS_BAILED);
            assert_eq!(pc, 2);
            assert_eq!(
                frame,
                [
                    tag::box_int32(left),
                    tag::box_int32(right),
                    Value::undefined().to_bits()
                ]
            );
        }
    }

    #[test]
    fn checked_integer_negation_deopts_on_overflow_and_negative_zero() {
        let success = compile_output(&checked_neg_view(17), None).code;
        let (result, _, _) = execute(&success, &[], 0);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, tag::box_int32(-17));

        let interrupt = 0_u8;
        for source in [0, i32::MIN] {
            let code = compile_output(&checked_neg_view(source), None).code;
            let mut fuel = i64::MAX as u64;
            let (result, frame, pc) =
                execute_with_poll_cells(&code, &[], 0, std::ptr::addr_of!(interrupt), &mut fuel);
            assert_eq!(result.status, STATUS_BAILED);
            assert_eq!(pc, 1);
            assert_eq!(
                frame,
                [tag::box_int32(source), Value::undefined().to_bits()]
            );
        }
    }

    #[test]
    fn float_leaf_math_and_truthiness_preserve_javascript_edges() {
        let rem_view = float_binary_view(Op::Rem);
        let rem_hir = NumericFunction::build(&rem_view).expect("remainder numeric HIR");
        let rem_sequence = select(&rem_hir).expect("remainder Machine IR");
        rem_sequence
            .allocate(&TargetRegisterFile::aarch64_scalar_function())
            .expect("remainder allocation");

        for (op, left, right, expected) in [
            (Op::Rem, 5.5, 2.0, 1.5),
            (Op::Pow, 2.0, 10.0, 1024.0),
            (Op::Pow, f64::NAN, 0.0, 1.0),
        ] {
            let code = compile_output(&float_binary_view(op), None).code;
            let (result, _, _) = execute(&code, &[boxed_f64(left), boxed_f64(right)], 0);
            assert_eq!(result.status, STATUS_RETURNED);
            assert_eq!(unbox_number(result.value), expected);
        }

        let rem = compile_output(&float_binary_view(Op::Rem), None).code;
        let (result, _, _) = execute(&rem, &[boxed_f64(-4.0), boxed_f64(2.0)], 0);
        assert_eq!(unbox_number(result.value).to_bits(), (-0.0_f64).to_bits());

        let pow = compile_output(&float_binary_view(Op::Pow), None).code;
        let (result, _, _) = execute(&pow, &[boxed_f64(-1.0), boxed_f64(f64::INFINITY)], 0);
        assert!(unbox_number(result.value).is_nan());

        let truthiness = compile_output(&float_truthiness_view(), None).code;
        for (input, expected) in [(0.0, true), (-0.0, true), (f64::NAN, true), (3.5, false)] {
            let (result, _, _) = execute(&truthiness, &[boxed_f64(input)], 0);
            assert_eq!(result.status, STATUS_RETURNED);
            assert_eq!(result.value, Value::boolean(expected).to_bits());
        }

        for (input, expected) in [(0, true), (-1, false)] {
            let code = compile_output(&integer_truthiness_view(input), None).code;
            let (result, _, _) = execute(&code, &[], 0);
            assert_eq!(result.status, STATUS_RETURNED);
            assert_eq!(result.value, Value::boolean(expected).to_bits());
        }
    }

    #[test]
    fn unsigned_shift_boxes_and_deopts_as_uint32() {
        for (left, shift, expected) in [
            (-1, 0, 4_294_967_295_f64),
            (-1, 1, 2_147_483_647_f64),
            (i32::MIN, -1, 1_f64),
        ] {
            let code = compile_output(&ushr_view(left, shift), None).code;
            let (result, _, _) = execute(&code, &[], 0);
            assert_eq!(result.status, STATUS_RETURNED);
            assert_eq!(unbox_number(result.value), expected);
        }

        let comparison = compile_output(&ushr_comparison_view(), None).code;
        let (result, _, _) = execute(&comparison, &[], 0);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, Value::boolean(true).to_bits());

        let view = ushr_backedge_view();
        let hir = NumericFunction::build(&view).expect("uint32-loop numeric HIR");
        let poll_state = hir
            .frame_states
            .iter()
            .find(|state| matches!(state.point, NumericFramePoint::Backedge { .. }))
            .expect("uint32 backedge FrameState");
        let uint_value = match poll_state.slots[0] {
            hir::NumericFrameSlot::Value(value) => value,
            hir::NumericFrameSlot::Undefined => panic!("uint32 loop value is live"),
        };
        assert_eq!(hir.nodes[uint_value.0].value_type(), NumericType::Uint32);

        let code = compile_output(&view, None).code;
        let interrupt = 1_u8;
        let mut fuel = i64::MAX as u64;
        let (result, frame, pc) =
            execute_with_poll_cells(&code, &[], 0, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(pc, 6);
        assert_eq!(unbox_number(frame[0]), 4_294_967_295_f64);
        assert_eq!(frame[1], tag::box_int32(1));
    }

    #[test]
    fn callee_saved_value_survives_leaf_call_and_overflow_deopt() {
        let view = typed_parameter_leaf_overflow_view();
        let hir = NumericFunction::build(&view).expect("typed leaf overflow numeric HIR");
        let sequence = select(&hir).expect("typed leaf overflow Machine IR");
        let allocation = sequence
            .allocate(&TargetRegisterFile::aarch64_scalar_function())
            .expect("typed leaf overflow allocation");
        assert!(allocation.metadata().iter().any(|metadata| {
            metadata.deopt.is_some()
                && matches!(metadata.location, AllocatedLocation::Register(register)
                    if (register.is_integer() && (20..=28).contains(&register.encoding()))
                        || (register.is_float() && (8..=15).contains(&register.encoding())))
        }));

        let code = compile_output(&view, None).code;
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let (result, frame, pc) = execute_with_poll_cells(
            &code,
            &[tag::box_int32(i32::MAX)],
            0,
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(pc, 5);
        assert_eq!(frame[0], tag::box_int32(i32::MAX));
        assert_eq!(frame[5], tag::box_int32(1));
    }

    #[test]
    fn register_comparisons_return_canonical_booleans() {
        for (op, left, right, expected) in [
            (Op::Equal, 7, 7, true),
            (Op::NotEqual, 7, 8, true),
            (Op::LessThan, -1, 0, true),
            (Op::LessEq, 7, 7, true),
            (Op::GreaterThan, 8, 7, true),
            (Op::GreaterEq, 7, 7, true),
        ] {
            let code = compile_output(&integer_comparison_view(op, left, right), None).code;
            let (result, _, _) = execute(&code, &[], 0);
            assert_eq!(result.status, STATUS_RETURNED);
            assert_eq!(result.value, Value::boolean(expected).to_bits());
        }

        for (op, left, right, expected) in [
            (Op::Equal, 3.5, 3.5, true),
            (Op::NotEqual, f64::NAN, f64::NAN, true),
            (Op::LessThan, f64::NAN, 1.0, false),
            (Op::LessEq, f64::NAN, 1.0, false),
            (Op::GreaterThan, f64::NAN, 1.0, false),
            (Op::GreaterEq, f64::NAN, 1.0, false),
        ] {
            let code = compile_output(&float_comparison_view(op), None).code;
            let (result, _, _) = execute(&code, &[boxed_f64(left), boxed_f64(right)], 0);
            assert_eq!(result.status, STATUS_RETURNED);
            assert_eq!(result.value, Value::boolean(expected).to_bits());
        }
    }

    #[test]
    fn publishes_combined_integer_scalar_loop_through_machine_ir_backend() {
        let view = integer_scalar_loop_view();
        let hir = NumericFunction::build(&view).expect("integer-scalar numeric HIR");
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerMul(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerShiftRightLogical(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerLessThan(..)))
        );
        let sequence = select(&hir).expect("integer-scalar Machine IR");
        let allocation = sequence
            .allocate(&TargetRegisterFile::aarch64_scalar_function())
            .expect("integer-scalar allocation");
        let frame = arm64::frame_layout(&allocation, 0).expect("integer-scalar frame");
        lower_deopt_table(
            &sequence,
            &allocation,
            frame,
            arm64::GPR_BUDGET,
            arm64::FP_BUDGET,
            &machine_frame_states(&hir),
        )
        .expect("integer-scalar deopt table");

        let output = crate::optimizing::compile_optimized_with_artifacts(
            &view,
            7006,
            &TransitionTable::resolve(),
            Some(ArtifactRequest {
                identity: JitArtifactIdentity {
                    function_name: "engineKernel".to_string(),
                    module: "benchmarks/scripts/integer-scalar.js".to_string(),
                },
                tier: JitDebugTier::Optimizing,
                entry: JitDebugTarget::Entry,
            }),
            false,
        )
        .expect("production selector compiles integer-scalar loop");
        let optimized_ir = std::str::from_utf8(
            output
                .artifact
                .as_ref()
                .expect("integer-scalar artifact")
                .file(JitArtifactFileName::OptimizedIr)
                .expect("integer-scalar optimized IR")
                .contents(),
        )
        .expect("UTF-8 optimized IR");
        assert!(optimized_ir.starts_with("; backend=otter-machine-ir scalar-function\n"));

        let (result, _, _) = execute(&output.code, &[], 0);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(unbox_number(result.value), 1725.0);
    }

    #[test]
    fn publishes_float_leaf_loop_through_machine_ir_backend() {
        let view = float_leaf_loop_view();
        let hir = NumericFunction::build(&view).expect("float-leaf-loop numeric HIR");
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerNeg(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::Rem(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::Pow(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::FloatToBoolean(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::BooleanNot(..)))
        );

        let sequence = select(&hir).expect("float-leaf-loop Machine IR");
        let leaf = sequence
            .instructions()
            .iter()
            .find(|instruction| instruction.opcode == MachineOpcode::FloatRem)
            .expect("fixed-ABI FP leaf");
        assert_eq!(
            leaf.operands
                .iter()
                .map(|operand| operand.constraint)
                .collect::<Vec<_>>(),
            [
                OperandConstraint::Fixed(PhysicalRegister::float(0)),
                OperandConstraint::Fixed(PhysicalRegister::float(1)),
                OperandConstraint::Fixed(PhysicalRegister::float(0)),
            ]
        );
        let allocation = sequence
            .allocate(&TargetRegisterFile::aarch64_scalar_function())
            .expect("float-leaf-loop allocation");
        assert!(
            allocation.used_registers().any(|register| {
                (register.is_integer() && (20..=28).contains(&register.encoding()))
                    || (register.is_float() && (8..=15).contains(&register.encoding()))
            }),
            "values live across leaf calls must occupy callee-saved registers"
        );
        let output = crate::optimizing::compile_optimized_with_artifacts(
            &view,
            7007,
            &TransitionTable::resolve(),
            Some(ArtifactRequest {
                identity: JitArtifactIdentity {
                    function_name: "engineKernel".to_string(),
                    module: "benchmarks/scripts/float-leaf-math.js".to_string(),
                },
                tier: JitDebugTier::Optimizing,
                entry: JitDebugTarget::Entry,
            }),
            false,
        )
        .expect("production selector compiles float leaf loop");
        let optimized_ir = std::str::from_utf8(
            output
                .artifact
                .as_ref()
                .expect("float-leaf-loop artifact")
                .file(JitArtifactFileName::OptimizedIr)
                .expect("float-leaf-loop optimized IR")
                .contents(),
        )
        .expect("UTF-8 optimized IR");
        assert!(optimized_ir.starts_with("; backend=otter-machine-ir scalar-function\n"));
        assert!(optimized_ir.contains("FloatRem"));
        assert!(optimized_ir.contains("FloatPow"));
        assert!(!optimized_ir.contains("FloatLeafResult"));

        let (result, _, _) = execute(&output.code, &[], 0);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, tag::box_int32(199_999));

        let interrupt = 1_u8;
        let mut fuel = i64::MAX as u64;
        let (result, frame, pc) = execute_with_poll_cells(
            &output.code,
            &[],
            0,
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(pc, 6);
        assert_eq!(unbox_number(frame[0]), 0.75);
        assert_eq!(frame[1], tag::box_int32(1));
        assert_eq!(frame[2], tag::box_int32(2));
    }

    #[test]
    fn publishes_float_bitwise_loop_through_machine_ir_backend() {
        let view = float_bitwise_loop_view();
        let hir = NumericFunction::build(&view).expect("float-bitwise-loop numeric HIR");
        assert!(
            hir.nodes
                .iter()
                .filter(|node| matches!(node, NumericNode::FloatToInt32(..)))
                .count()
                >= 2
        );
        let sequence = select(&hir).expect("float-bitwise-loop Machine IR");
        let leaf = sequence
            .instructions()
            .iter()
            .find(|instruction| instruction.opcode == MachineOpcode::Float64ToInt32)
            .expect("fixed-ABI ToInt32 leaf");
        assert_eq!(
            leaf.operands
                .iter()
                .map(|operand| operand.constraint)
                .collect::<Vec<_>>(),
            [
                OperandConstraint::Fixed(PhysicalRegister::float(0)),
                OperandConstraint::Fixed(PhysicalRegister::integer(0)),
            ]
        );
        let allocation = sequence
            .allocate(&TargetRegisterFile::aarch64_scalar_function())
            .expect("float-bitwise-loop allocation");
        assert!(
            allocation.used_registers().any(|register| {
                (register.is_integer() && (20..=28).contains(&register.encoding()))
                    || (register.is_float() && (8..=15).contains(&register.encoding()))
            }),
            "loop-carried values live across ToInt32 leaves must occupy callee-saved registers"
        );

        let output = crate::optimizing::compile_optimized_with_artifacts(
            &view,
            7008,
            &TransitionTable::resolve(),
            Some(ArtifactRequest {
                identity: JitArtifactIdentity {
                    function_name: "engineKernel".to_string(),
                    module: "benchmarks/scripts/float-bitwise.js".to_string(),
                },
                tier: JitDebugTier::Optimizing,
                entry: JitDebugTarget::Entry,
            }),
            false,
        )
        .expect("production selector compiles float bitwise loop");
        let optimized_ir = std::str::from_utf8(
            output
                .artifact
                .as_ref()
                .expect("float-bitwise artifact")
                .file(JitArtifactFileName::OptimizedIr)
                .expect("float-bitwise optimized IR")
                .contents(),
        )
        .expect("UTF-8 optimized IR");
        assert!(optimized_ir.starts_with("; backend=otter-machine-ir scalar-function\n"));
        assert!(optimized_ir.contains("Float64ToInt32"));
        assert!(!optimized_ir.contains("IntegerLeafResult"));

        let (result, _, _) = execute(&output.code, &[], 0);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, tag::box_int32(120_790));

        let interrupt = 1_u8;
        let mut fuel = i64::MAX as u64;
        let (result, frame, pc) = execute_with_poll_cells(
            &output.code,
            &[],
            0,
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(pc, 4);
        assert_eq!(unbox_number(frame[0]), 4_294_967_299.25);
        assert_eq!(frame[1], tag::box_int32(0));
        assert_eq!(frame[2], tag::box_int32(1));
    }

    #[test]
    fn checked_integer_overflow_reconstructs_exact_mid_loop_frames() {
        let add = compile_output(&branch_phi_loop_view_with(i32::MAX, 0, 1, 1), None).code;
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let (result, frame, pc) =
            execute_with_poll_cells(&add, &[], 0, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(pc, 13);
        assert_eq!(frame[0], tag::box_int32(i32::MAX));
        assert_eq!(frame[1], tag::box_int32(0));
        assert_eq!(frame[2], tag::box_int32(2));

        let add_immediate =
            compile_output(&branch_phi_loop_view_with(0, 1, 2, i32::MAX), None).code;
        let mut fuel = i64::MAX as u64;
        let (result, frame, pc) = execute_with_poll_cells(
            &add_immediate,
            &[],
            0,
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(pc, 15);
        assert_eq!(frame[0], tag::box_int32(-14));
        assert_eq!(frame[1], tag::box_int32(1));
    }

    #[test]
    fn numeric_osr_enters_exact_header_and_rejects_without_vm_mutation() {
        let code = compile_output(&branch_phi_loop_view_with(0, 0, 5, 1), None).code;
        let mut frame = vec![Value::undefined().to_bits(); 12];
        frame[0] = tag::box_int32(2);
        frame[1] = tag::box_int32(1);
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let (result, after, pc) = execute_osr_with_poll_cells(
            &code,
            3,
            frame.clone(),
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, tag::box_int32(-22));
        assert_eq!(after, frame, "successful OSR must keep VM slots untouched");
        assert_eq!(pc, 3);

        frame[0] = Value::boolean(true).to_bits();
        let rejected = frame.clone();
        let mut fuel = i64::MAX as u64;
        let (result, after, pc) =
            execute_osr_with_poll_cells(&code, 3, frame, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(pc, 3);
        assert_eq!(after, rejected, "OSR representation reject must be atomic");
    }

    #[test]
    fn numeric_osr_deopts_overflow_and_interrupts_before_phi_moves() {
        let overflow = compile_output(&branch_phi_loop_view_with(i32::MAX, 0, 1, 1), None).code;
        let mut frame = vec![Value::undefined().to_bits(); 12];
        frame[0] = tag::box_int32(i32::MAX);
        frame[1] = tag::box_int32(0);
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let (result, frame, pc) = execute_osr_with_poll_cells(
            &overflow,
            3,
            frame,
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(pc, 13);
        assert_eq!(frame[0], tag::box_int32(i32::MAX));
        assert_eq!(frame[1], tag::box_int32(0));
        assert_eq!(frame[2], tag::box_int32(2));

        let code = compile_output(&branch_phi_loop_view_with(0, 0, 5, 1), None).code;
        let mut frame = vec![Value::undefined().to_bits(); 12];
        frame[0] = tag::box_int32(2);
        frame[1] = tag::box_int32(1);
        let interrupt = 1_u8;
        let mut fuel = i64::MAX as u64;
        let (result, frame, pc) =
            execute_osr_with_poll_cells(&code, 3, frame, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(pc, 3);
        assert_eq!(frame[0], tag::box_int32(-12));
        assert_eq!(frame[1], tag::box_int32(2));
        assert_eq!(frame[2], Value::undefined().to_bits());
    }

    #[test]
    fn numeric_osr_materializes_float_uint32_and_boolean_headers() {
        let float_view = float_bitwise_loop_view();
        let float_hir = NumericFunction::build(&float_view).expect("float OSR HIR");
        let float_sequence = select(&float_hir).expect("float OSR Machine IR");
        let float_inputs = float_sequence
            .instructions()
            .iter()
            .find_map(|instruction| match &instruction.opcode {
                MachineOpcode::OsrEntry {
                    logical_pc: 4,
                    inputs,
                } => Some(inputs.as_slice()),
                _ => None,
            })
            .expect("float OSR marker");
        assert_eq!(
            float_inputs,
            [
                MachineOsrInput {
                    frame_register: 0,
                    value_type: MachineOsrType::Float64,
                },
                MachineOsrInput {
                    frame_register: 1,
                    value_type: MachineOsrType::Int32,
                },
                MachineOsrInput {
                    frame_register: 2,
                    value_type: MachineOsrType::Int32,
                },
                MachineOsrInput {
                    frame_register: 3,
                    value_type: MachineOsrType::Int32,
                },
            ]
        );
        let float_code = compile_output(&float_view, None).code;
        let mut float_frame = vec![Value::undefined().to_bits(); 12];
        float_frame[0] = boxed_f64(4_294_967_299.25);
        float_frame[1] = tag::box_int32(0);
        float_frame[2] = tag::box_int32(1);
        float_frame[3] = tag::box_int32(200_000);
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let (result, after, _) = execute_osr_with_poll_cells(
            &float_code,
            4,
            float_frame.clone(),
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, tag::box_int32(120_790));
        assert_eq!(after, float_frame);

        let view = mixed_osr_loop_view();
        let hir = NumericFunction::build(&view).expect("mixed OSR numeric HIR");
        let sequence = select(&hir).expect("mixed OSR Machine IR");
        let inputs = sequence
            .instructions()
            .iter()
            .find_map(|instruction| match &instruction.opcode {
                MachineOpcode::OsrEntry {
                    logical_pc: 6,
                    inputs,
                } => Some(inputs.as_slice()),
                _ => None,
            })
            .expect("mixed OSR marker");
        assert_eq!(
            inputs
                .iter()
                .map(|input| input.value_type)
                .collect::<Vec<_>>(),
            [
                MachineOsrType::Uint32,
                MachineOsrType::Boolean,
                MachineOsrType::Int32,
                MachineOsrType::Int32,
            ]
        );

        let code = compile_output(&view, None).code;
        let mut frame = vec![Value::undefined().to_bits(); 10];
        frame[0] = boxed_f64(f64::from(u32::MAX));
        frame[1] = Value::boolean(true).to_bits();
        frame[2] = tag::box_int32(0);
        frame[3] = tag::box_int32(3);
        let mut fuel = i64::MAX as u64;
        let (result, after, _) = execute_osr_with_poll_cells(
            &code,
            6,
            frame.clone(),
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, tag::box_int32(536_870_911));
        assert_eq!(after, frame);

        frame[1] = tag::box_int32(1);
        let rejected = frame.clone();
        let mut fuel = i64::MAX as u64;
        let (result, after, pc) =
            execute_osr_with_poll_cells(&code, 6, frame, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(pc, 6);
        assert_eq!(after, rejected);
    }

    #[test]
    fn numeric_osr_materializes_allocator_spill_homes() {
        const LIVE_VALUES: usize = 30;
        const HEADER_PC: u32 = LIVE_VALUES as u32 + 3;
        let view = osr_spill_pressure_loop_view();
        let hir = NumericFunction::build(&view).expect("OSR spill-pressure HIR");
        let sequence = select(&hir).expect("OSR spill-pressure Machine IR");
        let marker = sequence
            .instructions()
            .iter()
            .position(|instruction| matches!(instruction.opcode, MachineOpcode::OsrEntry { .. }))
            .map(|index| MachineInstructionId(index as u32))
            .expect("OSR spill-pressure marker");
        let allocation = sequence
            .allocate(&TargetRegisterFile::aarch64_scalar_function())
            .expect("OSR spill-pressure allocation");
        assert!(
            allocation
                .instruction_locations(marker)
                .expect("OSR marker locations")
                .iter()
                .any(|location| matches!(location, AllocatedLocation::Stack(_))),
            "OSR pressure fixture must exercise direct spill materialization"
        );

        let code = compile_output(&view, None).code;
        let mut frame = vec![Value::undefined().to_bits(); LIVE_VALUES + 5];
        for (register, slot) in frame.iter_mut().enumerate().take(LIVE_VALUES) {
            *slot = tag::box_int32(register as i32 + 1);
        }
        frame[LIVE_VALUES] = tag::box_int32(0);
        frame[LIVE_VALUES + 1] = tag::box_int32(1);
        frame[LIVE_VALUES + 3] = tag::box_int32(0);
        let original = frame.clone();
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let (result, after, _) = execute_osr_with_poll_cells(
            &code,
            HEADER_PC,
            frame,
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, tag::box_int32(465));
        assert_eq!(after, original);
    }

    #[test]
    fn backedge_poll_refills_fuel_and_interrupt_bails_before_phi_moves() {
        extern "C" fn refill(ctx: *mut JitCtx) -> u64 {
            // SAFETY: the execution fixture keeps its thread and fuel cell live.
            unsafe {
                let thread = &*(*ctx).thread;
                *(thread.backedge_fuel_cell as *mut u64) = 100;
            }
            0
        }

        let mut transitions = TransitionTable::resolve();
        transitions.replace_entry_for_test(STUB_JIT_BACKEDGE_POLL, refill as *const () as usize);
        let code = compile_output_with_transitions(
            &branch_phi_loop_view_with(0, 0, 5, 1),
            &transitions,
            None,
        )
        .code;
        let interrupt = 0_u8;
        let mut fuel = 1_u64;
        let (result, _, _) =
            execute_with_poll_cells(&code, &[], 0, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(result.value, tag::box_int32(-22));
        assert_eq!(fuel, 96);

        let interrupt = 1_u8;
        let mut fuel = i64::MAX as u64;
        let (result, frame, pc) =
            execute_with_poll_cells(&code, &[], 0, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(result.status, STATUS_BAILED);
        assert_eq!(pc, 3);
        assert_eq!(frame[0], tag::box_int32(2));
        assert_eq!(frame[1], tag::box_int32(1));
        assert_eq!(frame[2], Value::undefined().to_bits());
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
                .starts_with("; backend=otter-machine-ir scalar-function\n")
        );
        assert!(text(JitArtifactFileName::CodeMap).contains("\"kind\": \"machineScalarFunction\""));
        assert_eq!(
            artifact
                .file(JitArtifactFileName::Code)
                .expect("exact code artifact")
                .contents(),
            output.code.compiled_code().bytes(),
        );
    }
}
