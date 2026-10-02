//! Precomputed logical control-flow metadata for executable CodeBlocks.
//!
//! # Contents
//! - [`CodeBlockControlFlow`] — immutable basic-block, loop-header, and
//!   exception-handler tables built from verified schema wordcode and the
//!   function's handler table.
//! - [`CodeBlockControlFlowView`] — borrowed read-only access for JIT consumers.
//!
//! # Invariants
//! - Every PC is a canonical instruction index, never a serialized byte PC.
//! - Targets are resolved once after wordcode verification; dispatch and JIT
//!   lowering do not reinterpret relative branch operands.
//! - Handler entries keep the verified table order, innermost first, so the
//!   first entry covering a PC is the handler a throw there lands in. Every
//!   handler target starts a basic block.
//!
//! # See also
//! - [`crate::CodeBlock`]
//! - [`otter_bytecode::ExceptionHandler`]
//! - [`otter_bytecode::opcode_schema`]

use std::collections::{BTreeMap, BTreeSet};

use otter_bytecode::{
    ExceptionHandler, FunctionCode, Operand,
    opcode_schema::{ControlFlow, SuccessorSpec, opcode_schema},
};

/// Borrowed access to one CodeBlock's immutable logical control-flow tables.
#[derive(Debug, Clone, Copy)]
pub struct CodeBlockControlFlowView<'a> {
    control_flow: &'a CodeBlockControlFlow,
}

impl<'a> CodeBlockControlFlowView<'a> {
    pub(crate) const fn new(control_flow: &'a CodeBlockControlFlow) -> Self {
        Self { control_flow }
    }

    /// Sorted logical PCs beginning basic blocks in this function.
    #[must_use]
    pub fn block_starts(self) -> &'a [u32] {
        self.control_flow.block_starts()
    }

    /// Sorted logical PCs targeted by backwards normal-flow edges.
    #[must_use]
    pub fn loop_headers(self) -> &'a [u32] {
        self.control_flow.loop_headers()
    }

    /// Last logical backedge PC for `header_pc`.
    #[must_use]
    pub fn loop_latch(self, header_pc: u32) -> Option<u32> {
        self.control_flow.loop_latch(header_pc)
    }

    /// The function's exception handlers, innermost first.
    #[must_use]
    pub fn handlers(self) -> &'a [ExceptionHandler] {
        self.control_flow.handlers()
    }

    /// The handler a throw by the instruction at `pc` lands in.
    #[must_use]
    pub fn handler_at(self, pc: u32) -> Option<ExceptionHandler> {
        self.control_flow.handler_at(pc)
    }
}

/// Immutable logical-PC tables shared by interpreter and JIT consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodeBlockControlFlow {
    block_starts: Box<[u32]>,
    loop_headers: Box<[u32]>,
    loop_latches: Box<[(u32, u32)]>,
    handlers: Box<[ExceptionHandler]>,
}

impl CodeBlockControlFlow {
    /// Heap bytes the four precomputed tables retain for the owning code
    /// block's lifetime.
    pub(crate) fn retained_bytes(&self) -> u64 {
        (std::mem::size_of_val::<[u32]>(&self.block_starts) as u64)
            .saturating_add(std::mem::size_of_val::<[u32]>(&self.loop_headers) as u64)
            .saturating_add(std::mem::size_of_val::<[(u32, u32)]>(&self.loop_latches) as u64)
            .saturating_add(std::mem::size_of_val::<[ExceptionHandler]>(&self.handlers) as u64)
    }

    /// Build tables from wordcode and a handler table that have already
    /// passed verification.
    pub(crate) fn from_verified_wordcode(code: &FunctionCode, handlers: &[ExceptionHandler]) -> Self {
        let mut block_starts = BTreeSet::new();
        let mut loop_latches = BTreeMap::<u32, u32>::new();
        let instruction_count = code.len() as u32;

        if instruction_count != 0 {
            block_starts.insert(0);
        }

        for (index, instruction) in code.iter().enumerate() {
            let pc = index as u32;
            let next_pc = pc + 1;
            let schema = opcode_schema(instruction.op);

            for successor in schema.successor_shape.exact() {
                if let SuccessorSpec::RelativeTarget { operand_index, .. } = successor {
                    let target = relative_target(code, index, *operand_index);
                    if target < instruction_count {
                        block_starts.insert(target);
                    }
                    if target < pc {
                        loop_latches
                            .entry(target)
                            .and_modify(|latch| *latch = (*latch).max(pc))
                            .or_insert(pc);
                    }
                }
            }

            if next_pc < instruction_count
                && !matches!(
                    schema.control_flow,
                    ControlFlow::Fallthrough | ControlFlow::Call
                )
            {
                block_starts.insert(next_pc);
            }
        }
        for handler in handlers {
            block_starts.insert(handler.target);
        }

        let loop_headers = loop_latches.keys().copied().collect();
        Self {
            block_starts: block_starts.into_iter().collect(),
            loop_headers,
            loop_latches: loop_latches.into_iter().collect(),
            handlers: handlers.into(),
        }
    }

    pub(crate) fn block_starts(&self) -> &[u32] {
        &self.block_starts
    }

    pub(crate) fn loop_headers(&self) -> &[u32] {
        &self.loop_headers
    }

    pub(crate) fn loop_latch(&self, header_pc: u32) -> Option<u32> {
        self.loop_latches
            .binary_search_by_key(&header_pc, |(header, _)| *header)
            .ok()
            .map(|index| self.loop_latches[index].1)
    }

    pub(crate) fn handlers(&self) -> &[ExceptionHandler] {
        &self.handlers
    }

    pub(crate) fn handler_at(&self, pc: u32) -> Option<ExceptionHandler> {
        self.handlers
            .iter()
            .find(|handler| handler.covers(pc))
            .copied()
    }
}

fn relative_target(code: &FunctionCode, instruction_index: usize, operand_index: usize) -> u32 {
    let instruction = &code[instruction_index];
    let Some(Operand::Imm32(delta)) = code.operand(instruction, operand_index) else {
        unreachable!("verified relative target operand")
    };
    let target = instruction_index as i64 + 1 + i64::from(delta);
    u32::try_from(target).expect("verified relative target is non-negative and in range")
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::{FunctionCodeBuilder, Op, Operand};

    #[test]
    fn derives_blocks_and_loop_headers_from_schema_successors() {
        let mut builder = FunctionCodeBuilder::new();
        builder.push(Op::JumpIfFalse, &[Operand::Imm32(2), Operand::Register(0)]);
        builder.push(Op::Nop, &[]);
        builder.push(Op::Jump, &[Operand::Imm32(-3)]);
        builder.push(Op::ReturnUndefined, &[]);
        let cfg = CodeBlockControlFlow::from_verified_wordcode(&builder.finish(), &[]);

        assert_eq!(cfg.block_starts(), &[0, 1, 3]);
        assert_eq!(cfg.loop_headers(), &[0]);
        assert_eq!(cfg.loop_latch(0), Some(2));
    }

    #[test]
    fn innermost_covering_handler_wins_and_targets_start_blocks() {
        let mut builder = FunctionCodeBuilder::new();
        builder.push(Op::Nop, &[]);
        builder.push(Op::Nop, &[]);
        builder.push(Op::ReturnUndefined, &[]);
        builder.push(Op::ReturnUndefined, &[]);
        builder.push(Op::ReturnUndefined, &[]);
        let inner = ExceptionHandler {
            start: 1,
            end: 2,
            target: 3,
            exception: 0,
        };
        let outer = ExceptionHandler {
            start: 0,
            end: 3,
            target: 4,
            exception: 1,
        };
        let cfg = CodeBlockControlFlow::from_verified_wordcode(&builder.finish(), &[inner, outer]);

        assert_eq!(cfg.handler_at(0), Some(outer));
        assert_eq!(cfg.handler_at(1), Some(inner));
        assert_eq!(cfg.handler_at(2), Some(outer));
        assert_eq!(cfg.handler_at(3), None);
        assert_eq!(cfg.block_starts(), &[0, 3, 4]);
    }
}
