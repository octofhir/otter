//! Abstract interpreter-frame reconstruction state over SSA values.
//!
//! # Contents
//! - [`AbstractFrameState`] — reaching SSA values at one bytecode boundary.
//! - [`FrameStateTable`] — deterministic exact-PC frame-state lookup.
//! - [`FrameStateTable::build`] — normal-dominator-forest reaching-value walk.
//! - [`FrameStateTable::verify`] — completeness, dominance, and operand checks.
//! - [`FrameStateError`] — precise construction and verification failures.
//!
//! # Invariants
//! - Every canonical instruction PC has exactly one state, sorted by PC.
//! - State slots are interpreter registers, never machine locations.
//! - Construction follows the same normal-edge dominator forest and block-head
//!   definitions as SSA renaming, including independent exception-handler roots.
//! - Verification uses full-edge dominance and cross-checks every SSA operand.
//!
//! # See also
//! - [`crate::ir::ssa`]
//! - [`crate::ir::dom`]

use super::{
    cfg::{BlockId, ControlFlowGraph},
    dom::{DomError, DominatorTree},
    ssa::{SsaError, SsaFunction, ValueDef, ValueId},
};

/// Reaching SSA values for all interpreter registers at one deopt point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbstractFrameState {
    /// Canonical bytecode PC of the instruction about to execute.
    pub pc: u32,
    /// CFG block containing the instruction.
    pub block: BlockId,
    /// Reaching SSA value per interpreter register.
    pub registers: Box<[Option<ValueId>]>,
}

/// Exact-PC abstract frame states for one SSA function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameStateTable {
    states: Box<[AbstractFrameState]>,
}

/// Failure to construct or verify abstract frame state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameStateError {
    /// Construction was given invalid SSA.
    InvalidSsa(SsaError),
    /// A block-head value is outside dense SSA storage.
    HeadValueOutOfRange {
        /// Block containing the head value.
        block: BlockId,
        /// Invalid value identity.
        value: ValueId,
    },
    /// A block-head value does not define an interpreter register.
    InvalidHeadDefinition {
        /// Block containing the head value.
        block: BlockId,
        /// Invalid value identity.
        value: ValueId,
    },
    /// A block-head definition names a register outside the frame.
    HeadRegisterOutOfRange {
        /// Block containing the head value.
        block: BlockId,
        /// Invalid register.
        register: u16,
        /// Function register count.
        register_count: u16,
    },
    /// An instruction's SSA result and destination-register metadata disagree.
    ResultRegisterMismatch {
        /// Instruction with mismatched result metadata.
        pc: u32,
    },
    /// An instruction result is outside dense SSA storage.
    ResultValueOutOfRange {
        /// Instruction defining the invalid value.
        pc: u32,
        /// Invalid result identity.
        value: ValueId,
    },
    /// An instruction result names a register outside the frame.
    ResultRegisterOutOfRange {
        /// Instruction defining the register.
        pc: u32,
        /// Invalid register.
        register: u16,
        /// Function register count.
        register_count: u16,
    },
    /// Verification was given a normal-edge-only dominator tree.
    NormalDominatorUsedForVerification,
    /// The supplied full-edge dominator tree is internally invalid.
    InvalidFullDominator(DomError),
    /// SSA block storage does not cover the CFG block set.
    SsaBlockCountMismatch {
        /// Number of CFG blocks.
        expected: usize,
        /// Number of SSA blocks.
        actual: usize,
    },
    /// An SSA block's instruction PCs differ from its CFG block.
    SsaInstructionLayoutMismatch {
        /// Block with mismatched instruction layout.
        block: BlockId,
    },
    /// Stored states do not cover exactly all instructions.
    StateCountMismatch {
        /// Number of CFG instructions.
        expected: usize,
        /// Number of stored states.
        actual: usize,
    },
    /// A stored state is absent from its canonical sorted-PC position.
    StatePcMismatch {
        /// Position in the state table.
        index: usize,
        /// Expected canonical PC.
        expected: u32,
        /// Stored PC.
        actual: u32,
    },
    /// A stored state names the wrong containing block.
    StateBlockMismatch {
        /// Canonical PC of the state.
        pc: u32,
        /// Expected containing block.
        expected: BlockId,
        /// Stored containing block.
        actual: BlockId,
    },
    /// A state does not contain exactly one slot per interpreter register.
    RegisterCountMismatch {
        /// Canonical PC of the state.
        pc: u32,
        /// Function register count.
        expected: usize,
        /// Number of stored slots.
        actual: usize,
    },
    /// A state slot references a value outside dense SSA storage.
    ValueOutOfRange {
        /// Canonical PC of the state.
        pc: u32,
        /// Register containing the invalid value.
        register: u16,
        /// Invalid value identity.
        value: ValueId,
        /// Number of valid SSA values.
        value_count: usize,
    },
    /// A state slot's definition does not dominate the deopt point's block.
    ValueDefinitionDoesNotDominate {
        /// Canonical PC of the state.
        pc: u32,
        /// Register containing the invalid value.
        register: u16,
        /// Non-dominating value.
        value: ValueId,
        /// Block defining the value.
        definition: BlockId,
        /// Block containing the deopt point.
        block: BlockId,
    },
    /// An instruction does not retain one source register per SSA input.
    OperandRegisterCountMismatch {
        /// Instruction with mismatched operand metadata.
        pc: u32,
        /// Number of SSA inputs.
        inputs: usize,
        /// Number of source registers.
        registers: usize,
    },
    /// A retained operand register lies outside the frame.
    OperandRegisterOutOfRange {
        /// Instruction using the invalid register.
        pc: u32,
        /// Operand position.
        operand_index: usize,
        /// Invalid register.
        register: u16,
        /// Function register count.
        register_count: u16,
    },
    /// A register's frame-state value differs from the SSA operand value.
    OperandValueMismatch {
        /// Instruction containing the operand.
        pc: u32,
        /// Operand position.
        operand_index: usize,
        /// Source interpreter register.
        register: u16,
        /// Value recorded by SSA renaming.
        expected: ValueId,
        /// Value recorded in the frame state.
        actual: Option<ValueId>,
    },
}

impl std::fmt::Display for FrameStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid abstract deopt frame state: {self:?}")
    }
}

impl std::error::Error for FrameStateError {}

impl FrameStateTable {
    /// Build one abstract frame state before every SSA instruction.
    pub fn build(ssa: &SsaFunction, cfg: &ControlFlowGraph) -> Result<Self, FrameStateError> {
        let full_dom = DominatorTree::compute(cfg);
        ssa.verify(cfg, &full_dom)
            .map_err(FrameStateError::InvalidSsa)?;
        let normal_dom = DominatorTree::compute_normal(cfg);

        enum Event {
            Enter(BlockId),
            Exit(Vec<u16>),
        }

        let mut children = vec![Vec::new(); cfg.blocks.len()];
        for &block in normal_dom.reverse_postorder() {
            if let Some(parent) = normal_dom.immediate_dominator(block) {
                children[parent.0 as usize].push(block);
            }
        }
        let roots: Vec<_> = normal_dom
            .reverse_postorder()
            .iter()
            .copied()
            .filter(|&block| normal_dom.immediate_dominator(block).is_none())
            .collect();
        let mut stacks = vec![Vec::new(); usize::from(ssa.register_count)];
        let mut states = Vec::new();

        for root in roots {
            let mut events = vec![Event::Enter(root)];
            while let Some(event) = events.pop() {
                match event {
                    Event::Exit(pushed) => {
                        for register in pushed.into_iter().rev() {
                            stacks[usize::from(register)]
                                .pop()
                                .expect("frame-state walk pops exactly the values it pushed");
                        }
                    }
                    Event::Enter(block) => {
                        let block_index = block.0 as usize;
                        let mut pushed = Vec::new();
                        for &value in &ssa.blocks[block_index].phis {
                            let data = ssa
                                .values
                                .get(value.0 as usize)
                                .ok_or(FrameStateError::HeadValueOutOfRange { block, value })?;
                            let register = head_register(&data.def)
                                .ok_or(FrameStateError::InvalidHeadDefinition { block, value })?;
                            if register >= ssa.register_count {
                                return Err(FrameStateError::HeadRegisterOutOfRange {
                                    block,
                                    register,
                                    register_count: ssa.register_count,
                                });
                            }
                            stacks[usize::from(register)].push(value);
                            pushed.push(register);
                        }

                        for instruction in &ssa.blocks[block_index].instrs {
                            states.push(AbstractFrameState {
                                pc: instruction.pc,
                                block,
                                registers: stacks
                                    .iter()
                                    .map(|stack| stack.last().copied())
                                    .collect::<Vec<_>>()
                                    .into_boxed_slice(),
                            });

                            match (instruction.result, instruction.result_register) {
                                (Some(value), Some(register)) => {
                                    if ssa.values.get(value.0 as usize).is_none() {
                                        return Err(FrameStateError::ResultValueOutOfRange {
                                            pc: instruction.pc,
                                            value,
                                        });
                                    }
                                    if register >= ssa.register_count {
                                        return Err(FrameStateError::ResultRegisterOutOfRange {
                                            pc: instruction.pc,
                                            register,
                                            register_count: ssa.register_count,
                                        });
                                    }
                                    stacks[usize::from(register)].push(value);
                                    pushed.push(register);
                                }
                                (None, None) => {}
                                _ => {
                                    return Err(FrameStateError::ResultRegisterMismatch {
                                        pc: instruction.pc,
                                    });
                                }
                            }
                        }

                        events.push(Event::Exit(pushed));
                        for &child in children[block_index].iter().rev() {
                            events.push(Event::Enter(child));
                        }
                    }
                }
            }
            debug_assert!(stacks.iter().all(Vec::is_empty));
        }

        states.sort_by_key(|state| state.pc);
        let table = Self {
            states: states.into_boxed_slice(),
        };
        table.verify(ssa, cfg, &full_dom)?;
        Ok(table)
    }

    /// Return all states in ascending canonical-PC order.
    #[must_use]
    pub fn states(&self) -> &[AbstractFrameState] {
        &self.states
    }

    /// Return the frame state at exactly `pc`.
    #[must_use]
    pub fn at(&self, pc: u32) -> Option<&AbstractFrameState> {
        let index = self
            .states
            .binary_search_by_key(&pc, |state| state.pc)
            .ok()?;
        Some(&self.states[index])
    }

    /// Verify completeness, widths, full-edge dominance, and SSA operands.
    pub fn verify(
        &self,
        ssa: &SsaFunction,
        cfg: &ControlFlowGraph,
        full_dom: &DominatorTree,
    ) -> Result<(), FrameStateError> {
        if !full_dom.includes_exception_edges() {
            return Err(FrameStateError::NormalDominatorUsedForVerification);
        }
        full_dom
            .verify(cfg)
            .map_err(FrameStateError::InvalidFullDominator)?;
        if ssa.blocks.len() != cfg.blocks.len() {
            return Err(FrameStateError::SsaBlockCountMismatch {
                expected: cfg.blocks.len(),
                actual: ssa.blocks.len(),
            });
        }

        let expected_count: usize = cfg.blocks.iter().map(|block| block.instr_pcs.len()).sum();
        if self.states.len() != expected_count {
            return Err(FrameStateError::StateCountMismatch {
                expected: expected_count,
                actual: self.states.len(),
            });
        }

        let mut state_index = 0;
        for cfg_block in &cfg.blocks {
            let ssa_block = &ssa.blocks[cfg_block.id.0 as usize];
            if ssa_block.instrs.len() != cfg_block.instr_pcs.len()
                || ssa_block
                    .instrs
                    .iter()
                    .map(|instruction| instruction.pc)
                    .ne(cfg_block.instr_pcs.iter().copied())
            {
                return Err(FrameStateError::SsaInstructionLayoutMismatch {
                    block: cfg_block.id,
                });
            }

            for instruction in &ssa_block.instrs {
                let state = &self.states[state_index];
                if state.pc != instruction.pc {
                    return Err(FrameStateError::StatePcMismatch {
                        index: state_index,
                        expected: instruction.pc,
                        actual: state.pc,
                    });
                }
                if state.block != cfg_block.id {
                    return Err(FrameStateError::StateBlockMismatch {
                        pc: state.pc,
                        expected: cfg_block.id,
                        actual: state.block,
                    });
                }
                if state.registers.len() != usize::from(ssa.register_count) {
                    return Err(FrameStateError::RegisterCountMismatch {
                        pc: state.pc,
                        expected: usize::from(ssa.register_count),
                        actual: state.registers.len(),
                    });
                }

                for (register_index, value) in state.registers.iter().copied().enumerate() {
                    let Some(value) = value else {
                        continue;
                    };
                    let register = register_index as u16;
                    let Some(data) = ssa.values.get(value.0 as usize) else {
                        return Err(FrameStateError::ValueOutOfRange {
                            pc: state.pc,
                            register,
                            value,
                            value_count: ssa.values.len(),
                        });
                    };
                    if !full_dom.dominates(data.def_block, state.block) {
                        return Err(FrameStateError::ValueDefinitionDoesNotDominate {
                            pc: state.pc,
                            register,
                            value,
                            definition: data.def_block,
                            block: state.block,
                        });
                    }
                }

                if instruction.input_registers.len() != instruction.inputs.len() {
                    return Err(FrameStateError::OperandRegisterCountMismatch {
                        pc: instruction.pc,
                        inputs: instruction.inputs.len(),
                        registers: instruction.input_registers.len(),
                    });
                }
                for (operand_index, (&register, &expected)) in instruction
                    .input_registers
                    .iter()
                    .zip(&instruction.inputs)
                    .enumerate()
                {
                    let Some(actual) = state.registers.get(usize::from(register)).copied() else {
                        return Err(FrameStateError::OperandRegisterOutOfRange {
                            pc: instruction.pc,
                            operand_index,
                            register,
                            register_count: ssa.register_count,
                        });
                    };
                    if actual != Some(expected) {
                        return Err(FrameStateError::OperandValueMismatch {
                            pc: instruction.pc,
                            operand_index,
                            register,
                            expected,
                            actual,
                        });
                    }
                }
                state_index += 1;
            }
        }
        Ok(())
    }
}

fn head_register(definition: &ValueDef) -> Option<u16> {
    match definition {
        ValueDef::Param { register, .. }
        | ValueDef::Uninitialized { register }
        | ValueDef::ExceptionInput { register, .. }
        | ValueDef::InlineResult { register, .. }
        | ValueDef::Phi { register, .. } => Some(*register),
        ValueDef::InlineUndefinedReturn { .. } | ValueDef::Op { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use otter_bytecode::{NO_HANDLER_OFFSET, Op, Operand};
    use otter_vm::{JitCompileSnapshot, jit::JitTestInstruction};

    use super::*;

    fn snapshot(
        param_count: u16,
        register_count: u16,
        instructions: Vec<(Op, Vec<Operand>)>,
    ) -> JitCompileSnapshot {
        let instructions = instructions
            .into_iter()
            .enumerate()
            .map(|(pc, (op, operands))| {
                JitTestInstruction::new(op, pc as u32, pc as u32 * 4, operands)
            })
            .collect();
        JitCompileSnapshot::without_feedback(0, param_count, register_count, instructions)
    }

    fn analyses(
        param_count: u16,
        register_count: u16,
        instructions: Vec<(Op, Vec<Operand>)>,
    ) -> (
        ControlFlowGraph,
        SsaFunction,
        DominatorTree,
        FrameStateTable,
    ) {
        let snapshot = snapshot(param_count, register_count, instructions);
        let cfg = ControlFlowGraph::build(&snapshot).expect("CFG builds");
        let ssa = SsaFunction::build(&snapshot, &cfg).expect("SSA builds");
        let full_dom = DominatorTree::compute(&cfg);
        let states = FrameStateTable::build(&ssa, &cfg).expect("frame states build");
        states
            .verify(&ssa, &cfg, &full_dom)
            .expect("frame states verify");
        (cfg, ssa, full_dom, states)
    }

    fn op_value_at(ssa: &SsaFunction, pc: u32) -> ValueId {
        ssa.values
            .iter()
            .find_map(|value| match value.def {
                ValueDef::Op { pc: owner, .. } if owner == pc => Some(value.id),
                _ => None,
            })
            .expect("instruction has an SSA result")
    }

    fn phi_for(ssa: &SsaFunction, block: BlockId, register: u16) -> ValueId {
        ssa.blocks[block.0 as usize]
            .phis
            .iter()
            .copied()
            .find(|&value| {
                matches!(
                    ssa.values[value.0 as usize].def,
                    ValueDef::Phi { register: owner, .. } if owner == register
                )
            })
            .expect("expected register phi")
    }

    #[test]
    fn straight_line_snapshots_each_latest_definition() {
        let (cfg, ssa, _dom, states) = analyses(
            1,
            3,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(1)]),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(2)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(1),
                        Operand::Register(0),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );

        assert_eq!(
            states.at(1).unwrap().registers[1],
            Some(op_value_at(&ssa, 0))
        );
        assert_eq!(
            states.at(2).unwrap().registers[1],
            Some(op_value_at(&ssa, 1))
        );
        assert_eq!(
            states.at(3).unwrap().registers[2],
            Some(op_value_at(&ssa, 2))
        );
        assert_eq!(states, FrameStateTable::build(&ssa, &cfg).unwrap());
    }

    #[test]
    fn diamond_join_uses_phi_value() {
        let (cfg, ssa, _dom, states) = analyses(
            1,
            3,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(0)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(1), Operand::Imm32(10)],
                ),
                (Op::Jump, vec![Operand::Imm32(1)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(1), Operand::Imm32(20)],
                ),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        let join = cfg.blocks.iter().find(|block| block.start_pc == 5).unwrap();

        assert_eq!(
            states.at(5).unwrap().registers[1],
            Some(phi_for(&ssa, join.id, 1))
        );
    }

    #[test]
    fn loop_body_uses_loop_header_phi() {
        let (cfg, ssa, _dom, states) = analyses(
            1,
            3,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (Op::Jump, vec![Operand::Imm32(0)]),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(3), Operand::Register(1)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(1),
                        Operand::Register(1),
                        Operand::Register(0),
                    ],
                ),
                (Op::Nop, vec![]),
                (Op::Jump, vec![Operand::Imm32(-4)]),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        let header = cfg.blocks.iter().find(|block| block.start_pc == 2).unwrap();

        assert_eq!(
            states.at(3).unwrap().registers[1],
            Some(phi_for(&ssa, header.id, 1))
        );
    }

    #[test]
    fn handler_starts_from_exception_inputs() {
        let (cfg, ssa, _dom, states) = analyses(
            0,
            4,
            vec![
                (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(1)]),
                (
                    Op::EnterTry,
                    vec![
                        Operand::Imm32(4),
                        Operand::Imm32(NO_HANDLER_OFFSET),
                        Operand::Register(3),
                    ],
                ),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(2)]),
                (
                    Op::LoadGlobalOrThrow,
                    vec![Operand::Register(2), Operand::ConstIndex(0)],
                ),
                (Op::LeaveTry, vec![]),
                (Op::Jump, vec![Operand::Imm32(2)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(1),
                        Operand::Register(0),
                    ],
                ),
                (Op::Nop, vec![]),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let handler = cfg.blocks.iter().find(|block| block.start_pc == 6).unwrap();
        let state = states.at(6).unwrap();

        for (register, &value) in state.registers.iter().enumerate() {
            assert!(matches!(
                ssa.values[value.unwrap().0 as usize].def,
                ValueDef::ExceptionInput { block, register: owner }
                    if block == handler.id && owner == register as u16
            ));
        }
    }

    #[test]
    fn verifier_rejects_corrupted_operand_register_value() {
        let (cfg, ssa, full_dom, mut states) = analyses(
            1,
            3,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(1)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(1),
                        Operand::Register(0),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let expected = op_value_at(&ssa, 0);
        let replacement = states.at(1).unwrap().registers[0].unwrap();
        states.states[1].registers[1] = Some(replacement);

        assert_eq!(
            states.verify(&ssa, &cfg, &full_dom),
            Err(FrameStateError::OperandValueMismatch {
                pc: 1,
                operand_index: 0,
                register: 1,
                expected,
                actual: Some(replacement),
            })
        );
    }
}
