//! Precomputed logical control-flow metadata for executable CodeBlocks.
//!
//! # Contents
//! - [`CodeBlockControlFlow`] — immutable basic-block, loop-header, and
//!   exception-region tables built from verified schema wordcode.
//! - [`CodeBlockExceptionRegion`] — resolved handler PCs for one `EnterTry`.
//! - [`CodeBlockControlFlowView`] — borrowed read-only access for JIT consumers.
//! - [`ActiveCatchRegions`] — exact runtime handler-stack reconstruction for
//!   catch-only regions.
//!
//! # Invariants
//! - Every PC is a canonical instruction index, never a serialized byte PC.
//! - Targets are resolved once after wordcode verification; dispatch and JIT
//!   lowering do not reinterpret relative branch or handler operands.
//! - Tables are sorted and duplicate-free. Exception regions are keyed by the
//!   `EnterTry` instruction PC and end at the matching `LeaveTry` PC.
//! - Static handler reconstruction supports catch-only regions. A `finally` or
//!   catchless active region is rejected instead of manufacturing incomplete
//!   completion state.
//!
//! # See also
//! - [`crate::CodeBlock`]
//! - [`otter_bytecode::opcode_schema`]

use std::collections::{BTreeMap, BTreeSet};

use otter_bytecode::{
    FunctionCode, NO_HANDLER_OFFSET, Op, Operand,
    opcode_schema::{ControlFlow, ExceptionSuccessorSpec, SuccessorSpec, opcode_schema},
};

/// Resolved static handlers installed by one `EnterTry` instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeBlockExceptionRegion {
    /// PC of the `EnterTry` instruction owning this region.
    pub enter_pc: u32,
    /// PC of the matching `LeaveTry`. The protected body ends before this
    /// instruction, but a frame resuming *at* this PC still owns the handler:
    /// executing `LeaveTry` is what removes it from the runtime stack.
    pub end_pc: u32,
    /// Catch entry PC, absent for `try/finally` without a catch.
    pub catch_pc: Option<u32>,
    /// Finally entry PC, absent when the region has no finally clause.
    pub finally_pc: Option<u32>,
    /// Register receiving the thrown value at the catch entry.
    pub exception_register: u16,
}

/// Why an exact resume PC cannot be reconstructed from static catch metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveCatchRegionError {
    /// The active region owns a `finally` continuation. Reconstructing it also
    /// needs the dynamic completion stack, which exact catch-only deopt does
    /// not synthesize.
    Finally {
        /// PC of the `EnterTry` owning the unsupported region.
        enter_pc: u32,
    },
    /// The active region has no catch target. Such a region is necessarily a
    /// catchless completion handler and cannot be represented as a catch-only
    /// [`crate::TryHandler`] stack entry.
    Catchless {
        /// PC of the `EnterTry` owning the unsupported region.
        enter_pc: u32,
    },
}

/// Outer-to-inner iterator over catch-only handlers active at one exact PC.
///
/// Construction validates every lexically active region before iteration, so
/// consumers never observe a partial prefix followed by an unsupported
/// `finally`/catchless region.
#[derive(Debug, Clone)]
pub struct ActiveCatchRegions<'a> {
    regions: std::slice::Iter<'a, CodeBlockExceptionRegion>,
    resume_pc: u32,
}

impl Iterator for ActiveCatchRegions<'_> {
    type Item = CodeBlockExceptionRegion;

    fn next(&mut self) -> Option<Self::Item> {
        self.regions
            .find(|region| region_is_active_at(region, self.resume_pc))
            .copied()
    }
}

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

    /// Exception regions in ascending `EnterTry` PC order.
    #[must_use]
    pub fn exception_regions(self) -> &'a [CodeBlockExceptionRegion] {
        self.control_flow.exception_regions()
    }

    /// Region installed by the `EnterTry` at `enter_pc`.
    #[must_use]
    pub fn exception_region(self, enter_pc: u32) -> Option<CodeBlockExceptionRegion> {
        self.control_flow.exception_region(enter_pc)
    }

    /// Innermost region whose protected body contains `pc`.
    #[must_use]
    pub fn enclosing_exception_region(self, pc: u32) -> Option<CodeBlockExceptionRegion> {
        self.control_flow.enclosing_exception_region(pc)
    }

    /// Catch-only handler stack active immediately before `resume_pc` runs.
    ///
    /// Results are ordered outermost to innermost, matching runtime push
    /// order. `EnterTry` itself is excluded because it has not executed yet;
    /// its matching `LeaveTry` is included because that instruction has not
    /// popped the handler yet. PCs after `LeaveTry`, including the catch body,
    /// exclude the region.
    pub fn active_catch_regions(
        self,
        resume_pc: u32,
    ) -> Result<ActiveCatchRegions<'a>, ActiveCatchRegionError> {
        self.control_flow.active_catch_regions(resume_pc)
    }
}

/// Immutable logical-PC tables shared by interpreter and JIT consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodeBlockControlFlow {
    block_starts: Box<[u32]>,
    loop_headers: Box<[u32]>,
    loop_latches: Box<[(u32, u32)]>,
    exception_regions: Box<[CodeBlockExceptionRegion]>,
}

impl CodeBlockControlFlow {
    /// Build tables from wordcode that has already passed schema verification.
    pub(crate) fn from_verified_wordcode(code: &FunctionCode) -> Self {
        let mut block_starts = BTreeSet::new();
        let mut loop_latches = BTreeMap::<u32, u32>::new();
        let mut exception_regions = Vec::new();
        let mut open_exception_regions = Vec::new();
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
                    let target = relative_target(code, index, *operand_index, None);
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

            if instruction.op == Op::EnterTry {
                let mut handlers = schema.exception_successor_shape.exact().iter();
                let catch_pc = optional_exception_target(code, index, handlers.next());
                let finally_pc = optional_exception_target(code, index, handlers.next());
                let Some(Operand::Register(exception_register)) = code.operand(instruction, 2)
                else {
                    unreachable!("verified EnterTry exception register")
                };
                if let Some(target) = catch_pc
                    && target < instruction_count
                {
                    block_starts.insert(target);
                }
                if let Some(target) = finally_pc
                    && target < instruction_count
                {
                    block_starts.insert(target);
                }
                exception_regions.push(CodeBlockExceptionRegion {
                    enter_pc: pc,
                    end_pc: instruction_count,
                    catch_pc,
                    finally_pc,
                    exception_register,
                });
                open_exception_regions.push(exception_regions.len() - 1);
            } else if instruction.op == Op::LeaveTry
                && let Some(region_index) = open_exception_regions.pop()
            {
                exception_regions[region_index].end_pc = pc;
            }
        }

        let loop_headers = loop_latches.keys().copied().collect();
        Self {
            block_starts: block_starts.into_iter().collect(),
            loop_headers,
            loop_latches: loop_latches.into_iter().collect(),
            exception_regions: exception_regions.into_boxed_slice(),
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

    pub(crate) fn exception_region(&self, enter_pc: u32) -> Option<CodeBlockExceptionRegion> {
        self.exception_regions
            .binary_search_by_key(&enter_pc, |region| region.enter_pc)
            .ok()
            .map(|index| self.exception_regions[index])
    }

    fn exception_regions(&self) -> &[CodeBlockExceptionRegion] {
        &self.exception_regions
    }

    fn enclosing_exception_region(&self, pc: u32) -> Option<CodeBlockExceptionRegion> {
        self.exception_regions
            .iter()
            .rev()
            .copied()
            .find(|region| region.enter_pc < pc && pc < region.end_pc)
    }

    fn active_catch_regions(
        &self,
        resume_pc: u32,
    ) -> Result<ActiveCatchRegions<'_>, ActiveCatchRegionError> {
        for region in self
            .exception_regions
            .iter()
            .filter(|region| region_is_active_at(region, resume_pc))
        {
            if region.finally_pc.is_some() {
                return Err(ActiveCatchRegionError::Finally {
                    enter_pc: region.enter_pc,
                });
            }
            if region.catch_pc.is_none() {
                return Err(ActiveCatchRegionError::Catchless {
                    enter_pc: region.enter_pc,
                });
            }
        }
        Ok(ActiveCatchRegions {
            regions: self.exception_regions.iter(),
            resume_pc,
        })
    }
}

#[inline]
fn region_is_active_at(region: &CodeBlockExceptionRegion, resume_pc: u32) -> bool {
    region.enter_pc < resume_pc && resume_pc <= region.end_pc
}

fn optional_exception_target(
    code: &FunctionCode,
    instruction_index: usize,
    successor: Option<&ExceptionSuccessorSpec>,
) -> Option<u32> {
    let Some(ExceptionSuccessorSpec::OptionalRelativeTarget {
        operand_index,
        absent_value,
        ..
    }) = successor
    else {
        unreachable!("EnterTry schema owns two optional handler targets")
    };
    let target = relative_target(code, instruction_index, *operand_index, Some(*absent_value));
    (target != u32::MAX).then_some(target)
}

fn relative_target(
    code: &FunctionCode,
    instruction_index: usize,
    operand_index: usize,
    absent_value: Option<i32>,
) -> u32 {
    let instruction = &code[instruction_index];
    let Some(Operand::Imm32(delta)) = code.operand(instruction, operand_index) else {
        unreachable!("verified relative target operand")
    };
    if absent_value == Some(delta) || delta == NO_HANDLER_OFFSET {
        return u32::MAX;
    }
    let target = instruction_index as i64 + 1 + i64::from(delta);
    u32::try_from(target).expect("verified relative target is non-negative and in range")
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::{FunctionCodeBuilder, Operand};

    #[test]
    fn derives_blocks_and_loop_headers_from_schema_successors() {
        let mut builder = FunctionCodeBuilder::new();
        builder.push(Op::JumpIfFalse, &[Operand::Imm32(2), Operand::Register(0)]);
        builder.push(Op::Nop, &[]);
        builder.push(Op::Jump, &[Operand::Imm32(-3)]);
        builder.push(Op::ReturnUndefined, &[]);
        let cfg = CodeBlockControlFlow::from_verified_wordcode(&builder.finish());

        assert_eq!(cfg.block_starts(), &[0, 1, 3]);
        assert_eq!(cfg.loop_headers(), &[0]);
        assert_eq!(cfg.loop_latch(0), Some(2));
    }

    #[test]
    fn resolves_enter_try_handlers_once() {
        let mut builder = FunctionCodeBuilder::new();
        builder.push(
            Op::EnterTry,
            &[
                Operand::Imm32(1),
                Operand::Imm32(NO_HANDLER_OFFSET),
                Operand::Register(3),
            ],
        );
        builder.push(Op::LeaveTry, &[]);
        builder.push(Op::ReturnUndefined, &[]);
        let cfg = CodeBlockControlFlow::from_verified_wordcode(&builder.finish());

        assert_eq!(
            cfg.exception_region(0),
            Some(CodeBlockExceptionRegion {
                enter_pc: 0,
                end_pc: 1,
                catch_pc: Some(2),
                finally_pc: None,
                exception_register: 3,
            })
        );
        assert_eq!(cfg.exception_region(1), None);
        assert_eq!(cfg.block_starts(), &[0, 1, 2]);
        assert_eq!(cfg.enclosing_exception_region(0), None);
        assert_eq!(cfg.enclosing_exception_region(1), None);
    }

    #[test]
    fn matches_nested_exception_region_ranges() {
        let mut builder = FunctionCodeBuilder::new();
        builder.push(
            Op::EnterTry,
            &[
                Operand::Imm32(NO_HANDLER_OFFSET),
                Operand::Imm32(6),
                Operand::Register(0),
            ],
        );
        builder.push(
            Op::EnterTry,
            &[
                Operand::Imm32(3),
                Operand::Imm32(NO_HANDLER_OFFSET),
                Operand::Register(0),
            ],
        );
        builder.push(Op::Nop, &[]);
        builder.push(Op::LeaveTry, &[]);
        builder.push(Op::Jump, &[Operand::Imm32(1)]);
        builder.push(Op::Nop, &[]);
        builder.push(Op::LeaveTry, &[]);
        builder.push(Op::Nop, &[]);
        builder.push(Op::EndFinally, &[]);
        builder.push(Op::ReturnUndefined, &[]);
        let cfg = CodeBlockControlFlow::from_verified_wordcode(&builder.finish());

        assert_eq!(cfg.exception_regions[0].end_pc, 6);
        assert_eq!(cfg.exception_regions[1].end_pc, 3);
        assert_eq!(cfg.enclosing_exception_region(2).unwrap().enter_pc, 1);
        assert_eq!(cfg.enclosing_exception_region(5).unwrap().enter_pc, 0);
        assert_eq!(cfg.enclosing_exception_region(6), None);
    }

    #[test]
    fn reconstructs_nested_catch_handlers_at_exact_runtime_boundaries() {
        let mut builder = FunctionCodeBuilder::new();
        builder.push(
            Op::EnterTry,
            &[
                Operand::Imm32(9),
                Operand::Imm32(NO_HANDLER_OFFSET),
                Operand::Register(0),
            ],
        );
        builder.push(Op::Nop, &[]);
        builder.push(
            Op::EnterTry,
            &[
                Operand::Imm32(3),
                Operand::Imm32(NO_HANDLER_OFFSET),
                Operand::Register(1),
            ],
        );
        builder.push(Op::Nop, &[]);
        builder.push(Op::LeaveTry, &[]);
        builder.push(Op::Jump, &[Operand::Imm32(1)]);
        builder.push(Op::Nop, &[]); // inner catch body
        builder.push(Op::Nop, &[]); // after inner try/catch, still in outer try
        builder.push(Op::LeaveTry, &[]);
        builder.push(Op::Jump, &[Operand::Imm32(1)]);
        builder.push(Op::Nop, &[]); // outer catch body
        builder.push(Op::ReturnUndefined, &[]);
        let cfg = CodeBlockControlFlow::from_verified_wordcode(&builder.finish());

        let enters = |pc| {
            cfg.active_catch_regions(pc)
                .expect("catch-only region")
                .map(|region| region.enter_pc)
                .collect::<Vec<_>>()
        };

        assert_eq!(enters(0), Vec::<u32>::new()); // outer EnterTry has not executed
        assert_eq!(enters(1), &[0]);
        assert_eq!(enters(2), &[0]); // inner EnterTry has not executed
        assert_eq!(enters(3), &[0, 2]);
        assert_eq!(enters(4), &[0, 2]); // inner LeaveTry has not executed
        assert_eq!(enters(5), &[0]); // after inner LeaveTry
        assert_eq!(enters(6), &[0]); // inner catch body excludes its handler
        assert_eq!(enters(8), &[0]); // outer LeaveTry has not executed
        assert_eq!(enters(9), Vec::<u32>::new()); // after outer LeaveTry
        assert_eq!(enters(10), Vec::<u32>::new()); // outer catch body
    }

    #[test]
    fn rejects_active_finally_and_catchless_regions_before_iteration() {
        let mut finally_builder = FunctionCodeBuilder::new();
        finally_builder.push(
            Op::EnterTry,
            &[
                Operand::Imm32(NO_HANDLER_OFFSET),
                Operand::Imm32(2),
                Operand::Register(0),
            ],
        );
        finally_builder.push(Op::Nop, &[]);
        finally_builder.push(Op::LeaveTry, &[]);
        finally_builder.push(Op::EndFinally, &[]);
        let finally_cfg = CodeBlockControlFlow::from_verified_wordcode(&finally_builder.finish());
        assert_eq!(
            finally_cfg.active_catch_regions(1).unwrap_err(),
            ActiveCatchRegionError::Finally { enter_pc: 0 }
        );

        // A malformed/catchless static region is rejected explicitly rather
        // than yielding an incomplete handler. The verified compiler never
        // emits this shape, but the control-flow API remains total for a
        // hand-built FunctionCode fixture.
        let mut catchless_builder = FunctionCodeBuilder::new();
        catchless_builder.push(
            Op::EnterTry,
            &[
                Operand::Imm32(NO_HANDLER_OFFSET),
                Operand::Imm32(NO_HANDLER_OFFSET),
                Operand::Register(0),
            ],
        );
        catchless_builder.push(Op::Nop, &[]);
        catchless_builder.push(Op::LeaveTry, &[]);
        let catchless_cfg =
            CodeBlockControlFlow::from_verified_wordcode(&catchless_builder.finish());
        assert_eq!(
            catchless_cfg.active_catch_regions(1).unwrap_err(),
            ActiveCatchRegionError::Catchless { enter_pc: 0 }
        );
    }
}
