//! Production numeric-function lowering through the shared Machine IR pipeline.
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
//! - Emitted functions contain no call or safepoint and own no GC roots.

mod arm64;
mod hir;

use otter_vm::{JitArtifactFileName, JitCompileSnapshot, deopt::DeoptTable};
use std::collections::{BTreeMap, BTreeSet};

use self::hir::{NumericFramePoint, NumericFunction, NumericNode, NumericTerminator, NumericType};
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
    // Loop CFG/SSA is constructed now, but publishing native code waits for
    // backedge polls and allocator-driven FrameState reconstruction.
    if hir.has_backedges || hir.requires_integer_lowering {
        return Ok(None);
    }
    let sequence = select(&hir)
        .map_err(|_| Unsupported::OperandShape("numeric HIR to Machine IR selection"))?;
    let allocation = sequence
        .allocate(&TargetRegisterFile::aarch64_numeric_function())
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
            "; backend=otter-machine-ir numeric-function\n; parameters={} registers={} blocks={} arithmetic-ops={}\n",
            hir.parameter_count,
            hir.register_count,
            hir.blocks.len(),
            hir.arithmetic_op_count
        );
        tier_input.push_str(&sequence.normalized());
        tier_input.push_str(&allocation.normalized());
        let mut code_map = CodeMapCapture::default();
        code_map.record(CodeRegion::structural(
            "machineNumericFunction",
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
    let mut representations = hir
        .nodes
        .iter()
        .map(|node| match node.value_type() {
            NumericType::Int32 => MachineRepresentation::Int32,
            NumericType::Number => MachineRepresentation::Float64,
            NumericType::Boolean => MachineRepresentation::Int32,
        })
        .collect::<Vec<_>>();
    let values = (0..hir.nodes.len())
        .map(|index| MachineValue(index as u32))
        .collect::<Vec<_>>();
    let mut tagged_parameters = Vec::with_capacity(hir.parameter_count as usize);
    for _ in 0..hir.parameter_count {
        let tagged = push_value(&mut representations, MachineRepresentation::Tagged);
        tagged_parameters.push(tagged);
    }

    let selection_cfg = SelectionCfg::build(hir);
    let mut instructions = Vec::with_capacity(hir.nodes.len() + hir.parameter_count as usize + 4);
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
            for parameter in 0..hir.parameter_count {
                instructions.push(MachineInstruction::plain(
                    MachineOpcode::EntryValue(parameter),
                    vec![MachineOperand::register_output(
                        tagged_parameters[usize::from(parameter)],
                    )],
                ));
            }
        }
        for &node_value in &block.nodes {
            let result = values[node_value.0];
            let node = hir.nodes[node_value.0];
            let mut instruction = match node {
                NumericNode::Parameter(parameter) => {
                    let tagged = tagged_parameters[usize::from(parameter)];
                    MachineInstruction::plain(
                        MachineOpcode::DecodeNumber,
                        vec![
                            MachineOperand::register_input(tagged),
                            MachineOperand::register_output(result),
                        ],
                    )
                }
                NumericNode::BlockParameter(_) => continue,
                NumericNode::IntegerConstant(value) => MachineInstruction::plain(
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
                NumericNode::IntegerAdd(left, right) => MachineInstruction::plain(
                    MachineOpcode::IntegerAdd,
                    vec![
                        MachineOperand::register_input(machine_value(&values, left)),
                        MachineOperand::register_input(machine_value(&values, right)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::IntegerAddImmediate(source, immediate)
                | NumericNode::IntegerAndImmediate(source, immediate)
                | NumericNode::IntegerLessThanImmediate(source, immediate)
                | NumericNode::IntegerEqualImmediate(source, immediate) => {
                    let opcode = match node {
                        NumericNode::IntegerAddImmediate(..) => {
                            MachineOpcode::IntegerAddImmediate(immediate)
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
                NumericNode::LessThan(left, right) => MachineInstruction::plain(
                    MachineOpcode::FloatLessThan,
                    vec![
                        MachineOperand::register_input(machine_value(&values, left)),
                        MachineOperand::register_input(machine_value(&values, right)),
                        MachineOperand::register_output(result),
                    ],
                ),
            };
            if let Some(&deopt) = frame_state_ids.get(&NumericFramePoint::Node(node_value)) {
                attach_frame_state(hir, &values, deopt, &mut instruction);
            }
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
                let boxed = push_value(&mut representations, MachineRepresentation::Tagged);
                let box_opcode = match hir.nodes[value.0].value_type() {
                    NumericType::Int32 => MachineOpcode::BoxInt32,
                    NumericType::Number => MachineOpcode::BoxNumber,
                    NumericType::Boolean => {
                        return Err(super::VerificationError::InvalidValue(machine_value(
                            &values, value,
                        )));
                    }
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
        Vec::new(),
        blocks,
        instructions,
    )
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

#[cfg(test)]
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
        jit_feedback::{ARITH_FLOAT64, ARITH_INT32, ArithFeedback},
        native_abi::{NativeFrame, NativeFrameFlags, NativeFrameKind, VmFrameHeader, VmThread},
        value::tag,
    };

    use super::*;
    use crate::entry::{JitCtx, JitEntry, JitRet, STATUS_BAILED, STATUS_RETURNED};
    use crate::machine::{MachineFrameLayout, lower_deopt_table};

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
                    | Op::Neg
                    | Op::LessThan
                    | Op::AddImm
                    | Op::BitwiseAndImm
                    | Op::LessThanImm
                    | Op::EqualImm
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
        let mut view = numeric_view(
            0,
            12,
            vec![
                (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(0)]),
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
                        Operand::Imm32(1_000_000),
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
                        Operand::Imm32(1),
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
        try_compile(view, 7001, artifact_request)
            .expect("numeric Machine IR code generation")
            .expect("eligible numeric function")
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
            "numeric function must not mutate VM slots"
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
    fn builds_and_allocates_loop_header_parameters_before_native_publication() {
        let view = loop_view();
        let hir = NumericFunction::build(&view).expect("loop numeric HIR");
        assert!(hir.has_backedges);
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
            .allocate(&TargetRegisterFile::aarch64_numeric_function())
            .expect("loop Machine IR allocation");

        assert!(
            try_compile(&view, 7002, None)
                .expect("loop compilation decision")
                .is_none(),
            "native publication waits for poll and FrameState lowering"
        );
    }

    #[test]
    fn builds_branch_phi_as_typed_int32_loop_before_frame_state_publication() {
        let view = branch_phi_loop_view();
        let hir = NumericFunction::build(&view).expect("branch-phi numeric HIR");
        assert!(hir.has_backedges);
        assert!(hir.requires_integer_lowering);
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
            .allocate(&TargetRegisterFile::aarch64_numeric_function())
            .expect("branch-phi Machine IR allocation");
        let layout = MachineFrameLayout::new(&allocation, 16, 16).expect("branch-phi frame layout");
        let deopt_table = lower_deopt_table(
            &sequence,
            &allocation,
            layout,
            16,
            8,
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
        assert!(
            try_compile(&view, 7003, None)
                .expect("branch-phi compilation decision")
                .is_none(),
            "native publication waits for emitting overflow exits and the backedge poll"
        );
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
                .starts_with("; backend=otter-machine-ir numeric-function\n")
        );
        assert!(
            text(JitArtifactFileName::CodeMap).contains("\"kind\": \"machineNumericFunction\"")
        );
        assert_eq!(
            artifact
                .file(JitArtifactFileName::Code)
                .expect("exact code artifact")
                .contents(),
            output.code.compiled_code().bytes(),
        );
    }
}
