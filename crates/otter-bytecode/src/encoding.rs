//! Bytecode wire format: byte-stream encoder, decoder, source-map
//! and jump-offset helpers.
//!
//! # Contents
//! - panic-free validation for logical DTOs and authoritative wordcode
//! - bounded handler/abrupt-completion control-flow analysis
//! - cold byte-stream encoding, decoding, and branch fixups
//! - source-map and serialized-layout helpers
//!
//! # Invariants
//! - Authoritative wordcode is validated before executable consumers inspect it.
//! - Runtime handler stacks agree with the lexical `EnterTry`/`LeaveTry`
//!   regions at every reachable instruction and every control-flow join.
//! - Parked-finally state distinguishes normal from abrupt completion. Joins
//!   union those finite kinds, while abrupt destinations remain explicit
//!   bounded summary edges.
//! - Abrupt edges use the same handler floors and finally ordering as the
//!   interpreter; validation stores at most linear state plus discovered edges.
//!
//! Encodes the compiler's [`Instruction`] DTO stream into a self-describing
//! byte buffer retained for cold metadata, diagnostics, and serialization.
//! Active VM dispatch executes schema-typed CodeBlock words instead. Logical
//! instruction-index DTOs are verified directly before encoding; decoding a
//! serialized stream independently validates byte-boundary branch and handler
//! targets. Negative compiler/backend fixtures may measure canonical wordcode
//! size without validating their deliberately malformed targets. Bytecode is
//! never persisted across incompatible versions, so no wire-format version is
//! carried in the stream.
//!
//! # See also
//! - [`crate::opcode_schema`] for operand and successor authority.
//! - `otter-vm`'s `code_block_cfg` and frame handler operations for consumers.
//!
//! # Wire format (per instruction)
//!
//! ```text
//! instruction    := opcode operand_count operand*
//! opcode         := u8                (Op as u8)
//! operand_count  := u8
//! operand        := operand_kind operand_bytes
//! operand_kind   := u8                (OPERAND_KIND_REGISTER | _CONST_INDEX | _IMM32)
//! operand_bytes  :=
//!     Register:    u16 little-endian
//!     ConstIndex:  u32 little-endian
//!     Imm32:       i32 little-endian
//! ```

use std::collections::{HashMap, VecDeque};

use crate::{
    FunctionCode, Instruction, NO_HANDLER_OFFSET, Op, Operand, SpanEntry,
    opcode_schema::{
        ExceptionSuccessorSpec, OperandKind, OperandShapeError, RelativeTargetBase, SuccessorSpec,
        decode_operand_word, opcode_schema, operand_kind_at, verify_operand_shape,
    },
    wordcode::{INLINE_OPERAND_WORDS, Instruction as WordInstruction},
};

/// Verify authoritative execution wordcode directly in logical-PC space.
///
/// # Errors
/// Returns [`VerifyError`] for malformed/noncanonical operand storage,
/// operand-shape, branch-target, handler-target, or finally-floor violations.
pub fn verify_wordcode_function(code: &FunctionCode) -> Result<(), VerifyError> {
    let len = i64::try_from(code.len()).map_err(|_| VerifyError::FunctionTooLarge)?;
    let (instructions, overflow_operand_words) = code.raw_parts();
    let mut next_overflow_offset = 0;
    let mut decoded_operands = Vec::with_capacity(instructions.len());
    for (instruction_index, instr) in instructions.iter().enumerate() {
        let operands = decode_wordcode_operands(
            instr,
            overflow_operand_words,
            &mut next_overflow_offset,
            instruction_index,
        )?;
        verify_operand_shape(instr.op, &operands).map_err(|error| {
            VerifyError::InvalidOperandShape {
                instruction_index,
                error,
            }
        })?;
        let schema = opcode_schema(instr.op);
        for successor in schema.successor_shape.exact() {
            if let SuccessorSpec::RelativeTarget { operand_index, .. } = successor {
                verify_wordcode_target(&operands, instruction_index, *operand_index, None, len)?;
            }
        }
        for successor in schema.exception_successor_shape.exact() {
            match successor {
                ExceptionSuccessorSpec::OptionalRelativeTarget {
                    operand_index,
                    absent_value,
                    ..
                } => verify_wordcode_target(
                    &operands,
                    instruction_index,
                    *operand_index,
                    Some(*absent_value),
                    len,
                )?,
                ExceptionSuccessorSpec::RunFinallyHandlersToFloor {
                    floor_operand_index,
                } => {
                    let floor = checked_wordcode_imm32_operand(
                        &operands,
                        instruction_index,
                        *floor_operand_index,
                    )?;
                    if floor < 0 {
                        return Err(VerifyError::InvalidControlFlowOperand {
                            instruction_index,
                            operand_index: *floor_operand_index,
                            value: floor,
                        });
                    }
                }
                ExceptionSuccessorSpec::DynamicFrameHandlerOrCaller
                | ExceptionSuccessorSpec::CallerHandlerOrUncaught
                | ExceptionSuccessorSpec::ResumeParkedAbruptCompletion
                | ExceptionSuccessorSpec::RunFinallyHandlersToFrameReturn => {}
            }
        }
        decoded_operands.push(operands.into_boxed_slice());
    }
    verify_wordcode_overflow_consumed(next_overflow_offset, overflow_operand_words.len())?;
    verify_wordcode_control_flow(instructions, &decoded_operands)?;
    Ok(())
}

fn verify_wordcode_target(
    operands: &[Operand],
    instruction_index: usize,
    operand_index: usize,
    absent_value: Option<i32>,
    instruction_count: i64,
) -> Result<(), VerifyError> {
    resolve_wordcode_target(
        operands,
        instruction_index,
        operand_index,
        absent_value,
        instruction_count,
    )?;
    Ok(())
}

fn resolve_wordcode_target(
    operands: &[Operand],
    instruction_index: usize,
    operand_index: usize,
    absent_value: Option<i32>,
    instruction_count: i64,
) -> Result<Option<usize>, VerifyError> {
    let delta = checked_wordcode_imm32_operand(operands, instruction_index, operand_index)?;
    if absent_value == Some(delta) {
        return Ok(None);
    }
    let base = i64::try_from(instruction_index)
        .map_err(|_| VerifyError::FunctionTooLarge)?
        .checked_add(1)
        .ok_or(VerifyError::FunctionTooLarge)?;
    let target = base
        .checked_add(i64::from(delta))
        .ok_or(VerifyError::FunctionTooLarge)?;
    if !(0..=instruction_count).contains(&target) {
        return Err(VerifyError::InvalidControlFlowTarget {
            instruction_index,
            target,
        });
    }
    let target = usize::try_from(target).map_err(|_| VerifyError::FunctionTooLarge)?;
    Ok(Some(target))
}

fn checked_wordcode_imm32_operand(
    operands: &[Operand],
    instruction_index: usize,
    operand_index: usize,
) -> Result<i32, VerifyError> {
    match operands.get(operand_index).copied() {
        Some(Operand::Imm32(value)) => Ok(value),
        Some(operand) => Err(VerifyError::InvalidOperandShape {
            instruction_index,
            error: OperandShapeError::Kind {
                index: operand_index,
                expected: OperandKind::Imm32,
                actual: OperandKind::of(&operand),
            },
        }),
        None => Err(VerifyError::InvalidOperandShape {
            instruction_index,
            error: OperandShapeError::Count {
                expected: operand_index.saturating_add(1),
                actual: operands.len(),
            },
        }),
    }
}

fn decode_wordcode_operands(
    instruction: &WordInstruction,
    overflow_operand_words: &[u32],
    next_overflow_offset: &mut usize,
    instruction_index: usize,
) -> Result<Vec<Operand>, VerifyError> {
    let (op, raw_operand_count, inline_operand_words, raw_overflow_offset) =
        instruction.raw_parts();
    let operand_count = usize::from(raw_operand_count);
    let operand_words = if operand_count <= INLINE_OPERAND_WORDS {
        if raw_overflow_offset != u32::MAX {
            return Err(VerifyError::InvalidOperandStorage {
                instruction_index,
                error: OperandStorageError::InlineUsesOverflow {
                    operand_count: raw_operand_count,
                    overflow_offset: raw_overflow_offset,
                },
            });
        }
        for (word_index, word) in inline_operand_words
            .iter()
            .copied()
            .enumerate()
            .skip(operand_count)
        {
            if word != 0 {
                return Err(VerifyError::InvalidOperandStorage {
                    instruction_index,
                    error: OperandStorageError::NonZeroInlinePadding { word_index, word },
                });
            }
        }
        &inline_operand_words[..operand_count]
    } else {
        if raw_overflow_offset == u32::MAX {
            return Err(VerifyError::InvalidOperandStorage {
                instruction_index,
                error: OperandStorageError::MissingOverflow {
                    operand_count: raw_operand_count,
                },
            });
        }
        for (word_index, word) in inline_operand_words.iter().copied().enumerate() {
            if word != 0 {
                return Err(VerifyError::InvalidOperandStorage {
                    instruction_index,
                    error: OperandStorageError::NonZeroInlinePadding { word_index, word },
                });
            }
        }
        let overflow_offset = usize::try_from(raw_overflow_offset).map_err(|_| {
            VerifyError::InvalidOperandStorage {
                instruction_index,
                error: OperandStorageError::OverflowOutOfBounds {
                    overflow_offset: raw_overflow_offset,
                    operand_count: raw_operand_count,
                    available_words: overflow_operand_words.len(),
                },
            }
        })?;
        let overflow_end = overflow_offset.checked_add(operand_count).ok_or(
            VerifyError::InvalidOperandStorage {
                instruction_index,
                error: OperandStorageError::OverflowOutOfBounds {
                    overflow_offset: raw_overflow_offset,
                    operand_count: raw_operand_count,
                    available_words: overflow_operand_words.len(),
                },
            },
        )?;
        if overflow_end > overflow_operand_words.len() {
            return Err(VerifyError::InvalidOperandStorage {
                instruction_index,
                error: OperandStorageError::OverflowOutOfBounds {
                    overflow_offset: raw_overflow_offset,
                    operand_count: raw_operand_count,
                    available_words: overflow_operand_words.len(),
                },
            });
        }
        if overflow_offset != *next_overflow_offset {
            return Err(VerifyError::InvalidOperandStorage {
                instruction_index,
                error: OperandStorageError::NonCanonicalOverflowOffset {
                    expected: *next_overflow_offset,
                    actual: raw_overflow_offset,
                },
            });
        }
        *next_overflow_offset = overflow_end;
        &overflow_operand_words[overflow_offset..overflow_end]
    };

    if let Some(expected) = opcode_schema(op).operand_shape.fixed()
        && operand_count != expected.len()
    {
        return Err(VerifyError::InvalidOperandShape {
            instruction_index,
            error: OperandShapeError::Count {
                expected: expected.len(),
                actual: operand_count,
            },
        });
    }

    let mut operands = Vec::with_capacity(operand_count);
    for (operand_index, word) in operand_words.iter().copied().enumerate() {
        let Some(kind) = operand_kind_at(op, operand_index) else {
            let expected = opcode_schema(op)
                .operand_shape
                .prefix()
                .map_or(0, |prefix| prefix.len());
            return Err(VerifyError::InvalidOperandShape {
                instruction_index,
                error: OperandShapeError::Count {
                    expected,
                    actual: operand_count,
                },
            });
        };
        let Some(operand) = decode_operand_word(kind, word) else {
            return Err(VerifyError::InvalidOperandStorage {
                instruction_index,
                error: OperandStorageError::InvalidOperandWord {
                    operand_index,
                    expected: kind,
                    word,
                },
            });
        };
        operands.push(operand);
    }
    Ok(operands)
}

fn verify_wordcode_overflow_consumed(
    consumed_words: usize,
    total_words: usize,
) -> Result<(), VerifyError> {
    if consumed_words == total_words {
        return Ok(());
    }
    Err(VerifyError::TrailingOverflowOperandWords {
        first_unused_word: consumed_words,
        total_words,
    })
}

#[derive(Debug, Clone, Copy)]
struct HandlerRegion {
    enter_instruction_index: usize,
    parent: Option<usize>,
    depth: usize,
    catch_target: Option<usize>,
    finally_target: Option<usize>,
}

struct HandlerLayout {
    regions: Vec<HandlerRegion>,
    enter_regions: Vec<Option<usize>>,
    leave_regions: Vec<Option<usize>>,
    lexical_tops: Vec<Option<usize>>,
}

impl HandlerLayout {
    fn build(
        instructions: &[WordInstruction],
        operands: &[Box<[Operand]>],
    ) -> Result<Self, VerifyError> {
        let instruction_count =
            i64::try_from(instructions.len()).map_err(|_| VerifyError::FunctionTooLarge)?;
        let mut regions = Vec::new();
        let mut enter_regions = vec![None; instructions.len()];
        let mut leave_regions = vec![None; instructions.len()];
        let mut lexical_tops = vec![None; instructions.len().saturating_add(1)];
        let mut open_regions = Vec::new();

        for (instruction_index, instruction) in instructions.iter().enumerate() {
            lexical_tops[instruction_index] = open_regions.last().copied();
            match instruction.op {
                Op::EnterTry => {
                    let catch_target = resolve_wordcode_target(
                        &operands[instruction_index],
                        instruction_index,
                        0,
                        Some(NO_HANDLER_OFFSET),
                        instruction_count,
                    )?;
                    let finally_target = resolve_wordcode_target(
                        &operands[instruction_index],
                        instruction_index,
                        1,
                        Some(NO_HANDLER_OFFSET),
                        instruction_count,
                    )?;
                    if catch_target.is_none() && finally_target.is_none() {
                        return Err(VerifyError::HandlerWithoutTarget { instruction_index });
                    }
                    for (operand_index, target) in [(0, catch_target), (1, finally_target)] {
                        if target == Some(instructions.len()) {
                            return Err(VerifyError::InvalidHandlerTarget {
                                instruction_index,
                                operand_index,
                                target: instructions.len(),
                            });
                        }
                    }
                    let region_index = regions.len();
                    regions.push(HandlerRegion {
                        enter_instruction_index: instruction_index,
                        parent: open_regions.last().copied(),
                        depth: open_regions.len().saturating_add(1),
                        catch_target,
                        finally_target,
                    });
                    enter_regions[instruction_index] = Some(region_index);
                    open_regions.push(region_index);
                }
                Op::LeaveTry => {
                    let Some(region_index) = open_regions.pop() else {
                        return Err(VerifyError::HandlerStackUnderflow { instruction_index });
                    };
                    leave_regions[instruction_index] = Some(region_index);
                }
                _ => {}
            }
        }
        lexical_tops[instructions.len()] = open_regions.last().copied();
        if let Some(region_index) = open_regions.last().copied() {
            return Err(VerifyError::UnclosedHandler {
                enter_instruction_index: regions[region_index].enter_instruction_index,
            });
        }

        let layout = Self {
            regions,
            enter_regions,
            leave_regions,
            lexical_tops,
        };
        for region in &layout.regions {
            for target in [region.catch_target, region.finally_target]
                .into_iter()
                .flatten()
            {
                let actual = layout.lexical_tops[target];
                if actual != region.parent {
                    return Err(VerifyError::InvalidHandlerLandingState {
                        instruction_index: region.enter_instruction_index,
                        target,
                        expected_depth: layout.handler_depth(region.parent),
                        actual_depth: layout.handler_depth(actual),
                    });
                }
            }
        }
        Ok(layout)
    }

    fn handler_depth(&self, top: Option<usize>) -> usize {
        top.map_or(0, |region_index| self.regions[region_index].depth)
    }

    fn enter_instruction_index(&self, top: Option<usize>) -> Option<usize> {
        top.map(|region_index| self.regions[region_index].enter_instruction_index)
    }
}

type ParkedStackId = usize;
const EMPTY_PARKED_STACK: ParkedStackId = 0;
/// Hard ceiling for implicit handler/parked-state steps discovered while
/// expanding abrupt summaries and joining completion states. Normal CFG work
/// remains linear in the instruction stream; this prevents deeply nested
/// hostile regions from manufacturing quadratic verification work.
const MAX_WORDCODE_ABRUPT_TRANSITIONS: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ParkedCompletionKinds(u8);

impl ParkedCompletionKinds {
    const NORMAL: Self = Self(1 << 0);
    const ABRUPT: Self = Self(1 << 1);

    fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    fn contains_normal(self) -> bool {
        self.0 & Self::NORMAL.0 != 0
    }
}

#[derive(Debug, Clone, Copy)]
struct ParkedStackNode {
    parent: ParkedStackId,
    handler: usize,
    completions: ParkedCompletionKinds,
    depth: usize,
}

struct ParkedStacks {
    nodes: Vec<ParkedStackNode>,
    interned: HashMap<(ParkedStackId, usize, ParkedCompletionKinds), ParkedStackId>,
}

impl ParkedStacks {
    fn new() -> Self {
        Self {
            nodes: vec![ParkedStackNode {
                parent: EMPTY_PARKED_STACK,
                handler: usize::MAX,
                completions: ParkedCompletionKinds(0),
                depth: 0,
            }],
            interned: HashMap::new(),
        }
    }

    fn depth(&self, stack: ParkedStackId) -> usize {
        self.nodes[stack].depth
    }

    fn push(
        &mut self,
        stack: ParkedStackId,
        handler: usize,
        completions: ParkedCompletionKinds,
    ) -> ParkedStackId {
        if let Some(existing) = self.interned.get(&(stack, handler, completions)).copied() {
            return existing;
        }
        let id = self.nodes.len();
        self.nodes.push(ParkedStackNode {
            parent: stack,
            handler,
            completions,
            depth: self.nodes[stack].depth.saturating_add(1),
        });
        self.interned.insert((stack, handler, completions), id);
        id
    }

    fn pop(&self, stack: ParkedStackId) -> Option<(ParkedStackId, ParkedCompletionKinds)> {
        (stack != EMPTY_PARKED_STACK).then(|| {
            let node = self.nodes[stack];
            (node.parent, node.completions)
        })
    }

    fn pop_count(&self, mut stack: ParkedStackId, count: usize) -> Option<ParkedStackId> {
        if count > self.depth(stack) {
            return None;
        }
        for _ in 0..count {
            stack = self.nodes[stack].parent;
        }
        Some(stack)
    }

    fn prune_to_handler_depth(
        &self,
        mut stack: ParkedStackId,
        handler_depth: usize,
        layout: &HandlerLayout,
    ) -> (ParkedStackId, usize) {
        let mut pruned = 0;
        while stack != EMPTY_PARKED_STACK {
            let node = self.nodes[stack];
            let parked_at_depth = layout.regions[node.handler].depth.saturating_sub(1);
            if parked_at_depth <= handler_depth {
                break;
            }
            stack = node.parent;
            pruned += 1;
        }
        (stack, pruned)
    }

    /// Merge completion kinds for two stacks with the same handler spine.
    /// Frames are returned top-to-bottom so the caller can charge the complete
    /// traversal before interning the widened state.
    fn merged_frames(
        &self,
        mut first: ParkedStackId,
        mut second: ParkedStackId,
    ) -> Option<Vec<(usize, ParkedCompletionKinds)>> {
        let mut merged = Vec::new();
        while first != EMPTY_PARKED_STACK && second != EMPTY_PARKED_STACK {
            let first_node = self.nodes[first];
            let second_node = self.nodes[second];
            if first_node.handler != second_node.handler {
                return None;
            }
            merged.push((
                first_node.handler,
                first_node.completions.union(second_node.completions),
            ));
            first = first_node.parent;
            second = second_node.parent;
        }
        if first != EMPTY_PARKED_STACK || second != EMPTY_PARKED_STACK {
            return None;
        }
        Some(merged)
    }

    fn intern_merged_frames(
        &mut self,
        frames_top_to_bottom: &[(usize, ParkedCompletionKinds)],
    ) -> ParkedStackId {
        let mut stack = EMPTY_PARKED_STACK;
        for &(handler, completions) in frames_top_to_bottom.iter().rev() {
            stack = self.push(stack, handler, completions);
        }
        stack
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WordcodeFlowState {
    handlers: Option<usize>,
    parked: ParkedStackId,
}

#[derive(Debug, Clone, Copy)]
struct IncomingWordcodeFlowState {
    state: WordcodeFlowState,
    predecessor: Option<usize>,
}

struct WordcodeControlFlowVerifier<'a> {
    instructions: &'a [WordInstruction],
    operands: &'a [Box<[Operand]>],
    layout: HandlerLayout,
    parked_stacks: ParkedStacks,
    incoming: Vec<Option<IncomingWordcodeFlowState>>,
    worklist: VecDeque<usize>,
    instruction_count: i64,
    abrupt_transitions: usize,
}

impl<'a> WordcodeControlFlowVerifier<'a> {
    fn new(
        instructions: &'a [WordInstruction],
        operands: &'a [Box<[Operand]>],
    ) -> Result<Self, VerifyError> {
        let layout = HandlerLayout::build(instructions, operands)?;
        let mut incoming = vec![None; instructions.len().saturating_add(1)];
        incoming[0] = Some(IncomingWordcodeFlowState {
            state: WordcodeFlowState {
                handlers: None,
                parked: EMPTY_PARKED_STACK,
            },
            predecessor: None,
        });
        let mut worklist = VecDeque::new();
        if !instructions.is_empty() {
            worklist.push_back(0);
        }
        Ok(Self {
            instructions,
            operands,
            layout,
            parked_stacks: ParkedStacks::new(),
            incoming,
            worklist,
            instruction_count: i64::try_from(instructions.len())
                .map_err(|_| VerifyError::FunctionTooLarge)?,
            abrupt_transitions: 0,
        })
    }

    fn verify(mut self) -> Result<(), VerifyError> {
        while let Some(instruction_index) = self.worklist.pop_front() {
            let Some(incoming) = self.incoming[instruction_index] else {
                continue;
            };
            self.verify_lexical_handler_state(instruction_index, incoming.state)?;
            self.visit_instruction(instruction_index, incoming.state)?;
        }
        if let Some(incoming) = self.incoming[self.instructions.len()] {
            self.verify_lexical_handler_state(self.instructions.len(), incoming.state)?;
            return Err(VerifyError::ReachableFunctionEnd {
                predecessor: incoming.predecessor,
                handler_depth: self.layout.handler_depth(incoming.state.handlers),
                parked_finally_depth: self.parked_stacks.depth(incoming.state.parked),
            });
        }
        Ok(())
    }

    fn verify_lexical_handler_state(
        &self,
        instruction_index: usize,
        state: WordcodeFlowState,
    ) -> Result<(), VerifyError> {
        let expected = self.layout.lexical_tops[instruction_index];
        if state.handlers == expected {
            return Ok(());
        }
        Err(VerifyError::InvalidHandlerStackState {
            instruction_index,
            expected_depth: self.layout.handler_depth(expected),
            actual_depth: self.layout.handler_depth(state.handlers),
            expected_enter_instruction_index: self.layout.enter_instruction_index(expected),
            actual_enter_instruction_index: self.layout.enter_instruction_index(state.handlers),
        })
    }

    fn visit_instruction(
        &mut self,
        instruction_index: usize,
        state: WordcodeFlowState,
    ) -> Result<(), VerifyError> {
        let instruction = &self.instructions[instruction_index];
        match instruction.op {
            Op::EnterTry => {
                let region = self.layout.enter_regions[instruction_index]
                    .ok_or(VerifyError::HandlerStackUnderflow { instruction_index })?;
                let mut outgoing = state;
                outgoing.handlers = Some(region);
                self.propagate_normal_successors(instruction_index, outgoing)
            }
            Op::LeaveTry => {
                let region = self.layout.leave_regions[instruction_index]
                    .ok_or(VerifyError::HandlerStackUnderflow { instruction_index })?;
                if state.handlers != Some(region) {
                    return Err(VerifyError::InvalidHandlerStackState {
                        instruction_index,
                        expected_depth: self.layout.regions[region].depth,
                        actual_depth: self.layout.handler_depth(state.handlers),
                        expected_enter_instruction_index: Some(
                            self.layout.regions[region].enter_instruction_index,
                        ),
                        actual_enter_instruction_index: self
                            .layout
                            .enter_instruction_index(state.handlers),
                    });
                }
                let handler = self.layout.regions[region];
                let mut outgoing = state;
                outgoing.handlers = handler.parent;
                if handler.finally_target.is_some() {
                    outgoing.parked = self.parked_stacks.push(
                        outgoing.parked,
                        region,
                        ParkedCompletionKinds::NORMAL,
                    );
                }
                self.propagate_normal_successors(instruction_index, outgoing)
            }
            Op::EndFinally => {
                let available = self.parked_stacks.depth(state.parked);
                let Some((parked, completions)) = self.parked_stacks.pop(state.parked) else {
                    return Err(VerifyError::AbruptCompletionUnderflow {
                        instruction_index,
                        op: instruction.op,
                        requested: 1,
                        available,
                    });
                };
                if completions.contains_normal() {
                    self.propagate_normal_successors(
                        instruction_index,
                        WordcodeFlowState { parked, ..state },
                    )?;
                }
                Ok(())
            }
            Op::PopParkedFinally => {
                let count = checked_wordcode_imm32_operand(
                    &self.operands[instruction_index],
                    instruction_index,
                    0,
                )?;
                let count =
                    usize::try_from(count).map_err(|_| VerifyError::InvalidControlFlowOperand {
                        instruction_index,
                        operand_index: 0,
                        value: count,
                    })?;
                let available = self.parked_stacks.depth(state.parked);
                if count > available {
                    return Err(VerifyError::AbruptCompletionUnderflow {
                        instruction_index,
                        op: instruction.op,
                        requested: count,
                        available,
                    });
                }
                self.charge_abrupt_transitions(instruction_index, count)?;
                let parked = self.parked_stacks.pop_count(state.parked, count).ok_or(
                    VerifyError::AbruptCompletionUnderflow {
                        instruction_index,
                        op: instruction.op,
                        requested: count,
                        available,
                    },
                )?;
                self.propagate_normal_successors(
                    instruction_index,
                    WordcodeFlowState { parked, ..state },
                )
            }
            Op::JumpViaFinally => {
                let target = self.relative_target(instruction_index, 0)?;
                let floor = checked_wordcode_imm32_operand(
                    &self.operands[instruction_index],
                    instruction_index,
                    1,
                )?;
                let floor =
                    usize::try_from(floor).map_err(|_| VerifyError::InvalidControlFlowOperand {
                        instruction_index,
                        operand_index: 1,
                        value: floor,
                    })?;
                self.route_abrupt(instruction_index, state, floor, Some(target))
            }
            Op::Return | Op::ReturnValue | Op::ReturnUndefined => {
                self.route_abrupt(instruction_index, state, 0, None)
            }
            Op::Throw => self.route_throw(instruction_index, state),
            _ => {
                let exception_successors = opcode_schema(instruction.op)
                    .exception_successor_shape
                    .exact();
                if exception_successors
                    .contains(&ExceptionSuccessorSpec::DynamicFrameHandlerOrCaller)
                {
                    self.route_throw(instruction_index, state)?;
                }
                if exception_successors
                    .contains(&ExceptionSuccessorSpec::RunFinallyHandlersToFrameReturn)
                {
                    self.route_abrupt(instruction_index, state, 0, None)?;
                }
                self.propagate_normal_successors(instruction_index, state)
            }
        }
    }

    fn relative_target(
        &self,
        instruction_index: usize,
        operand_index: usize,
    ) -> Result<usize, VerifyError> {
        resolve_wordcode_target(
            &self.operands[instruction_index],
            instruction_index,
            operand_index,
            None,
            self.instruction_count,
        )?
        .ok_or(VerifyError::InvalidControlFlowOperand {
            instruction_index,
            operand_index,
            value: NO_HANDLER_OFFSET,
        })
    }

    fn propagate_normal_successors(
        &mut self,
        instruction_index: usize,
        state: WordcodeFlowState,
    ) -> Result<(), VerifyError> {
        for successor in opcode_schema(self.instructions[instruction_index].op)
            .successor_shape
            .exact()
        {
            match successor {
                SuccessorSpec::Fallthrough => {
                    let target = instruction_index
                        .checked_add(1)
                        .ok_or(VerifyError::FunctionTooLarge)?;
                    self.propagate(instruction_index, target, state)?;
                }
                SuccessorSpec::RelativeTarget { operand_index, .. } => {
                    let target = self.relative_target(instruction_index, *operand_index)?;
                    self.propagate(instruction_index, target, state)?;
                }
                SuccessorSpec::FrameReturn => {}
            }
        }
        Ok(())
    }

    fn route_throw(
        &mut self,
        instruction_index: usize,
        state: WordcodeFlowState,
    ) -> Result<(), VerifyError> {
        let mut handlers = state.handlers;
        let mut parked = state.parked;
        while let Some(region_index) = handlers {
            self.charge_abrupt_transition(instruction_index)?;
            let region = self.layout.regions[region_index];
            handlers = region.parent;
            let (next_parked, pruned) = self.parked_stacks.prune_to_handler_depth(
                parked,
                self.layout.handler_depth(handlers),
                &self.layout,
            );
            self.charge_abrupt_transitions(instruction_index, pruned)?;
            parked = next_parked;
            if let Some(target) = region.catch_target {
                return self.propagate(
                    instruction_index,
                    target,
                    WordcodeFlowState { handlers, parked },
                );
            }
            if let Some(target) = region.finally_target {
                let finalizer_parked =
                    self.parked_stacks
                        .push(parked, region_index, ParkedCompletionKinds::ABRUPT);
                self.propagate(
                    instruction_index,
                    target,
                    WordcodeFlowState {
                        handlers,
                        parked: finalizer_parked,
                    },
                )?;
            }
        }
        Ok(())
    }

    fn route_abrupt(
        &mut self,
        instruction_index: usize,
        state: WordcodeFlowState,
        floor: usize,
        direct_target: Option<usize>,
    ) -> Result<(), VerifyError> {
        let handler_depth = self.layout.handler_depth(state.handlers);
        if floor > handler_depth {
            return Err(VerifyError::InvalidFinallyFloor {
                instruction_index,
                floor,
                handler_depth,
            });
        }
        let mut handlers = state.handlers;
        let mut parked = state.parked;
        while self.layout.handler_depth(handlers) > floor {
            self.charge_abrupt_transition(instruction_index)?;
            let region_index = handlers.ok_or(VerifyError::InvalidFinallyFloor {
                instruction_index,
                floor,
                handler_depth,
            })?;
            let region = self.layout.regions[region_index];
            handlers = region.parent;
            let (next_parked, pruned) = self.parked_stacks.prune_to_handler_depth(
                parked,
                self.layout.handler_depth(handlers),
                &self.layout,
            );
            self.charge_abrupt_transitions(instruction_index, pruned)?;
            parked = next_parked;
            if let Some(target) = region.finally_target {
                let finalizer_parked =
                    self.parked_stacks
                        .push(parked, region_index, ParkedCompletionKinds::ABRUPT);
                self.propagate(
                    instruction_index,
                    target,
                    WordcodeFlowState {
                        handlers,
                        parked: finalizer_parked,
                    },
                )?;
            }
        }
        if let Some(target) = direct_target {
            self.propagate(
                instruction_index,
                target,
                WordcodeFlowState { handlers, parked },
            )?;
        }
        Ok(())
    }

    fn charge_abrupt_transition(&mut self, instruction_index: usize) -> Result<(), VerifyError> {
        self.charge_abrupt_transitions(instruction_index, 1)
    }

    fn charge_abrupt_transitions(
        &mut self,
        instruction_index: usize,
        count: usize,
    ) -> Result<(), VerifyError> {
        self.abrupt_transitions = self.abrupt_transitions.checked_add(count).ok_or(
            VerifyError::ControlFlowAnalysisLimitExceeded {
                instruction_index,
                limit: MAX_WORDCODE_ABRUPT_TRANSITIONS,
            },
        )?;
        if self.abrupt_transitions > MAX_WORDCODE_ABRUPT_TRANSITIONS {
            return Err(VerifyError::ControlFlowAnalysisLimitExceeded {
                instruction_index,
                limit: MAX_WORDCODE_ABRUPT_TRANSITIONS,
            });
        }
        Ok(())
    }

    fn merge_parked_states(
        &mut self,
        instruction_index: usize,
        first: ParkedStackId,
        second: ParkedStackId,
    ) -> Result<Option<ParkedStackId>, VerifyError> {
        let Some(frames) = self.parked_stacks.merged_frames(first, second) else {
            return Ok(None);
        };
        self.charge_abrupt_transitions(instruction_index, frames.len())?;
        Ok(Some(self.parked_stacks.intern_merged_frames(&frames)))
    }

    fn propagate(
        &mut self,
        predecessor: usize,
        instruction_index: usize,
        state: WordcodeFlowState,
    ) -> Result<(), VerifyError> {
        if instruction_index >= self.incoming.len() {
            return Err(VerifyError::InvalidControlFlowTarget {
                instruction_index: predecessor,
                target: i64::try_from(instruction_index).unwrap_or(i64::MAX),
            });
        }
        if let Some(first) = self.incoming[instruction_index] {
            if first.state.handlers != state.handlers {
                return Err(VerifyError::IncompatibleHandlerStackStates {
                    instruction_index,
                    first_predecessor: first.predecessor,
                    conflicting_predecessor: predecessor,
                    first_depth: self.layout.handler_depth(first.state.handlers),
                    conflicting_depth: self.layout.handler_depth(state.handlers),
                });
            }
            if first.state.parked != state.parked {
                let Some(parked) =
                    self.merge_parked_states(instruction_index, first.state.parked, state.parked)?
                else {
                    return Err(VerifyError::IncompatibleAbruptCompletionStates {
                        instruction_index,
                        first_predecessor: first.predecessor,
                        conflicting_predecessor: predecessor,
                        first_depth: self.parked_stacks.depth(first.state.parked),
                        conflicting_depth: self.parked_stacks.depth(state.parked),
                    });
                };
                if parked != first.state.parked {
                    self.incoming[instruction_index] = Some(IncomingWordcodeFlowState {
                        state: WordcodeFlowState {
                            parked,
                            ..first.state
                        },
                        predecessor: first.predecessor,
                    });
                    if instruction_index < self.instructions.len() {
                        self.worklist.push_back(instruction_index);
                    }
                }
            }
            return Ok(());
        }
        self.incoming[instruction_index] = Some(IncomingWordcodeFlowState {
            state,
            predecessor: Some(predecessor),
        });
        if instruction_index < self.instructions.len() {
            self.worklist.push_back(instruction_index);
        }
        Ok(())
    }
}

fn verify_wordcode_control_flow(
    instructions: &[WordInstruction],
    operands: &[Box<[Operand]>],
) -> Result<(), VerifyError> {
    WordcodeControlFlowVerifier::new(instructions, operands)?.verify()
}

const OPERAND_KIND_REGISTER: u8 = 0;
const OPERAND_KIND_CONST_INDEX: u8 = 1;
const OPERAND_KIND_IMM32: u8 = 2;

/// Errors surfaced while decoding the cold serialized representation.
#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Stream ended mid-instruction.
    UnexpectedEnd {
        /// Byte offset at which the stream ended unexpectedly.
        offset: usize,
    },
    /// Opcode byte not recognised.
    UnknownOpcode {
        /// Byte offset of the offending opcode byte.
        offset: usize,
        /// Raw opcode byte value.
        byte: u8,
    },
    /// Operand kind tag not recognised.
    UnknownOperandKind {
        /// Byte offset of the operand kind byte.
        offset: usize,
        /// Raw operand kind byte value.
        kind: u8,
    },
    /// A schema-authoritative fixed instruction has an invalid shape.
    InvalidOperandShape {
        /// Byte offset of the instruction opcode.
        offset: usize,
        /// Exact count or wire-kind mismatch.
        error: OperandShapeError,
    },
    /// A schema-authoritative branch targets an invalid byte position.
    InvalidControlFlowTarget {
        /// Byte offset of the branch instruction.
        offset: usize,
        /// Resolved signed target byte position.
        target: i64,
    },
    /// A schema-authoritative control-flow operand has an invalid value.
    InvalidControlFlowOperand {
        /// Byte offset of the instruction.
        offset: usize,
        /// Operand position.
        operand_index: usize,
        /// Invalid signed immediate.
        value: i32,
    },
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnexpectedEnd { offset } => {
                write!(
                    f,
                    "unexpected end of bytecode stream at byte offset {offset}"
                )
            }
            Self::UnknownOpcode { offset, byte } => {
                write!(f, "unknown opcode byte 0x{byte:02X} at offset {offset}")
            }
            Self::UnknownOperandKind { offset, kind } => {
                write!(f, "unknown operand kind 0x{kind:02X} at offset {offset}")
            }
            Self::InvalidOperandShape { offset, error } => {
                write!(f, "invalid operand shape at byte offset {offset}: {error}")
            }
            Self::InvalidControlFlowTarget { offset, target } => write!(
                f,
                "invalid control-flow target {target} at byte offset {offset}"
            ),
            Self::InvalidControlFlowOperand {
                offset,
                operand_index,
                value,
            } => write!(
                f,
                "invalid control-flow operand {operand_index} value {value} at byte offset {offset}"
            ),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Malformed or noncanonical operand storage in authoritative wordcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandStorageError {
    /// An inline instruction points into the overflow table.
    InlineUsesOverflow {
        /// Raw operand count.
        operand_count: u8,
        /// Unexpected overflow-table offset.
        overflow_offset: u32,
    },
    /// An instruction too large for inline storage has no overflow range.
    MissingOverflow {
        /// Raw operand count.
        operand_count: u8,
    },
    /// An overflow range does not fit wholly in the shared table.
    OverflowOutOfBounds {
        /// Raw overflow-table offset.
        overflow_offset: u32,
        /// Raw operand count.
        operand_count: u8,
        /// Number of words available in the table.
        available_words: usize,
    },
    /// An overflow range is valid in isolation but is not the next dense range.
    NonCanonicalOverflowOffset {
        /// Required next dense word offset.
        expected: usize,
        /// Raw offset stored by the instruction.
        actual: u32,
    },
    /// An inline slot that is not active in this storage form is nonzero.
    NonZeroInlinePadding {
        /// Inline word position.
        word_index: usize,
        /// Noncanonical word value.
        word: u32,
    },
    /// A raw word cannot be represented by its schema-declared operand kind.
    InvalidOperandWord {
        /// Operand position.
        operand_index: usize,
        /// Schema-declared kind.
        expected: OperandKind,
        /// Invalid raw word.
        word: u32,
    },
}

impl std::fmt::Display for OperandStorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InlineUsesOverflow {
                operand_count,
                overflow_offset,
            } => write!(
                f,
                "inline operand count {operand_count} uses overflow offset {overflow_offset}"
            ),
            Self::MissingOverflow { operand_count } => write!(
                f,
                "operand count {operand_count} exceeds inline storage without an overflow range"
            ),
            Self::OverflowOutOfBounds {
                overflow_offset,
                operand_count,
                available_words,
            } => write!(
                f,
                "overflow range {overflow_offset}..+{operand_count} exceeds {available_words} words"
            ),
            Self::NonCanonicalOverflowOffset { expected, actual } => write!(
                f,
                "overflow offset {actual} is noncanonical; expected dense offset {expected}"
            ),
            Self::NonZeroInlinePadding { word_index, word } => write!(
                f,
                "inactive inline operand word {word_index} is nonzero ({word})"
            ),
            Self::InvalidOperandWord {
                operand_index,
                expected,
                word,
            } => write!(
                f,
                "operand {operand_index} word {word} cannot be decoded as {expected:?}"
            ),
        }
    }
}

impl std::error::Error for OperandStorageError {}

/// Structural error in logical instructions or authoritative wordcode.
#[derive(Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// Cold serialized metadata would exceed the u32 PC coordinate space.
    FunctionTooLarge,
    /// An instruction violates its authoritative operand shape.
    InvalidOperandShape {
        /// Dense source-order instruction index.
        instruction_index: usize,
        /// Exact count or kind mismatch.
        error: OperandShapeError,
    },
    /// One instruction has malformed or noncanonical raw operand storage.
    InvalidOperandStorage {
        /// Dense source-order instruction index.
        instruction_index: usize,
        /// Exact storage violation.
        error: OperandStorageError,
    },
    /// The shared overflow table contains words not owned by an instruction.
    TrailingOverflowOperandWords {
        /// First word not consumed by canonical dense ranges.
        first_unused_word: usize,
        /// Total overflow-table word count.
        total_words: usize,
    },
    /// A relative branch or handler target falls outside `0..=len`.
    InvalidControlFlowTarget {
        /// Dense source-order instruction index.
        instruction_index: usize,
        /// Resolved signed target instruction index.
        target: i64,
    },
    /// A schema-declared control-flow operand contains an invalid value.
    InvalidControlFlowOperand {
        /// Dense source-order instruction index.
        instruction_index: usize,
        /// Operand position.
        operand_index: usize,
        /// Invalid signed immediate.
        value: i32,
    },
    /// A source-order `LeaveTry` has no lexically enclosing `EnterTry`.
    HandlerStackUnderflow {
        /// Dense source-order instruction index of the unmatched leave.
        instruction_index: usize,
    },
    /// An `EnterTry` reaches the end of the function without a matching leave.
    UnclosedHandler {
        /// Dense source-order instruction index of the unmatched enter.
        enter_instruction_index: usize,
    },
    /// An `EnterTry` would install a handler that can handle no completion.
    HandlerWithoutTarget {
        /// Dense source-order instruction index of the invalid enter.
        instruction_index: usize,
    },
    /// A catch/finally landing points at the non-executable end boundary.
    InvalidHandlerTarget {
        /// Dense source-order instruction index of the owning enter.
        instruction_index: usize,
        /// Catch/finally operand position.
        operand_index: usize,
        /// Resolved end-boundary instruction index.
        target: usize,
    },
    /// A handler landing is lexically inside a different handler stack.
    InvalidHandlerLandingState {
        /// Dense source-order instruction index of the owning enter.
        instruction_index: usize,
        /// Resolved catch/finally landing instruction.
        target: usize,
        /// Handler depth after the owning handler is popped.
        expected_depth: usize,
        /// Lexical handler depth at the landing instruction.
        actual_depth: usize,
    },
    /// One reachable instruction does not have its lexical handler stack.
    InvalidHandlerStackState {
        /// Dense source-order instruction index.
        instruction_index: usize,
        /// Handler depth implied by lexical region nesting.
        expected_depth: usize,
        /// Handler depth carried by the incoming runtime path.
        actual_depth: usize,
        /// Expected innermost `EnterTry`, when any.
        expected_enter_instruction_index: Option<usize>,
        /// Actual innermost `EnterTry`, when any.
        actual_enter_instruction_index: Option<usize>,
    },
    /// Two predecessors reach one instruction with different handler stacks.
    IncompatibleHandlerStackStates {
        /// Dense source-order join instruction index (or function end).
        instruction_index: usize,
        /// Predecessor that installed the first state; `None` is function entry.
        first_predecessor: Option<usize>,
        /// Predecessor carrying the conflicting state.
        conflicting_predecessor: usize,
        /// First handler depth.
        first_depth: usize,
        /// Conflicting handler depth.
        conflicting_depth: usize,
    },
    /// A `JumpViaFinally` floor exceeds the live handler-stack depth.
    InvalidFinallyFloor {
        /// Dense source-order jump instruction index.
        instruction_index: usize,
        /// Requested non-negative handler floor.
        floor: usize,
        /// Handler depth on this runtime path.
        handler_depth: usize,
    },
    /// `EndFinally`/`PopParkedFinally` consumes unavailable completions.
    AbruptCompletionUnderflow {
        /// Dense source-order instruction index.
        instruction_index: usize,
        /// Abrupt-completion opcode being validated.
        op: Op,
        /// Number of parked completions requested.
        requested: usize,
        /// Number of parked completions available on this path.
        available: usize,
    },
    /// Two predecessors disagree about the structural parked-finally stack.
    IncompatibleAbruptCompletionStates {
        /// Dense source-order join instruction index (or function end).
        instruction_index: usize,
        /// Predecessor that installed the first state; `None` is function entry.
        first_predecessor: Option<usize>,
        /// Predecessor carrying the conflicting state.
        conflicting_predecessor: usize,
        /// First parked-finally depth.
        first_depth: usize,
        /// Conflicting parked-finally depth.
        conflicting_depth: usize,
    },
    /// Normal control flow reaches the non-executable function-end boundary.
    ReachableFunctionEnd {
        /// Instruction whose successor reaches the end; `None` for an empty body.
        predecessor: Option<usize>,
        /// Live handler depth carried to the end boundary.
        handler_depth: usize,
        /// Live parked-finally depth carried to the end boundary.
        parked_finally_depth: usize,
    },
    /// Adversarial exceptional or parked-state flow exceeded the fixed
    /// analysis-work budget.
    ControlFlowAnalysisLimitExceeded {
        /// Instruction whose unwind walk exhausted the budget.
        instruction_index: usize,
        /// Maximum handler transitions inspected per function.
        limit: usize,
    },
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FunctionTooLarge => write!(f, "function byte metadata exceeds u32::MAX"),
            Self::InvalidOperandShape {
                instruction_index,
                error,
            } => write!(
                f,
                "invalid operand shape at instruction {instruction_index}: {error}"
            ),
            Self::InvalidOperandStorage {
                instruction_index,
                error,
            } => write!(
                f,
                "invalid operand storage at instruction {instruction_index}: {error}"
            ),
            Self::TrailingOverflowOperandWords {
                first_unused_word,
                total_words,
            } => write!(
                f,
                "overflow operand words {first_unused_word}..{total_words} are unreferenced"
            ),
            Self::InvalidControlFlowTarget {
                instruction_index,
                target,
            } => write!(
                f,
                "invalid control-flow target {target} at instruction {instruction_index}"
            ),
            Self::InvalidControlFlowOperand {
                instruction_index,
                operand_index,
                value,
            } => write!(
                f,
                "invalid control-flow operand {operand_index} value {value} at instruction {instruction_index}"
            ),
            Self::HandlerStackUnderflow { instruction_index } => write!(
                f,
                "LeaveTry at instruction {instruction_index} has no enclosing EnterTry"
            ),
            Self::UnclosedHandler {
                enter_instruction_index,
            } => write!(
                f,
                "EnterTry at instruction {enter_instruction_index} has no matching LeaveTry"
            ),
            Self::HandlerWithoutTarget { instruction_index } => write!(
                f,
                "EnterTry at instruction {instruction_index} has neither a catch nor a finally target"
            ),
            Self::InvalidHandlerTarget {
                instruction_index,
                operand_index,
                target,
            } => write!(
                f,
                "EnterTry operand {operand_index} at instruction {instruction_index} targets non-executable function end {target}"
            ),
            Self::InvalidHandlerLandingState {
                instruction_index,
                target,
                expected_depth,
                actual_depth,
            } => write!(
                f,
                "EnterTry at instruction {instruction_index} lands at instruction {target} with lexical handler depth {actual_depth}, expected {expected_depth}"
            ),
            Self::InvalidHandlerStackState {
                instruction_index,
                expected_depth,
                actual_depth,
                expected_enter_instruction_index,
                actual_enter_instruction_index,
            } => write!(
                f,
                "instruction {instruction_index} has handler depth {actual_depth} (top {actual_enter_instruction_index:?}), expected depth {expected_depth} (top {expected_enter_instruction_index:?})"
            ),
            Self::IncompatibleHandlerStackStates {
                instruction_index,
                first_predecessor,
                conflicting_predecessor,
                first_depth,
                conflicting_depth,
            } => write!(
                f,
                "instruction {instruction_index} joins handler depth {first_depth} from {first_predecessor:?} with depth {conflicting_depth} from instruction {conflicting_predecessor}"
            ),
            Self::InvalidFinallyFloor {
                instruction_index,
                floor,
                handler_depth,
            } => write!(
                f,
                "JumpViaFinally at instruction {instruction_index} requests handler floor {floor} above live depth {handler_depth}"
            ),
            Self::AbruptCompletionUnderflow {
                instruction_index,
                op,
                requested,
                available,
            } => write!(
                f,
                "{op:?} at instruction {instruction_index} consumes {requested} parked finally completions, but only {available} are available"
            ),
            Self::IncompatibleAbruptCompletionStates {
                instruction_index,
                first_predecessor,
                conflicting_predecessor,
                first_depth,
                conflicting_depth,
            } => write!(
                f,
                "instruction {instruction_index} joins parked-finally depth {first_depth} from {first_predecessor:?} with depth {conflicting_depth} from instruction {conflicting_predecessor}"
            ),
            Self::ReachableFunctionEnd {
                predecessor,
                handler_depth,
                parked_finally_depth,
            } => write!(
                f,
                "normal control flow from {predecessor:?} reaches function end with handler depth {handler_depth} and parked-finally depth {parked_finally_depth}"
            ),
            Self::ControlFlowAnalysisLimitExceeded {
                instruction_index,
                limit,
            } => write!(
                f,
                "control-flow analysis at instruction {instruction_index} exceeds {limit} handler/parked-state transitions"
            ),
        }
    }
}

impl std::error::Error for VerifyError {}

/// Verify compiler instructions directly in their logical instruction-index
/// coordinate system.
///
/// # Errors
/// Returns [`VerifyError`] for operand-shape, branch-target, handler-target, or
/// finally-floor violations.
pub fn verify_logical_function(instructions: &[Instruction]) -> Result<(), VerifyError> {
    let len = instructions.len() as i64;
    for (instruction_index, instr) in instructions.iter().enumerate() {
        verify_operand_shape(instr.op, instr.operands.as_slice()).map_err(|error| {
            VerifyError::InvalidOperandShape {
                instruction_index,
                error,
            }
        })?;
        let schema = opcode_schema(instr.op);
        for successor in schema.successor_shape.exact() {
            if let SuccessorSpec::RelativeTarget { operand_index, .. } = successor {
                verify_logical_target(instructions, instruction_index, *operand_index, None, len)?;
            }
        }
        for successor in schema.exception_successor_shape.exact() {
            match successor {
                ExceptionSuccessorSpec::OptionalRelativeTarget {
                    operand_index,
                    absent_value,
                    ..
                } => verify_logical_target(
                    instructions,
                    instruction_index,
                    *operand_index,
                    Some(*absent_value),
                    len,
                )?,
                ExceptionSuccessorSpec::RunFinallyHandlersToFloor {
                    floor_operand_index,
                } => {
                    let Operand::Imm32(floor) = instr.operands.as_slice()[*floor_operand_index]
                    else {
                        unreachable!("operand shape verified before control-flow metadata")
                    };
                    if floor < 0 {
                        return Err(VerifyError::InvalidControlFlowOperand {
                            instruction_index,
                            operand_index: *floor_operand_index,
                            value: floor,
                        });
                    }
                }
                ExceptionSuccessorSpec::DynamicFrameHandlerOrCaller
                | ExceptionSuccessorSpec::CallerHandlerOrUncaught
                | ExceptionSuccessorSpec::ResumeParkedAbruptCompletion
                | ExceptionSuccessorSpec::RunFinallyHandlersToFrameReturn => {}
            }
        }
    }
    Ok(())
}

/// Cold serialized byte-PC layout derived without materialising a byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionLayout {
    /// Total serialized length used as the byte-PC end boundary.
    pub total_bytes: u32,
    /// Byte PC for every logical instruction index.
    pub instr_to_byte_pc: Box<[u32]>,
}

/// Verify a logical function and calculate its cold serialized byte-PC layout
/// without encoding or decoding a byte stream.
///
/// # Errors
/// Returns [`VerifyError`] for an invalid logical function or u32 metadata
/// overflow.
pub fn layout_function(instructions: &[Instruction]) -> Result<FunctionLayout, VerifyError> {
    verify_logical_function(instructions)?;
    let mut byte_pc = 0_u32;
    let mut instr_to_byte_pc = Vec::with_capacity(instructions.len());
    for instr in instructions {
        instr_to_byte_pc.push(byte_pc);
        let mut byte_len = 2_u32;
        for operand in instr.operands.iter() {
            let operand_len = match operand {
                Operand::Register(_) => 3,
                Operand::ConstIndex(_) | Operand::Imm32(_) => 5,
            };
            byte_len = byte_len
                .checked_add(operand_len)
                .ok_or(VerifyError::FunctionTooLarge)?;
        }
        byte_pc = byte_pc
            .checked_add(byte_len)
            .ok_or(VerifyError::FunctionTooLarge)?;
    }
    Ok(FunctionLayout {
        total_bytes: byte_pc,
        instr_to_byte_pc: instr_to_byte_pc.into_boxed_slice(),
    })
}

/// Verify authoritative wordcode and calculate its cold serialized byte-PC
/// layout without creating a byte stream.
///
/// # Errors
/// Returns [`VerifyError`] for invalid wordcode or u32 metadata overflow.
pub fn layout_wordcode_function(code: &FunctionCode) -> Result<FunctionLayout, VerifyError> {
    verify_wordcode_function(code)?;
    measure_wordcode_function(code)
}

/// Calculate a wordcode function's serialized byte-PC layout without
/// validating control-flow targets.
///
/// Production loading should use [`layout_wordcode_function`]. This narrower
/// operation exists for compiler/backend negative-test fixtures that
/// deliberately carry malformed targets but still need the canonical encoded
/// size before the intended validation stage.
///
/// # Errors
/// Returns [`VerifyError`] for malformed/noncanonical operand storage,
/// operand-shape violations, or u32 metadata overflow. Control-flow targets
/// are intentionally not checked here.
pub fn measure_wordcode_function(code: &FunctionCode) -> Result<FunctionLayout, VerifyError> {
    let mut byte_pc = 0_u32;
    let mut instr_to_byte_pc = Vec::with_capacity(code.len());
    let (instructions, overflow_operand_words) = code.raw_parts();
    let mut next_overflow_offset = 0;
    for (instruction_index, instruction) in instructions.iter().enumerate() {
        let operands = decode_wordcode_operands(
            instruction,
            overflow_operand_words,
            &mut next_overflow_offset,
            instruction_index,
        )?;
        verify_operand_shape(instruction.op, &operands).map_err(|error| {
            VerifyError::InvalidOperandShape {
                instruction_index,
                error,
            }
        })?;
        instr_to_byte_pc.push(byte_pc);
        let mut byte_len = 2_u32;
        for operand in operands {
            let operand_len = match operand {
                Operand::Register(_) => 3,
                Operand::ConstIndex(_) | Operand::Imm32(_) => 5,
            };
            byte_len = byte_len
                .checked_add(operand_len)
                .ok_or(VerifyError::FunctionTooLarge)?;
        }
        byte_pc = byte_pc
            .checked_add(byte_len)
            .ok_or(VerifyError::FunctionTooLarge)?;
    }
    verify_wordcode_overflow_consumed(next_overflow_offset, overflow_operand_words.len())?;
    Ok(FunctionLayout {
        total_bytes: byte_pc,
        instr_to_byte_pc: instr_to_byte_pc.into_boxed_slice(),
    })
}

fn verify_logical_target(
    instructions: &[Instruction],
    instruction_index: usize,
    operand_index: usize,
    absent_value: Option<i32>,
    instruction_count: i64,
) -> Result<(), VerifyError> {
    let Operand::Imm32(delta) = instructions[instruction_index].operands.as_slice()[operand_index]
    else {
        unreachable!("operand shape verified before control-flow metadata")
    };
    if absent_value == Some(delta) {
        return Ok(());
    }
    let target = instruction_index as i64 + 1 + i64::from(delta);
    if !(0..=instruction_count).contains(&target) {
        return Err(VerifyError::InvalidControlFlowTarget {
            instruction_index,
            target,
        });
    }
    Ok(())
}

/// Append-only writer that builds the byte stream from an
/// [`Instruction`] sequence.
#[derive(Debug, Default)]
pub struct BytecodeWriter {
    bytes: Vec<u8>,
}

impl BytecodeWriter {
    /// Construct an empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Byte offset at which the next write will land.
    #[must_use]
    pub fn pc(&self) -> u32 {
        u32::try_from(self.bytes.len()).expect("bytecode stream over u32::MAX bytes")
    }

    /// Encode one [`Instruction`] onto the stream. Panics if the
    /// opcode has no registered byte mapping; callers should add a
    /// row to [`op_to_byte`] / [`op_from_byte`] before writing a new
    /// opcode.
    pub fn write(&mut self, instr: &Instruction) {
        let opcode_byte = op_to_byte(instr.op)
            .unwrap_or_else(|| panic!("opcode {:?} not registered in bytecode table", instr.op));
        self.bytes.push(opcode_byte);
        let operands = instr.operands.as_slice();
        let count =
            u8::try_from(operands.len()).expect("instruction operand count exceeds u8::MAX");
        self.bytes.push(count);
        for operand in operands {
            self.write_operand(operand);
        }
    }

    fn write_operand(&mut self, operand: &Operand) {
        match operand {
            Operand::Register(reg) => {
                self.bytes.push(OPERAND_KIND_REGISTER);
                self.bytes.extend_from_slice(&reg.to_le_bytes());
            }
            Operand::ConstIndex(idx) => {
                self.bytes.push(OPERAND_KIND_CONST_INDEX);
                self.bytes.extend_from_slice(&idx.to_le_bytes());
            }
            Operand::Imm32(imm) => {
                self.bytes.push(OPERAND_KIND_IMM32);
                self.bytes.extend_from_slice(&imm.to_le_bytes());
            }
        }
    }

    /// Freeze the writer into a boxed byte stream.
    #[must_use]
    pub fn into_bytes(self) -> Box<[u8]> {
        self.bytes.into_boxed_slice()
    }
}

/// Encoded function body: byte stream plus the per-instruction byte
/// offsets. Source-map translation reuses the offsets to rewrite each
/// `SpanEntry::pc` from instruction index (the compiler's coordinate
/// system) to byte offset (the dispatcher's) without re-walking the
/// stream.
#[derive(Debug, Clone)]
pub struct EncodedFunction {
    /// Byte-stream code.
    pub code: Box<[u8]>,
    /// `instr_to_byte_pc[i]` is the byte offset of the `i`-th
    /// instruction in `code`. Length equals the source `Instruction`
    /// slice length.
    pub instr_to_byte_pc: Box<[u32]>,
}

/// Encode a whole function body (an [`Instruction`] slice in source
/// order) into the byte stream and return both the bytes and the
/// per-instruction byte-offset map.
///
/// The compiler emits branch operands (`Op::Jump`, `Op::JumpIfTrue`,
/// `Op::JumpIfFalse`, `Op::JumpIfNullish`, `Op::EnterTry`) as
/// *instruction-index* deltas relative to the instruction following the
/// jump. The wire format wants *byte-offset* deltas relative to
/// `(jump_pc + 1)` (the byte right after the opcode), so encoding is a
/// two-pass walk:
///
/// 1. Write every instruction in order, capturing each jump operand's
///    byte position and the source instruction-index delta.
/// 2. Re-resolve each captured slot to a byte-offset delta computed
///    against `instr_to_byte_pc`.
///
/// The `NO_HANDLER_OFFSET` sentinel (`i32::MIN`) is preserved as-is —
/// the runtime treats it as "absent handler" for [`Op::EnterTry`].
#[must_use]
pub fn encode_function(instructions: &[Instruction]) -> EncodedFunction {
    try_encode_function(instructions)
        .unwrap_or_else(|error| panic!("invalid logical bytecode function: {error}"))
}

/// Verify and encode a compiler function DTO without decoding the serialized
/// bytes back into an execution representation.
///
/// # Errors
/// Returns [`VerifyError`] before encoding when the logical instruction stream
/// violates the opcode schema or its instruction-index CFG.
pub fn try_encode_function(instructions: &[Instruction]) -> Result<EncodedFunction, VerifyError> {
    verify_logical_function(instructions)?;
    let mut writer = BytecodeWriter::new();
    let mut instr_to_byte_pc: Vec<u32> = Vec::with_capacity(instructions.len());
    let mut fixups: Vec<JumpFixup> = Vec::new();

    for (idx, instr) in instructions.iter().enumerate() {
        let byte_pc = writer.pc();
        instr_to_byte_pc.push(byte_pc);
        write_instruction_capturing_jumps(&mut writer, instr, idx, byte_pc, &mut fixups);
    }

    let total_bytes = writer.pc();
    for fixup in &fixups {
        resolve_jump_fixup(
            &mut writer.bytes,
            fixup,
            &instr_to_byte_pc,
            total_bytes,
            instructions.len(),
        );
    }

    Ok(EncodedFunction {
        code: writer.into_bytes(),
        instr_to_byte_pc: instr_to_byte_pc.into_boxed_slice(),
    })
}

/// Slot bookkeeping for a single jump-class `Imm32` operand needing
/// byte-offset patching after the whole function has been laid out.
#[derive(Debug, Clone, Copy)]
struct JumpFixup {
    /// Source-order index of the jump instruction.
    jump_idx: usize,
    /// Byte offset of the jump opcode byte in the encoded stream.
    jump_byte_pc: u32,
    /// Byte offset of the `Imm32` payload bytes (the four bytes after
    /// the operand kind tag) for this jump operand.
    imm32_byte_offset: u32,
}

fn write_instruction_capturing_jumps(
    writer: &mut BytecodeWriter,
    instr: &Instruction,
    jump_idx: usize,
    jump_byte_pc: u32,
    fixups: &mut Vec<JumpFixup>,
) {
    let opcode_byte = op_to_byte(instr.op)
        .unwrap_or_else(|| panic!("opcode {:?} not registered in bytecode table", instr.op));
    writer.bytes.push(opcode_byte);
    let operands = instr.operands.as_slice();
    let count = u8::try_from(operands.len()).expect("instruction operand count exceeds u8::MAX");
    writer.bytes.push(count);
    let branch_slots = branch_imm32_operand_slots(instr.op);
    for (op_idx, operand) in operands.iter().enumerate() {
        let operand_start = writer.bytes.len() as u32;
        writer.write_operand(operand);
        if branch_slots.contains(&op_idx) {
            assert!(
                matches!(operand, Operand::Imm32(_)),
                "branch operand at slot {op_idx} of {:?} must be Imm32, got {operand:?}",
                instr.op
            );
            // `operand_start` points at the operand-kind tag byte; the
            // four little-endian `Imm32` payload bytes follow it.
            fixups.push(JumpFixup {
                jump_idx,
                jump_byte_pc,
                imm32_byte_offset: operand_start + 1,
            });
        }
    }
}

fn resolve_jump_fixup(
    bytes: &mut [u8],
    fixup: &JumpFixup,
    instr_to_byte_pc: &[u32],
    total_bytes: u32,
    instruction_count: usize,
) {
    let start = fixup.imm32_byte_offset as usize;
    let raw_bytes: [u8; 4] = bytes[start..start + 4]
        .try_into()
        .expect("imm32 payload occupies exactly 4 bytes");
    let raw = i32::from_le_bytes(raw_bytes);
    if raw == NO_HANDLER_OFFSET {
        return;
    }
    let target_instr_idx = (fixup.jump_idx as i64) + 1 + (raw as i64);
    assert!(
        target_instr_idx >= 0,
        "jump target instruction index underflow: jump_idx={} raw_delta={}",
        fixup.jump_idx,
        raw
    );
    let target_byte_pc = if target_instr_idx as usize == instruction_count {
        // Jump past the last instruction lands at end-of-stream.
        total_bytes
    } else {
        instr_to_byte_pc[target_instr_idx as usize]
    };
    let base = i64::from(fixup.jump_byte_pc) + 1;
    let byte_delta = i64::from(target_byte_pc) - base;
    let byte_delta_i32 =
        i32::try_from(byte_delta).expect("jump byte-offset delta exceeds i32 range");
    bytes[start..start + 4].copy_from_slice(&byte_delta_i32.to_le_bytes());
}

/// Operand slot positions whose `Imm32` value is a branch offset that
/// the encoder must rewrite from instruction-index delta to byte-offset
/// delta. Non-branch opcodes return an empty slice.
fn branch_imm32_operand_slots(op: Op) -> &'static [usize] {
    match op {
        Op::Jump | Op::JumpIfTrue | Op::JumpIfFalse | Op::JumpIfNullish | Op::JumpViaFinally => {
            &[0]
        }
        Op::EnterTry => &[0, 1],
        _ => &[],
    }
}

/// Translate an instruction-index source-map into byte-offset form
/// using the `instr_to_byte_pc` map produced by [`encode_function`].
/// Out-of-range entries fall back to the end of the byte stream
/// (`total_bytes`), matching the "jump past the last instruction"
/// convention used by the encoder itself.
///
/// Order is preserved; the caller may pass either a `Vec` or a slice
/// borrowed from [`crate::Function::spans`].
#[must_use]
pub fn translate_spans_to_byte_pcs(
    spans: &[SpanEntry],
    instr_to_byte_pc: &[u32],
    total_bytes: u32,
) -> Vec<SpanEntry> {
    spans
        .iter()
        .map(|entry| {
            let byte_pc = instr_to_byte_pc
                .get(entry.pc as usize)
                .copied()
                .unwrap_or(total_bytes);
            SpanEntry {
                pc: byte_pc,
                span: entry.span,
            }
        })
        .collect()
}

/// Decode a whole function body into the corresponding
/// [`Instruction`] sequence, re-walking the byte stream.
///
/// # Errors
///
/// Propagates any [`DecodeError`] from instruction decoding or schema-derived
/// control-flow target verification.
pub fn decode_function(code: &[u8]) -> Result<Vec<Instruction>, DecodeError> {
    let mut out: Vec<Instruction> = Vec::new();
    let mut pc = 0usize;
    while pc < code.len() {
        let (instr, next) = decode_instruction(code, pc)?;
        out.push(instr);
        pc = next;
    }
    verify_control_flow_targets(&out, code.len())?;
    Ok(out)
}

fn verify_control_flow_targets(
    instructions: &[Instruction],
    code_len: usize,
) -> Result<(), DecodeError> {
    let boundaries: Vec<u32> = instructions.iter().map(|instr| instr.pc).collect();
    for instr in instructions {
        let schema = opcode_schema(instr.op);
        for successor in schema.successor_shape.exact() {
            let SuccessorSpec::RelativeTarget {
                operand_index,
                base,
            } = successor
            else {
                continue;
            };
            verify_relative_target(instr, *operand_index, *base, None, &boundaries, code_len)?;
        }
        for successor in schema.exception_successor_shape.exact() {
            match successor {
                ExceptionSuccessorSpec::OptionalRelativeTarget {
                    operand_index,
                    base,
                    absent_value,
                } => verify_relative_target(
                    instr,
                    *operand_index,
                    *base,
                    Some(*absent_value),
                    &boundaries,
                    code_len,
                )?,
                ExceptionSuccessorSpec::RunFinallyHandlersToFloor {
                    floor_operand_index,
                } => {
                    let Operand::Imm32(floor) = instr.operands.as_slice()[*floor_operand_index]
                    else {
                        unreachable!("schema operand verification precedes successor verification")
                    };
                    if floor < 0 {
                        return Err(DecodeError::InvalidControlFlowOperand {
                            offset: instr.pc as usize,
                            operand_index: *floor_operand_index,
                            value: floor,
                        });
                    }
                }
                ExceptionSuccessorSpec::DynamicFrameHandlerOrCaller
                | ExceptionSuccessorSpec::CallerHandlerOrUncaught
                | ExceptionSuccessorSpec::ResumeParkedAbruptCompletion
                | ExceptionSuccessorSpec::RunFinallyHandlersToFrameReturn => {}
            }
        }
    }
    Ok(())
}

fn verify_relative_target(
    instr: &Instruction,
    operand_index: usize,
    base: RelativeTargetBase,
    absent_value: Option<i32>,
    boundaries: &[u32],
    code_len: usize,
) -> Result<(), DecodeError> {
    let Operand::Imm32(delta) = instr.operands.as_slice()[operand_index] else {
        unreachable!("schema operand verification precedes successor verification")
    };
    if absent_value == Some(delta) {
        return Ok(());
    }
    let base = match base {
        RelativeTargetBase::AfterOpcode => i64::from(instr.pc) + 1,
    };
    let target = base + i64::from(delta);
    let valid = target == code_len as i64
        || (target >= 0
            && target < code_len as i64
            && boundaries.binary_search(&(target as u32)).is_ok());
    if !valid {
        return Err(DecodeError::InvalidControlFlowTarget {
            offset: instr.pc as usize,
            target,
        });
    }
    Ok(())
}

/// Decode the next instruction from `code` starting at byte offset
/// `pc`. Returns the decoded instruction and the byte offset of the
/// instruction that follows it.
///
/// # Errors
///
/// [`DecodeError`] on truncation, unknown opcode byte, or unknown
/// operand kind tag.
pub fn decode_instruction(code: &[u8], pc: usize) -> Result<(Instruction, usize), DecodeError> {
    let opcode_byte = *code
        .get(pc)
        .ok_or(DecodeError::UnexpectedEnd { offset: pc })?;
    let op = op_from_byte(opcode_byte).ok_or(DecodeError::UnknownOpcode {
        offset: pc,
        byte: opcode_byte,
    })?;
    let operand_count = *code
        .get(pc + 1)
        .ok_or(DecodeError::UnexpectedEnd { offset: pc + 1 })? as usize;
    let mut cursor = pc + 2;
    let mut operands: Vec<Operand> = Vec::with_capacity(operand_count);
    for _ in 0..operand_count {
        let (operand, next) = decode_operand(code, cursor)?;
        operands.push(operand);
        cursor = next;
    }
    let instr = Instruction {
        pc: u32::try_from(pc).expect("pc fits in u32"),
        op,
        operands,
    };
    verify_operand_shape(instr.op, instr.operands.as_slice())
        .map_err(|error| DecodeError::InvalidOperandShape { offset: pc, error })?;
    Ok((instr, cursor))
}

fn decode_operand(code: &[u8], pc: usize) -> Result<(Operand, usize), DecodeError> {
    let kind = *code
        .get(pc)
        .ok_or(DecodeError::UnexpectedEnd { offset: pc })?;
    match kind {
        OPERAND_KIND_REGISTER => {
            let bytes = take_n::<2>(code, pc + 1)?;
            Ok((Operand::Register(u16::from_le_bytes(bytes)), pc + 3))
        }
        OPERAND_KIND_CONST_INDEX => {
            let bytes = take_n::<4>(code, pc + 1)?;
            Ok((Operand::ConstIndex(u32::from_le_bytes(bytes)), pc + 5))
        }
        OPERAND_KIND_IMM32 => {
            let bytes = take_n::<4>(code, pc + 1)?;
            Ok((Operand::Imm32(i32::from_le_bytes(bytes)), pc + 5))
        }
        other => Err(DecodeError::UnknownOperandKind {
            offset: pc,
            kind: other,
        }),
    }
}

fn take_n<const N: usize>(code: &[u8], pc: usize) -> Result<[u8; N], DecodeError> {
    let slice = code
        .get(pc..pc + N)
        .ok_or(DecodeError::UnexpectedEnd { offset: pc })?;
    let mut out = [0u8; N];
    out.copy_from_slice(slice);
    Ok(out)
}

/// Stable opcode → byte mapping. Returning `None` means the opcode
/// is not in [`OP_BYTE_TABLE`].
///
/// O(1) via the table's sequentiality invariant: rows are indexed by
/// their byte value, so the byte equals the row index.
#[must_use]
pub fn op_to_byte(op: Op) -> Option<u8> {
    OP_BYTE_TABLE
        .iter()
        .position(|(candidate, _)| *candidate == op)
        .map(|index| index as u8)
}

/// Reverse of [`op_to_byte`]. O(1) via direct table indexing.
#[must_use]
pub fn op_from_byte(byte: u8) -> Option<Op> {
    OP_BYTE_TABLE.get(byte as usize).map(|(op, _)| *op)
}

/// Generated byte assignments for every [`Op`] variant.
///
/// The declarative schema owns the rows; this re-export preserves the current
/// encoder/decoder API while preventing a second byte-assignment table.
pub use crate::opcode_schema::OP_BYTE_TABLE;

#[cfg(test)]
mod tests {
    use super::*;

    const NO_OVERFLOW: u32 = u32::MAX;

    fn make_instr(op: Op, operands: &[Operand]) -> Instruction {
        Instruction {
            pc: 0,
            op,
            operands: operands.to_vec(),
        }
    }

    fn roundtrip(instr: &Instruction) -> Instruction {
        let mut writer = BytecodeWriter::new();
        writer.write(instr);
        let bytes = writer.into_bytes();
        let (decoded, next_pc) = decode_instruction(&bytes, 0).expect("decode");
        assert_eq!(next_pc, bytes.len(), "decoder must consume full stream");
        decoded
    }

    fn raw_wordcode(
        instruction: WordInstruction,
        overflow_operand_words: Vec<u32>,
    ) -> FunctionCode {
        FunctionCode::from_raw_parts(vec![instruction], overflow_operand_words)
    }

    fn build_wordcode(instructions: Vec<(Op, Vec<Operand>)>) -> FunctionCode {
        let mut builder = crate::FunctionCodeBuilder::new();
        for (op, operands) in instructions {
            builder.push(op, &operands);
        }
        builder.finish()
    }

    #[test]
    fn wordcode_verifier_accepts_canonical_dense_overflow_storage() {
        let mut builder = crate::FunctionCodeBuilder::new();
        for destination in [0, 5] {
            builder.push(
                Op::MakeClass,
                &[
                    Operand::Register(destination),
                    Operand::Register(destination + 1),
                    Operand::Register(destination + 2),
                    Operand::Register(destination + 3),
                    Operand::Register(destination + 4),
                ],
            );
        }
        builder.push(Op::ReturnUndefined, &[]);
        let code = builder.finish();

        assert_eq!(verify_wordcode_function(&code), Ok(()));
        assert!(layout_wordcode_function(&code).is_ok());
    }

    #[test]
    fn wordcode_verifier_rejects_each_noncanonical_storage_form() {
        let inline_uses_overflow = raw_wordcode(
            WordInstruction::from_raw_parts(Op::LoadUndefined, 1, [0; 4], 0),
            vec![0],
        );
        assert!(matches!(
            verify_wordcode_function(&inline_uses_overflow),
            Err(VerifyError::InvalidOperandStorage {
                instruction_index: 0,
                error: OperandStorageError::InlineUsesOverflow {
                    operand_count: 1,
                    overflow_offset: 0,
                },
            })
        ));

        let missing_overflow = raw_wordcode(
            WordInstruction::from_raw_parts(Op::MakeClass, 5, [0; 4], NO_OVERFLOW),
            vec![0; 5],
        );
        assert!(matches!(
            verify_wordcode_function(&missing_overflow),
            Err(VerifyError::InvalidOperandStorage {
                instruction_index: 0,
                error: OperandStorageError::MissingOverflow { operand_count: 5 },
            })
        ));

        let out_of_bounds = raw_wordcode(
            WordInstruction::from_raw_parts(Op::MakeClass, 5, [0; 4], 0),
            vec![0; 4],
        );
        assert!(matches!(
            verify_wordcode_function(&out_of_bounds),
            Err(VerifyError::InvalidOperandStorage {
                instruction_index: 0,
                error: OperandStorageError::OverflowOutOfBounds {
                    overflow_offset: 0,
                    operand_count: 5,
                    available_words: 4,
                },
            })
        ));

        let noncanonical_offset = raw_wordcode(
            WordInstruction::from_raw_parts(Op::MakeClass, 5, [0; 4], 1),
            vec![0; 6],
        );
        assert!(matches!(
            verify_wordcode_function(&noncanonical_offset),
            Err(VerifyError::InvalidOperandStorage {
                instruction_index: 0,
                error: OperandStorageError::NonCanonicalOverflowOffset {
                    expected: 0,
                    actual: 1,
                },
            })
        ));

        let nonzero_padding = raw_wordcode(
            WordInstruction::from_raw_parts(Op::Nop, 0, [1, 0, 0, 0], NO_OVERFLOW),
            Vec::new(),
        );
        assert!(matches!(
            verify_wordcode_function(&nonzero_padding),
            Err(VerifyError::InvalidOperandStorage {
                instruction_index: 0,
                error: OperandStorageError::NonZeroInlinePadding {
                    word_index: 0,
                    word: 1,
                },
            })
        ));

        let invalid_register_word = raw_wordcode(
            WordInstruction::from_raw_parts(Op::LoadUndefined, 1, [u32::MAX, 0, 0, 0], NO_OVERFLOW),
            Vec::new(),
        );
        assert!(matches!(
            verify_wordcode_function(&invalid_register_word),
            Err(VerifyError::InvalidOperandStorage {
                instruction_index: 0,
                error: OperandStorageError::InvalidOperandWord {
                    operand_index: 0,
                    expected: OperandKind::Register,
                    word: u32::MAX,
                },
            })
        ));

        let trailing_overflow = raw_wordcode(
            WordInstruction::from_raw_parts(Op::Nop, 0, [0; 4], NO_OVERFLOW),
            vec![0],
        );
        assert!(matches!(
            verify_wordcode_function(&trailing_overflow),
            Err(VerifyError::TrailingOverflowOperandWords {
                first_unused_word: 0,
                total_words: 1,
            })
        ));
    }

    #[test]
    fn wordcode_verifier_rejects_hostile_raw_fields_without_panicking() {
        let hostile_bodies = [
            raw_wordcode(
                WordInstruction::from_raw_parts(Op::MakeClass, u8::MAX, [0; 4], u32::MAX - 1),
                Vec::new(),
            ),
            raw_wordcode(
                WordInstruction::from_raw_parts(
                    Op::LoadUndefined,
                    1,
                    [u32::MAX, 0, 0, 0],
                    NO_OVERFLOW,
                ),
                Vec::new(),
            ),
            raw_wordcode(
                WordInstruction::from_raw_parts(Op::Nop, 4, [0; 4], NO_OVERFLOW),
                Vec::new(),
            ),
        ];

        for code in hostile_bodies {
            let result = std::panic::catch_unwind(|| verify_wordcode_function(&code));
            assert!(
                matches!(result, Ok(Err(_))),
                "hostile wordcode must return a typed error: {result:?}"
            );
        }
    }

    #[test]
    fn wordcode_measurement_rejects_malformed_storage_without_operand_view() {
        let code = raw_wordcode(
            WordInstruction::from_raw_parts(Op::MakeClass, 5, [0; 4], u32::MAX - 1),
            Vec::new(),
        );

        assert!(matches!(
            measure_wordcode_function(&code),
            Err(VerifyError::InvalidOperandStorage {
                error: OperandStorageError::OverflowOutOfBounds { .. },
                ..
            })
        ));
    }

    #[test]
    fn wordcode_layout_always_verifies_control_flow() {
        let mut builder = crate::FunctionCodeBuilder::new();
        builder.push(Op::Jump, &[Operand::Imm32(-2)]);
        let code = builder.finish();

        assert!(matches!(
            layout_wordcode_function(&code),
            Err(VerifyError::InvalidControlFlowTarget {
                instruction_index: 0,
                target: -1,
            })
        ));
    }

    #[test]
    fn wordcode_verifier_rejects_unbalanced_handler_structure() {
        let underflow = build_wordcode(vec![(Op::LeaveTry, vec![]), (Op::ReturnUndefined, vec![])]);
        assert!(matches!(
            verify_wordcode_function(&underflow),
            Err(VerifyError::HandlerStackUnderflow {
                instruction_index: 0
            })
        ));

        let unclosed = build_wordcode(vec![
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(0),
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Register(0),
                ],
            ),
            (Op::ReturnUndefined, vec![]),
        ]);
        assert!(matches!(
            verify_wordcode_function(&unclosed),
            Err(VerifyError::UnclosedHandler {
                enter_instruction_index: 0
            })
        ));
    }

    #[test]
    fn wordcode_verifier_rejects_incompatible_handler_join() {
        // The taken path reaches instruction 4 with no handler. The other
        // path jumps over the matching LeaveTry while the handler is live.
        let code = build_wordcode(vec![
            (
                Op::JumpIfTrue,
                vec![Operand::Imm32(3), Operand::Register(0)],
            ),
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(3),
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Register(0),
                ],
            ),
            (Op::Jump, vec![Operand::Imm32(1)]),
            (Op::LeaveTry, vec![]),
            (Op::ReturnUndefined, vec![]),
            (Op::ReturnUndefined, vec![]),
        ]);

        assert!(matches!(
            verify_wordcode_function(&code),
            Err(VerifyError::IncompatibleHandlerStackStates {
                instruction_index: 4,
                first_depth: 0,
                conflicting_depth: 1,
                ..
            })
        ));
    }

    #[test]
    fn wordcode_verifier_rejects_invalid_handler_target_and_empty_handler() {
        let target_at_end = build_wordcode(vec![
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(2),
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Register(0),
                ],
            ),
            (Op::LeaveTry, vec![]),
            (Op::ReturnUndefined, vec![]),
        ]);
        assert!(matches!(
            verify_wordcode_function(&target_at_end),
            Err(VerifyError::InvalidHandlerTarget {
                instruction_index: 0,
                operand_index: 0,
                target: 3,
            })
        ));

        let empty_handler = build_wordcode(vec![
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Register(0),
                ],
            ),
            (Op::LeaveTry, vec![]),
            (Op::ReturnUndefined, vec![]),
        ]);
        assert!(matches!(
            verify_wordcode_function(&empty_handler),
            Err(VerifyError::HandlerWithoutTarget {
                instruction_index: 0
            })
        ));
    }

    #[test]
    fn wordcode_verifier_rejects_bad_finally_floor() {
        let code = build_wordcode(vec![
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Imm32(2),
                    Operand::Register(0),
                ],
            ),
            (
                Op::JumpViaFinally,
                vec![Operand::Imm32(2), Operand::Imm32(2)],
            ),
            (Op::LeaveTry, vec![]),
            (Op::EndFinally, vec![]),
            (Op::ReturnUndefined, vec![]),
        ]);

        assert!(matches!(
            verify_wordcode_function(&code),
            Err(VerifyError::InvalidFinallyFloor {
                instruction_index: 1,
                floor: 2,
                handler_depth: 1,
            })
        ));
    }

    #[test]
    fn wordcode_verifier_rejects_reachable_function_end_and_stale_finally_state() {
        let fallthrough = build_wordcode(vec![(Op::Nop, vec![])]);
        assert!(matches!(
            verify_wordcode_function(&fallthrough),
            Err(VerifyError::ReachableFunctionEnd {
                predecessor: Some(0),
                handler_depth: 0,
                parked_finally_depth: 0,
            })
        ));

        let stale_parked = build_wordcode(vec![
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Imm32(1),
                    Operand::Register(0),
                ],
            ),
            (Op::LeaveTry, vec![]),
            (Op::Jump, vec![Operand::Imm32(2)]),
            (Op::EndFinally, vec![]),
            (Op::ReturnUndefined, vec![]),
        ]);
        assert!(matches!(
            verify_wordcode_function(&stale_parked),
            Err(VerifyError::ReachableFunctionEnd {
                predecessor: Some(2),
                handler_depth: 0,
                parked_finally_depth: 1,
            })
        ));
    }

    #[test]
    fn wordcode_abrupt_analysis_budget_is_typed() {
        let code = build_wordcode(vec![(Op::ReturnUndefined, vec![])]);
        let (instructions, _) = code.raw_parts();
        let operands = vec![Vec::<Operand>::new().into_boxed_slice()];
        let mut verifier = WordcodeControlFlowVerifier::new(instructions, &operands).unwrap();
        verifier.abrupt_transitions = MAX_WORDCODE_ABRUPT_TRANSITIONS;

        assert!(matches!(
            verifier.charge_abrupt_transition(0),
            Err(VerifyError::ControlFlowAnalysisLimitExceeded {
                instruction_index: 0,
                limit: MAX_WORDCODE_ABRUPT_TRANSITIONS,
            })
        ));
    }

    #[test]
    fn wordcode_verifier_rejects_abrupt_completion_underflow_and_join() {
        for (op, operands, requested) in [
            (Op::EndFinally, vec![], 1),
            (Op::PopParkedFinally, vec![Operand::Imm32(1)], 1),
        ] {
            let code = build_wordcode(vec![(op, operands), (Op::ReturnUndefined, vec![])]);
            assert!(matches!(
                verify_wordcode_function(&code),
                Err(VerifyError::AbruptCompletionUnderflow {
                    instruction_index: 0,
                    requested: actual,
                    available: 0,
                    ..
                }) if actual == requested
            ));
        }

        // EndFinally consumes the parked record and then loops back to itself,
        // joining the original parked state with an empty one.
        let incompatible = build_wordcode(vec![
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Imm32(1),
                    Operand::Register(0),
                ],
            ),
            (Op::LeaveTry, vec![]),
            (Op::EndFinally, vec![]),
            (
                Op::JumpIfTrue,
                vec![Operand::Imm32(-2), Operand::Register(0)],
            ),
            (Op::ReturnUndefined, vec![]),
        ]);
        assert!(matches!(
            verify_wordcode_function(&incompatible),
            Err(VerifyError::IncompatibleAbruptCompletionStates {
                instruction_index: 2,
                first_depth: 1,
                conflicting_depth: 0,
                ..
            })
        ));
    }

    #[test]
    fn wordcode_yield_return_skips_catch_and_reaches_outer_finally() {
        // GeneratorResumeAbrupt(return) at Yield does not execute the inner
        // catch. It removes that catch-only handler and enters the outer
        // finally. Both ordinary continuation and generator `.throw()` loop
        // forever here, so only the external `.return()` edge can expose the
        // malformed fallthrough at the finalizer target.
        let code = build_wordcode(vec![
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Imm32(7),
                    Operand::Register(0),
                ],
            ),
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(3),
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Register(0),
                ],
            ),
            (Op::Yield, vec![Operand::Register(0), Operand::Register(1)]),
            (Op::Jump, vec![Operand::Imm32(-1)]),
            (Op::LeaveTry, vec![]),
            (Op::Jump, vec![Operand::Imm32(-1)]),
            (Op::LeaveTry, vec![]),
            (Op::ReturnUndefined, vec![]),
            (Op::Nop, vec![]),
        ]);

        assert!(matches!(
            verify_wordcode_function(&code),
            Err(VerifyError::ReachableFunctionEnd {
                predecessor: Some(8),
                handler_depth: 0,
                parked_finally_depth: 1,
            })
        ));
    }

    #[test]
    fn wordcode_load_this_tdz_reaches_catch_and_malformed_tail() {
        // The ordinary path leaves the handler and returns. A derived-`this`
        // hole instead throws from LoadThis, so the otherwise dead catch target
        // exposes the unterminated tail.
        let code = build_wordcode(vec![
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(3),
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Register(0),
                ],
            ),
            (Op::LoadThis, vec![Operand::Register(0)]),
            (Op::LeaveTry, vec![]),
            (Op::ReturnUndefined, vec![]),
            (Op::Nop, vec![]),
        ]);

        assert!(matches!(
            verify_wordcode_function(&code),
            Err(VerifyError::ReachableFunctionEnd {
                predecessor: Some(4),
                handler_depth: 0,
                parked_finally_depth: 0,
            })
        ));
    }

    #[test]
    fn wordcode_return_caller_exception_does_not_reach_local_catch() {
        // Derived-constructor validation (and async completion settlement)
        // happens after the returning activation is gone. Even though Return
        // may throw into its caller, the current frame's catch target remains
        // dead; making it reachable would expose this deliberately malformed
        // EOF and reject valid return control flow.
        let code = build_wordcode(vec![
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(2),
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Register(0),
                ],
            ),
            (Op::ReturnUndefined, vec![]),
            (Op::LeaveTry, vec![]),
            (Op::Nop, vec![]),
        ]);

        assert_eq!(verify_wordcode_function(&code), Ok(()));
    }

    #[test]
    fn abrupt_only_finally_does_not_manufacture_fallthrough() {
        let code = build_wordcode(vec![
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Imm32(2),
                    Operand::Register(0),
                ],
            ),
            (Op::ReturnUndefined, vec![]),
            (Op::LeaveTry, vec![]),
            (Op::Nop, vec![]),
            (Op::EndFinally, vec![]),
            // The return parked at instruction 1 resumes at EndFinally, so this
            // deliberately unterminated dead tail is not a normal successor.
            (Op::Nop, vec![]),
        ]);

        assert_eq!(verify_wordcode_function(&code), Ok(()));
    }

    #[test]
    fn finally_join_unions_normal_and_abrupt_completion_kinds() {
        let code = build_wordcode(vec![
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Imm32(3),
                    Operand::Register(0),
                ],
            ),
            (
                Op::JumpIfTrue,
                vec![Operand::Imm32(1), Operand::Register(0)],
            ),
            (Op::ReturnUndefined, vec![]),
            (Op::LeaveTry, vec![]),
            (Op::Nop, vec![]),
            (Op::EndFinally, vec![]),
            (Op::ReturnUndefined, vec![]),
        ]);

        assert_eq!(verify_wordcode_function(&code), Ok(()));
    }

    #[test]
    fn abrupt_only_nested_finally_chain_does_not_fall_through() {
        let code = build_wordcode(vec![
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Imm32(5),
                    Operand::Register(0),
                ],
            ),
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Imm32(2),
                    Operand::Register(0),
                ],
            ),
            (Op::ReturnUndefined, vec![]),
            (Op::LeaveTry, vec![]),
            (Op::EndFinally, vec![]),
            (Op::LeaveTry, vec![]),
            (Op::EndFinally, vec![]),
            (Op::Nop, vec![]),
        ]);

        assert_eq!(verify_wordcode_function(&code), Ok(()));
    }

    #[test]
    fn wordcode_verifier_accepts_nested_try_catch_finally_loop_and_abrupt_paths() {
        // Compiler-shaped outer finally + inner catch. LoadProperty supplies
        // the dynamic throw edge; the backedge stays inside the inner region;
        // JumpViaFinally summarizes the break path through the outer finally.
        let code = build_wordcode(vec![
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Imm32(10),
                    Operand::Register(0),
                ],
            ),
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(5),
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Register(0),
                ],
            ),
            (
                Op::LoadProperty,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(0),
                ],
            ),
            (
                Op::JumpIfTrue,
                vec![Operand::Imm32(1), Operand::Register(0)],
            ),
            (Op::Jump, vec![Operand::Imm32(-3)]),
            (Op::LeaveTry, vec![]),
            (Op::Jump, vec![Operand::Imm32(1)]),
            (Op::Nop, vec![]),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(1), Operand::Register(0)],
            ),
            (
                Op::JumpViaFinally,
                vec![Operand::Imm32(3), Operand::Imm32(0)],
            ),
            (Op::LeaveTry, vec![]),
            (Op::Nop, vec![]),
            (Op::EndFinally, vec![]),
            (Op::ReturnUndefined, vec![]),
        ]);
        assert_eq!(verify_wordcode_function(&code), Ok(()));

        let return_through_finally = build_wordcode(vec![
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Imm32(2),
                    Operand::Register(0),
                ],
            ),
            (Op::ReturnUndefined, vec![]),
            (Op::LeaveTry, vec![]),
            (Op::EndFinally, vec![]),
            (Op::ReturnUndefined, vec![]),
        ]);
        assert_eq!(verify_wordcode_function(&return_through_finally), Ok(()));
    }

    #[test]
    fn roundtrip_load_undefined() {
        let instr = make_instr(Op::LoadUndefined, &[Operand::Register(7)]);
        assert_eq!(roundtrip(&instr), instr);
    }

    #[test]
    fn roundtrip_load_int32() {
        let instr = make_instr(Op::LoadInt32, &[Operand::Register(0), Operand::Imm32(-42)]);
        assert_eq!(roundtrip(&instr), instr);
    }

    #[test]
    fn roundtrip_add_three_registers() {
        let instr = make_instr(
            Op::Add,
            &[
                Operand::Register(0),
                Operand::Register(1),
                Operand::Register(2),
            ],
        );
        assert_eq!(roundtrip(&instr), instr);
    }

    #[test]
    fn multi_instruction_stream_steps_pc_by_byte_size() {
        let mut writer = BytecodeWriter::new();
        writer.write(&make_instr(Op::Nop, &[]));
        writer.write(&make_instr(Op::LoadUndefined, &[Operand::Register(1)]));
        writer.write(&make_instr(
            Op::LoadInt32,
            &[Operand::Register(2), Operand::Imm32(7)],
        ));
        let bytes = writer.into_bytes();

        let mut pc = 0;
        let (first, next) = decode_instruction(&bytes, pc).unwrap();
        assert_eq!(first.op, Op::Nop);
        pc = next;

        let (second, next) = decode_instruction(&bytes, pc).unwrap();
        assert_eq!(second.op, Op::LoadUndefined);
        pc = next;

        let (third, next) = decode_instruction(&bytes, pc).unwrap();
        assert_eq!(third.op, Op::LoadInt32);
        assert_eq!(next, bytes.len());
    }

    #[test]
    fn direct_layout_matches_serialized_encoder_boundaries() {
        let instructions = vec![
            make_instr(Op::LoadUndefined, &[Operand::Register(1)]),
            make_instr(
                Op::LoadInt32,
                &[Operand::Register(2), Operand::Imm32(i32::MIN)],
            ),
            make_instr(
                Op::Call,
                &[
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(1),
                    Operand::Register(2),
                ],
            ),
        ];
        let layout = layout_function(&instructions).unwrap();
        let encoded = try_encode_function(&instructions).unwrap();

        assert_eq!(layout.instr_to_byte_pc, encoded.instr_to_byte_pc);
        assert_eq!(layout.total_bytes as usize, encoded.code.len());
    }

    #[test]
    fn truncated_stream_surfaces_clean_error() {
        let mut writer = BytecodeWriter::new();
        writer.write(&make_instr(
            Op::LoadInt32,
            &[Operand::Register(0), Operand::Imm32(0)],
        ));
        let bytes = writer.into_bytes();
        let truncated = &bytes[..bytes.len() - 1];
        match decode_instruction(truncated, 0) {
            Err(DecodeError::UnexpectedEnd { .. }) => {}
            other => panic!("expected UnexpectedEnd, got {other:?}"),
        }
    }

    #[test]
    fn unknown_opcode_byte_rejected() {
        let bytes = [0xFFu8, 0];
        match decode_instruction(&bytes, 0) {
            Err(DecodeError::UnknownOpcode { byte: 0xFF, .. }) => {}
            other => panic!("expected UnknownOpcode, got {other:?}"),
        }
    }

    #[test]
    fn authoritative_shape_rejects_wrong_operand_count() {
        let bytes = [0x01u8, 0]; // LoadUndefined requires one write register.
        assert!(matches!(
            decode_instruction(&bytes, 0),
            Err(DecodeError::InvalidOperandShape {
                error: OperandShapeError::Count {
                    expected: 1,
                    actual: 0
                },
                ..
            })
        ));
    }

    #[test]
    fn authoritative_shape_rejects_wrong_operand_kind() {
        let bytes = [0x06u8, 2, 0, 0, 0, 1, 0, 0, 0, 0];
        assert!(matches!(
            decode_instruction(&bytes, 0),
            Err(DecodeError::InvalidOperandShape {
                error: OperandShapeError::Kind {
                    index: 1,
                    expected: crate::opcode_schema::OperandKind::Imm32,
                    actual: crate::opcode_schema::OperandKind::ConstIndex,
                },
                ..
            })
        ));
    }

    #[test]
    fn authoritative_variadic_shape_rejects_count_mismatch() {
        let mut writer = BytecodeWriter::new();
        writer.write(&make_instr(
            Op::Call,
            &[
                Operand::Register(0),
                Operand::Register(1),
                Operand::ConstIndex(2),
                Operand::Register(2),
            ],
        ));
        let bytes = writer.into_bytes();
        assert!(matches!(
            decode_instruction(&bytes, 0),
            Err(DecodeError::InvalidOperandShape {
                error: OperandShapeError::Count {
                    expected: 5,
                    actual: 4
                },
                ..
            })
        ));
    }

    #[test]
    fn authoritative_variadic_shape_rejects_wrong_tail_kind() {
        let mut writer = BytecodeWriter::new();
        writer.write(&make_instr(
            Op::TailCall,
            &[
                Operand::Register(0),
                Operand::Register(1),
                Operand::ConstIndex(1),
                Operand::Imm32(2),
            ],
        ));
        let bytes = writer.into_bytes();
        assert!(matches!(
            decode_instruction(&bytes, 0),
            Err(DecodeError::InvalidOperandShape {
                error: OperandShapeError::Kind {
                    index: 3,
                    expected: crate::opcode_schema::OperandKind::Register,
                    actual: crate::opcode_schema::OperandKind::Imm32,
                },
                ..
            })
        ));
    }

    #[test]
    fn authoritative_variadic_shape_round_trips() {
        let instr = make_instr(
            Op::CallWithThis,
            &[
                Operand::Register(0),
                Operand::Register(1),
                Operand::Register(2),
                Operand::ConstIndex(2),
                Operand::Register(3),
                Operand::Register(4),
            ],
        );
        assert_eq!(roundtrip(&instr), instr);
    }

    #[test]
    fn authoritative_jump_rejects_target_inside_instruction() {
        let mut writer = BytecodeWriter::new();
        // Jump byte PC is 0 and relative deltas use base PC+1. Nop starts at
        // byte 7, so delta 7 resolves to byte 8: the Nop operand-count byte.
        writer.write(&make_instr(Op::Jump, &[Operand::Imm32(7)]));
        writer.write(&make_instr(Op::Nop, &[]));
        let bytes = writer.into_bytes();
        assert!(matches!(
            decode_function(&bytes),
            Err(DecodeError::InvalidControlFlowTarget {
                offset: 0,
                target: 8
            })
        ));
    }

    #[test]
    fn authoritative_jump_accepts_instruction_and_end_boundaries() {
        let instructions = [
            make_instr(Op::Jump, &[Operand::Imm32(0)]),
            make_instr(Op::Nop, &[]),
        ];
        let encoded = encode_function(&instructions);
        assert!(decode_function(&encoded.code).is_ok());

        let end_jump = encode_function(&[make_instr(Op::Jump, &[Operand::Imm32(0)])]);
        assert!(decode_function(&end_jump.code).is_ok());
    }

    #[test]
    fn authoritative_enter_try_rejects_handler_inside_instruction() {
        let mut writer = BytecodeWriter::new();
        // EnterTry occupies bytes 0..15 and Nop begins at 15. Base PC+1 plus
        // delta 15 resolves to byte 16, inside the Nop encoding.
        writer.write(&make_instr(
            Op::EnterTry,
            &[
                Operand::Imm32(15),
                Operand::Imm32(NO_HANDLER_OFFSET),
                Operand::Register(0),
            ],
        ));
        writer.write(&make_instr(Op::Nop, &[]));
        let bytes = writer.into_bytes();
        assert!(matches!(
            decode_function(&bytes),
            Err(DecodeError::InvalidControlFlowTarget {
                offset: 0,
                target: 16
            })
        ));
    }

    #[test]
    fn authoritative_enter_try_accepts_absent_handler_sentinels() {
        let instructions = [
            make_instr(
                Op::EnterTry,
                &[
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Register(0),
                ],
            ),
            make_instr(Op::EndFinally, &[]),
            make_instr(Op::ReturnUndefined, &[]),
        ];
        let encoded = encode_function(&instructions);
        assert!(decode_function(&encoded.code).is_ok());
    }

    #[test]
    fn jump_via_finally_uses_the_shared_branch_fixup() {
        let instructions = [
            make_instr(Op::JumpViaFinally, &[Operand::Imm32(1), Operand::Imm32(0)]),
            make_instr(Op::Nop, &[]),
            make_instr(Op::ReturnUndefined, &[]),
        ];
        let encoded = encode_function(&instructions);
        let (jump, _) = decode_instruction(&encoded.code, 0).expect("decode jump-via-finally");
        let Operand::Imm32(delta) = jump.operands.as_slice()[0] else {
            panic!("target must remain Imm32")
        };
        assert_eq!(
            (i64::from(jump.pc) + 1 + i64::from(delta)) as u32,
            encoded.instr_to_byte_pc[2]
        );
        assert!(decode_function(&encoded.code).is_ok());
    }

    #[test]
    fn jump_via_finally_rejects_negative_handler_floor() {
        let mut writer = BytecodeWriter::new();
        // A single instruction is 12 bytes, so delta 11 targets end-of-stream.
        writer.write(&make_instr(
            Op::JumpViaFinally,
            &[Operand::Imm32(11), Operand::Imm32(-1)],
        ));
        let bytes = writer.into_bytes();
        assert!(matches!(
            decode_function(&bytes),
            Err(DecodeError::InvalidControlFlowOperand {
                offset: 0,
                operand_index: 1,
                value: -1
            })
        ));
    }

    #[test]
    fn op_byte_table_round_trips() {
        for (op, byte) in OP_BYTE_TABLE {
            assert_eq!(op_to_byte(*op), Some(*byte));
            assert_eq!(op_from_byte(*byte), Some(*op));
        }
    }

    #[test]
    fn op_byte_assignments_unique() {
        let mut seen = std::collections::HashSet::new();
        for (op, byte) in OP_BYTE_TABLE {
            assert!(
                seen.insert(*byte),
                "byte 0x{:02X} assigned to multiple opcodes (offending: {:?})",
                byte,
                op
            );
        }
    }

    #[test]
    fn op_byte_assignments_are_sequential() {
        // Stable wire format requires monotonic byte assignments so
        // diffs on this table read as a single growing column.
        for (i, (_, byte)) in OP_BYTE_TABLE.iter().enumerate() {
            assert_eq!(
                *byte as usize, i,
                "OP_BYTE_TABLE row {i} has byte 0x{byte:02X}; table must stay dense"
            );
        }
    }

    #[test]
    fn op_byte_assignments_have_unique_opcodes() {
        let mut seen = std::collections::HashSet::new();
        for (op, _) in OP_BYTE_TABLE {
            assert!(
                seen.insert(*op),
                "opcode {op:?} appears twice in OP_BYTE_TABLE"
            );
        }
    }

    #[test]
    fn encode_decode_function_roundtrip() {
        let instructions = vec![
            make_instr(Op::Nop, &[]),
            make_instr(Op::LoadUndefined, &[Operand::Register(0)]),
            make_instr(Op::LoadInt32, &[Operand::Register(1), Operand::Imm32(42)]),
            make_instr(
                Op::Add,
                &[
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            make_instr(Op::Return, &[Operand::Register(2)]),
        ];
        let encoded = encode_function(&instructions);
        assert_eq!(encoded.instr_to_byte_pc.len(), instructions.len());
        // First instruction always lands at byte 0.
        assert_eq!(encoded.instr_to_byte_pc[0], 0);
        // Byte offsets must be strictly monotonic.
        for win in encoded.instr_to_byte_pc.windows(2) {
            assert!(win[0] < win[1]);
        }

        let decoded = decode_function(&encoded.code).expect("decode");
        // Re-stamp pc since round-trip through bytes flips PC from
        // instruction-index to byte-offset; the structural data should
        // otherwise match exactly.
        for (i, (orig, decoded)) in instructions.iter().zip(decoded.iter()).enumerate() {
            assert_eq!(orig.op, decoded.op, "op mismatch at index {i}");
            assert_eq!(
                orig.operands.as_slice(),
                decoded.operands.as_slice(),
                "operands mismatch at index {i}"
            );
            assert_eq!(
                decoded.pc, encoded.instr_to_byte_pc[i],
                "decoded pc must match the byte-offset map"
            );
        }
    }

    #[test]
    fn forward_jump_is_rewritten_to_byte_offset_delta() {
        // Layout: instr 0 = LoadInt32 (size 1 + 1 + 3 + 5 = 10 bytes),
        // instr 1 = Jump +1 (target = instr 3) (size 1 + 1 + 5 = 7),
        // instr 2 = LoadInt32 (10 bytes),
        // instr 3 = Return (1 + 1 + 3 = 5 bytes).
        let instructions = vec![
            make_instr(Op::LoadInt32, &[Operand::Register(0), Operand::Imm32(7)]),
            make_instr(Op::Jump, &[Operand::Imm32(1)]),
            make_instr(Op::LoadInt32, &[Operand::Register(1), Operand::Imm32(8)]),
            make_instr(Op::Return, &[Operand::Register(0)]),
        ];
        let encoded = encode_function(&instructions);
        let jump_byte_pc = encoded.instr_to_byte_pc[1];
        let target_byte_pc = encoded.instr_to_byte_pc[3];
        // Re-decode the jump and confirm its Imm32 is the byte-offset
        // delta from `(jump_pc + 1)` to the target.
        let (decoded_jump, _) = decode_instruction(&encoded.code, jump_byte_pc as usize).unwrap();
        let Operand::Imm32(byte_delta) = decoded_jump.operands.as_slice()[0] else {
            panic!("jump operand must remain Imm32");
        };
        let resolved_target = (jump_byte_pc as i64) + 1 + (byte_delta as i64);
        assert_eq!(resolved_target as u32, target_byte_pc);
    }

    #[test]
    fn backward_jump_byte_delta_is_negative() {
        // instr 0 = LoadInt32 (10),
        // instr 1 = Return (5),
        // instr 2 = Jump -2 (target = instr 1)
        let instructions = vec![
            make_instr(Op::LoadInt32, &[Operand::Register(0), Operand::Imm32(0)]),
            make_instr(Op::Return, &[Operand::Register(0)]),
            make_instr(Op::Jump, &[Operand::Imm32(-2)]),
        ];
        let encoded = encode_function(&instructions);
        let jump_byte_pc = encoded.instr_to_byte_pc[2];
        let target_byte_pc = encoded.instr_to_byte_pc[1];
        let (decoded_jump, _) = decode_instruction(&encoded.code, jump_byte_pc as usize).unwrap();
        let Operand::Imm32(byte_delta) = decoded_jump.operands.as_slice()[0] else {
            panic!("jump operand must remain Imm32");
        };
        assert!(
            byte_delta < 0,
            "expected backward branch delta, got {byte_delta}"
        );
        let resolved_target = (jump_byte_pc as i64) + 1 + (byte_delta as i64);
        assert_eq!(resolved_target as u32, target_byte_pc);
    }

    #[test]
    fn enter_try_no_handler_sentinel_preserved() {
        let instructions = vec![
            make_instr(
                Op::EnterTry,
                &[
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Register(0),
                ],
            ),
            make_instr(Op::LeaveTry, &[]),
            make_instr(Op::Return, &[Operand::Register(0)]),
        ];
        let encoded = encode_function(&instructions);
        let (decoded, _) = decode_instruction(&encoded.code, 0).unwrap();
        let operands = decoded.operands.as_slice();
        assert_eq!(operands[0], Operand::Imm32(NO_HANDLER_OFFSET));
        assert_eq!(operands[1], Operand::Imm32(NO_HANDLER_OFFSET));
        assert_eq!(operands[2], Operand::Register(0));
    }

    #[test]
    fn enter_try_handler_offsets_rewritten_to_byte_pcs() {
        // instr 0 = EnterTry catch=+1 finally=NO_HANDLER (size = 1+1+5+5+3 = 15)
        // instr 1 = LoadInt32 (size 10)
        // instr 2 = LeaveTry (size 1+1 = 2)        ← catch target
        // instr 3 = Return (size 5)
        let instructions = vec![
            make_instr(
                Op::EnterTry,
                &[
                    Operand::Imm32(1), // catch_offset (instr-index delta = +1 → target = idx 2)
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Register(7),
                ],
            ),
            make_instr(Op::LoadInt32, &[Operand::Register(0), Operand::Imm32(0)]),
            make_instr(Op::LeaveTry, &[]),
            make_instr(Op::Return, &[Operand::Register(0)]),
        ];
        let encoded = encode_function(&instructions);
        let try_byte_pc = encoded.instr_to_byte_pc[0];
        let leave_byte_pc = encoded.instr_to_byte_pc[2];
        let (decoded, _) = decode_instruction(&encoded.code, try_byte_pc as usize).unwrap();
        let operands = decoded.operands.as_slice();
        let Operand::Imm32(catch_byte_delta) = operands[0] else {
            panic!("catch operand must remain Imm32");
        };
        assert_eq!(operands[1], Operand::Imm32(NO_HANDLER_OFFSET));
        assert_eq!(operands[2], Operand::Register(7));
        let resolved = (try_byte_pc as i64) + 1 + (catch_byte_delta as i64);
        assert_eq!(resolved as u32, leave_byte_pc);
    }

    #[test]
    fn jump_past_last_instruction_lands_at_stream_end() {
        // Jump +0 from a last-position instruction equals "fall off"
        // (target = instructions.len()). Encoder maps that to the end
        // of the byte stream so unwind / source-map clients see a
        // stable PC.
        let instructions = vec![
            make_instr(Op::Nop, &[]),
            make_instr(Op::Jump, &[Operand::Imm32(0)]),
        ];
        let encoded = encode_function(&instructions);
        let total_len = encoded.code.len() as u32;
        let jump_byte_pc = encoded.instr_to_byte_pc[1];
        let (decoded, _) = decode_instruction(&encoded.code, jump_byte_pc as usize).unwrap();
        let Operand::Imm32(delta) = decoded.operands.as_slice()[0] else {
            panic!("jump operand kind");
        };
        let resolved = (jump_byte_pc as i64) + 1 + (delta as i64);
        assert_eq!(resolved as u32, total_len);
    }

    #[test]
    fn translate_spans_maps_to_byte_offsets() {
        // Three instructions: LoadInt32 (10), Nop (2), Return (5).
        let instructions = vec![
            make_instr(Op::LoadInt32, &[Operand::Register(0), Operand::Imm32(0)]),
            make_instr(Op::Nop, &[]),
            make_instr(Op::Return, &[Operand::Register(0)]),
        ];
        let encoded = encode_function(&instructions);
        let spans = vec![
            SpanEntry {
                pc: 0,
                span: (10, 20),
            },
            SpanEntry {
                pc: 1,
                span: (20, 25),
            },
            SpanEntry {
                pc: 2,
                span: (25, 30),
            },
        ];
        let translated = translate_spans_to_byte_pcs(
            &spans,
            &encoded.instr_to_byte_pc,
            encoded.code.len() as u32,
        );
        assert_eq!(translated.len(), spans.len());
        for (i, entry) in translated.iter().enumerate() {
            assert_eq!(entry.pc, encoded.instr_to_byte_pc[i]);
            assert_eq!(entry.span, spans[i].span);
        }
    }

    #[test]
    fn translate_spans_out_of_range_pc_falls_back_to_stream_end() {
        let instructions = vec![make_instr(Op::Nop, &[])];
        let encoded = encode_function(&instructions);
        let total = encoded.code.len() as u32;
        let spans = vec![SpanEntry {
            pc: 5,
            span: (0, 1),
        }];
        let translated = translate_spans_to_byte_pcs(&spans, &encoded.instr_to_byte_pc, total);
        assert_eq!(translated[0].pc, total);
    }
}
