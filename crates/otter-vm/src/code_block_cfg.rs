//! Precomputed logical control-flow metadata for executable CodeBlocks.
//!
//! # Contents
//! - [`CodeBlockControlFlow`] — immutable basic-block, loop-header, and
//!   exception-handler tables built from verified schema wordcode and the
//!   function's handler table.
//! - [`ControlFlowLayout`] / [`ControlFlowBuilder`] — the shared count and fill passes.
//! - [`CodeBlockControlFlowView`] — borrowed read-only access for JIT consumers.
//!
//! # Invariants
//! - Every PC is a canonical instruction index, never a serialized byte PC.
//! - Target decoding stays in executable preparation; dispatch and JIT
//!   lowering do not reinterpret relative branch operands.
//! - Handler entries keep the verified table order, innermost first, so the
//!   first entry covering a PC is the handler a throw there lands in. Every
//!   handler target starts a basic block.
//! - Fresh isolate copies prepare each fixed array fallibly against the owning
//!   CodeBlock lease; allocator refusal leaves the original tables unchanged.
//!
//! # See also
//! - [`crate::CodeBlock`]
//! - [`otter_bytecode::ExceptionHandler`]
//! - [`otter_bytecode::opcode_schema`]

use otter_bytecode::{
    ExceptionHandler, FunctionCode, Op, Operand,
    opcode_schema::{ControlFlow, SuccessorSpec, opcode_schema},
};

use otter_resource::{ResourceError, ResourceLease};

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

/// Capacity of the logical flow tables derived from admitted instructions.
#[derive(Clone, Copy)]
pub(crate) struct ControlFlowLayout {
    instruction_count: u32,
    block_starts: usize,
    loop_latches: usize,
    handlers: usize,
}

impl ControlFlowLayout {
    pub(crate) fn new(instructions: usize, handlers: usize) -> Self {
        Self {
            instruction_count: instructions as u32,
            block_starts: usize::from(instructions != 0).saturating_add(handlers),
            loop_latches: 0,
            handlers,
        }
    }

    pub(crate) fn count(&mut self, code: &FunctionCode, index: usize, op: Op) {
        let (blocks, latches) = (&mut self.block_starts, &mut self.loop_latches);
        visit_boundaries(
            code,
            index,
            op,
            self.instruction_count,
            |_| *blocks = blocks.saturating_add(1),
            |_, _| *latches = latches.saturating_add(1),
        );
    }

    pub(crate) fn bytes(self) -> u64 {
        use super::allocation::array_bytes;
        array_bytes::<u32>(self.block_starts)
            .saturating_add(array_bytes::<u32>(self.loop_latches))
            .saturating_add(array_bytes::<(u32, u32)>(self.loop_latches))
            .saturating_add(array_bytes::<ExceptionHandler>(self.handlers))
    }
}

/// Fills the admitted tables in logical order, then compacts their duplicates.
pub(crate) struct ControlFlowBuilder {
    layout: ControlFlowLayout,
    block_starts: Vec<u32>,
    loop_latches: Vec<(u32, u32)>,
}

impl ControlFlowBuilder {
    pub(crate) fn new(
        layout: ControlFlowLayout,
        lease: &mut ResourceLease,
    ) -> Result<Self, ResourceError> {
        use super::allocation::try_vec;
        let mut block_starts = try_vec(layout.block_starts, lease)?;
        let loop_latches = try_vec(layout.loop_latches, lease)?;
        if layout.instruction_count != 0 {
            block_starts.push(0);
        }
        Ok(Self {
            layout,
            block_starts,
            loop_latches,
        })
    }

    pub(crate) fn record(&mut self, code: &FunctionCode, index: usize, op: Op) {
        let (blocks, latches) = (&mut self.block_starts, &mut self.loop_latches);
        visit_boundaries(
            code,
            index,
            op,
            self.layout.instruction_count,
            |pc| blocks.push(pc),
            |target, pc| latches.push((target, pc)),
        );
    }

    pub(crate) fn finish(
        self,
        handlers: &[ExceptionHandler],
        lease: &mut ResourceLease,
    ) -> Result<CodeBlockControlFlow, ResourceError> {
        use super::allocation::try_vec;
        let Self {
            layout,
            mut block_starts,
            mut loop_latches,
        } = self;
        block_starts.extend(handlers.iter().map(|handler| handler.target));
        block_starts.sort_unstable();
        block_starts.dedup();
        loop_latches.sort_unstable();
        // Sort puts every header's last backedge last. Compact in-place, so
        // there is no second transient map/set or hidden fallible allocation.
        let mut written = 0;
        for index in 0..loop_latches.len() {
            let (header, latch) = loop_latches[index];
            if written != 0 && loop_latches[written - 1].0 == header {
                loop_latches[written - 1].1 = latch;
            } else {
                loop_latches[written] = (header, latch);
                written += 1;
            }
        }
        loop_latches.truncate(written);
        let mut loop_headers = try_vec(layout.loop_latches, lease)?;
        loop_headers.extend(loop_latches.iter().map(|&(header, _)| header));
        Ok(CodeBlockControlFlow {
            block_starts: block_starts.into_boxed_slice(),
            loop_headers: loop_headers.into_boxed_slice(),
            loop_latches: loop_latches.into_boxed_slice(),
            handlers: super::allocation::try_copy(handlers, lease)?,
        })
    }
}

/// Immutable logical-PC tables shared by interpreter and JIT consumers.
#[derive(Debug, PartialEq, Eq)]
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

    /// Copy fixed admitted tables into a different isolate without a new CFG walk.
    pub(crate) fn fresh_copy(
        &self,
        lease: &mut otter_resource::ResourceLease,
    ) -> Result<Self, otter_resource::ResourceError> {
        use super::allocation::try_copy;
        Ok(Self {
            block_starts: try_copy(&self.block_starts, lease)?,
            loop_headers: try_copy(&self.loop_headers, lease)?,
            loop_latches: try_copy(&self.loop_latches, lease)?,
            handlers: try_copy(&self.handlers, lease)?,
        })
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

fn visit_boundaries(
    code: &FunctionCode,
    index: usize,
    op: Op,
    instruction_count: u32,
    mut block: impl FnMut(u32),
    mut latch: impl FnMut(u32, u32),
) {
    let pc = index as u32;
    let schema = opcode_schema(op);
    for successor in schema.successor_shape.exact() {
        if let SuccessorSpec::RelativeTarget { operand_index, .. } = successor {
            let target = relative_target(code, index, *operand_index);
            if target < instruction_count {
                block(target);
            }
            if target < pc {
                latch(target, pc);
            }
        }
    }
    if pc + 1 < instruction_count
        && !matches!(
            schema.control_flow,
            ControlFlow::Fallthrough | ControlFlow::Call
        )
    {
        block(pc + 1);
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

    fn cfg(code: &FunctionCode, handlers: &[ExceptionHandler]) -> CodeBlockControlFlow {
        let mut layout = ControlFlowLayout::new(code.len(), handlers.len());
        for (index, instruction) in code.iter().enumerate() {
            layout.count(code, index, instruction.op);
        }
        let mut lease = otter_resource::ResourceAccount::default()
            .reserve_exact(
                otter_resource::ResourceClass::SourceModuleBytes,
                layout.bytes(),
            )
            .expect("admit control-flow fixture geometry");
        let mut builder =
            ControlFlowBuilder::new(layout, &mut lease).expect("prepare control-flow fixture");
        for (index, instruction) in code.iter().enumerate() {
            builder.record(code, index, instruction.op);
        }
        builder
            .finish(handlers, &mut lease)
            .expect("prepare control-flow fixture")
    }

    use otter_bytecode::{FunctionCodeBuilder, Op, Operand};

    #[test]
    fn derives_blocks_and_loop_headers_from_schema_successors() {
        let mut builder = FunctionCodeBuilder::new();
        builder.push(Op::JumpIfFalse, &[Operand::Imm32(2), Operand::Register(0)]);
        builder.push(Op::Nop, &[]);
        builder.push(Op::Jump, &[Operand::Imm32(-3)]);
        builder.push(Op::ReturnUndefined, &[]);
        let cfg = cfg(&builder.finish(), &[]);

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
        let cfg = cfg(&builder.finish(), &[inner, outer]);

        assert_eq!(cfg.handler_at(0), Some(outer));
        assert_eq!(cfg.handler_at(1), Some(inner));
        assert_eq!(cfg.handler_at(2), Some(outer));
        assert_eq!(cfg.handler_at(3), None);
        assert_eq!(cfg.block_starts(), &[0, 3, 4]);
    }
}
