//! Feedback-guided machine representations for backend-independent SSA.
//!
//! Purpose: select a proven representation for every SSA value and describe
//! the checked guards or lossless conversions required at instruction uses.
//!
//! # Contents
//! - [`Representation`] — the widening lattice for SSA values.
//! - [`ConversionKind`] and [`Conversion`] — required input adaptations.
//! - [`ReprMap`] — deterministic analysis output and its pure verifier.
//! - [`ReprError`] — precise verification failures.
//!
//! # Invariants
//! - Representations widen monotonically as `Int32 < Float64 < Tagged`.
//! - Phi representations are the least upper bound of all incoming values.
//! - `LoadLocal` / `StoreLocal` preserve their input representation; local
//!   bytecode moves never force an otherwise numeric SSA value to be boxed.
//! - Speculative numeric representations require arithmetic feedback from the
//!   immutable compile snapshot.
//! - Conversions are complete, unique, and ordered by canonical PC and operand.
//! - This analysis emits no machine code and has no runtime side effects.
//!
//! # See also
//! - [`super::ssa`] — the SSA graph analyzed here.
//! - [`otter_vm::jit_feedback::ArithFeedback`] — arithmetic observations.
//! - [`crate::ir::inline::InlineTree`] — the frames whose feedback is read.

use std::collections::BTreeMap;

use otter_bytecode::Op;
use otter_vm::{JitInstructionMetadata, jit_feedback::ArithFeedback};

use super::{
    inline::{InlineId, InlineTree},
    ssa::{SsaFunction, ValueDef, ValueId},
};

/// The constant a `LoadNumber` in one frame materializes.
fn load_number_at(tree: &InlineTree, inline: InlineId, pc: u32) -> Option<f64> {
    tree.frames[inline.0 as usize]
        .instructions
        .get(pc as usize)
        .and_then(|instruction| instruction.load_number)
}

/// Arithmetic feedback for one instruction of one frame.
///
/// A spliced callee carries its own feedback overlay, so a unit-wide lookup
/// against the root body would read another function's cell.
fn feedback_at(tree: &InlineTree, inline: InlineId, pc: u32) -> ArithFeedback {
    tree.frames[inline.0 as usize]
        .instructions
        .get(pc as usize)
        .map_or_else(ArithFeedback::default, JitInstructionMetadata::arith_feedback)
}

/// Machine representation selected for one SSA value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Representation {
    /// Unboxed signed 32-bit integer.
    Int32,
    /// Unboxed IEEE-754 double.
    Float64,
    /// Ordinary boxed VM value.
    Tagged,
}

/// Adaptation required at one SSA instruction input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionKind {
    /// Guard and unbox a tagged value as an `Int32`.
    CheckedTaggedToInt32,
    /// Guard and unbox a tagged value as a `Float64` number.
    CheckedTaggedToFloat64,
    /// Widen an `Int32` to `Float64` without loss.
    Int32ToFloat64,
    /// Box an `Int32` as a tagged VM value.
    BoxInt32,
    /// Box a `Float64` as a tagged VM value.
    BoxFloat64,
}

/// One conversion or guard required by an instruction operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conversion {
    /// Frame owning the use; [`Self::at_pc`] is canonical within it.
    pub inline: InlineId,
    /// Canonical instruction PC owning the use.
    pub at_pc: u32,
    /// Index in the instruction's SSA input list.
    pub operand_index: usize,
    /// SSA value consumed by the instruction.
    pub value: ValueId,
    /// Representation produced by the value definition.
    pub from: Representation,
    /// Representation required by this instruction use.
    pub to: Representation,
    /// Guard or lossless conversion to emit.
    pub kind: ConversionKind,
    /// Whether failure transfers control to deoptimization.
    pub may_deopt: bool,
}

/// Representation selection and ordered input conversions for one SSA graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReprMap {
    reprs: Box<[Representation]>,
    conversions: Box<[Conversion]>,
}

/// Failure reported by the independent representation verifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReprError {
    /// The representation table does not cover every dense SSA value exactly once.
    RepresentationCountMismatch {
        /// Number of SSA values.
        expected: usize,
        /// Number of stored representations.
        actual: usize,
    },
    /// A non-phi value disagrees with its constant or feedback-driven rule.
    ValueRepresentationMismatch {
        /// Value whose representation is unsound.
        value: ValueId,
        /// Independently recomputed representation.
        expected: Representation,
        /// Stored representation.
        actual: Representation,
    },
    /// A phi is not the least upper bound of its inputs.
    PhiRepresentationMismatch {
        /// Unsound phi value.
        phi: ValueId,
        /// Meet of the stored input representations.
        expected: Representation,
        /// Stored phi representation.
        actual: Representation,
    },
    /// A conversion references no corresponding SSA instruction input.
    UnexpectedConversion {
        /// Canonical instruction PC in the conversion.
        at_pc: u32,
        /// Operand index in the conversion.
        operand_index: usize,
    },
    /// A mismatched instruction input has no conversion.
    MissingConversion {
        /// Canonical instruction PC requiring the conversion.
        at_pc: u32,
        /// Operand index requiring the conversion.
        operand_index: usize,
    },
    /// More than one conversion describes the same instruction input.
    DuplicateConversion {
        /// Canonical instruction PC owning the duplicate.
        at_pc: u32,
        /// Operand index owning the duplicate.
        operand_index: usize,
    },
    /// A conversion is present even though input and target representations match.
    SpuriousConversion {
        /// Canonical instruction PC owning the use.
        at_pc: u32,
        /// Operand index that needs no conversion.
        operand_index: usize,
    },
    /// A conversion's value, representations, kind, or deopt flag is incorrect.
    ConversionMismatch {
        /// Canonical instruction PC owning the use.
        at_pc: u32,
        /// Operand index with the incorrect conversion.
        operand_index: usize,
        /// Independently derived conversion.
        expected: Conversion,
        /// Stored conversion.
        actual: Conversion,
    },
    /// The stored conversion stream is not in canonical deterministic order.
    ConversionOrderMismatch {
        /// Index of the first out-of-order conversion.
        index: usize,
    },
    /// The pinned conversion set cannot express a representation mismatch.
    UnsupportedConversion {
        /// Canonical instruction PC owning the use.
        at_pc: u32,
        /// Operand index requiring the unsupported conversion.
        operand_index: usize,
        /// Representation produced by the input.
        from: Representation,
        /// Representation required by the instruction.
        to: Representation,
    },
    /// Re-running the analysis on identical immutable inputs changed its output.
    NonDeterministic,
}

impl ReprMap {
    /// Select value representations and derive required input conversions.
    #[must_use]
    pub fn compute(tree: &InlineTree, ssa: &SsaFunction) -> Self {
        let mut reprs: Vec<_> = ssa
            .values
            .iter()
            .map(|value| match value.def {
                // Both block-head merges start at the lattice bottom and widen
                // to meet their inputs: an ordinary phi, and a spliced call's
                // result, which merges what the callee returned.
                ValueDef::Phi { .. }
                | ValueDef::InlineResult { .. }
                | ValueDef::Op {
                    op: Op::LoadLocal | Op::StoreLocal,
                    ..
                } => Representation::Int32,
                _ => selected_non_phi_representation(tree, &value.def),
            })
            .collect();

        loop {
            let mut changed = false;
            for value in &ssa.values {
                let widened = match &value.def {
                    ValueDef::Phi { inputs, .. } | ValueDef::InlineResult { inputs, .. } => {
                        inputs.iter().fold(Representation::Int32, |acc, input| {
                            meet(acc, reprs[input.0 as usize])
                        })
                    }
                    ValueDef::Op {
                        op: Op::LoadLocal | Op::StoreLocal,
                        inputs,
                        ..
                    } if inputs.len() == 1 => reprs[inputs[0].0 as usize],
                    _ => continue,
                };
                let slot = &mut reprs[value.id.0 as usize];
                if *slot != widened {
                    *slot = widened;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        let mut conversions = Vec::new();
        for block in &ssa.blocks {
            for instruction in &block.instrs {
                let required = if matches!(instruction.op, Op::LoadLocal | Op::StoreLocal) {
                    reprs[instruction
                        .result
                        .expect("local moves have one SSA result")
                        .0 as usize]
                } else {
                    selected_input_representation(instruction.op, feedback_at(tree, instruction.inline, instruction.pc))
                };
                for (operand_index, &value) in instruction.inputs.iter().enumerate() {
                    let from = reprs[value.0 as usize];
                    if let Some((kind, may_deopt)) = conversion_kind(from, required) {
                        conversions.push(Conversion {
                            inline: instruction.inline,
                            at_pc: instruction.pc,
                            operand_index,
                            value,
                            from,
                            to: required,
                            kind,
                            may_deopt,
                        });
                    }
                }
            }
        }
        // A PC is canonical only within its frame, so a use is named by both.
        conversions.sort_by_key(|conversion| {
            (conversion.inline, conversion.at_pc, conversion.operand_index)
        });

        Self {
            reprs: reprs.into_boxed_slice(),
            conversions: conversions.into_boxed_slice(),
        }
    }

    /// Representation assigned to one dense SSA value.
    #[must_use]
    pub fn representation(&self, value: ValueId) -> Representation {
        self.reprs[value.0 as usize]
    }

    /// Ordered conversions required by instruction inputs.
    #[must_use]
    pub fn conversions(&self) -> &[Conversion] {
        &self.conversions
    }

    /// Independently verify coverage, soundness, conversions, and determinism.
    pub fn verify(&self, tree: &InlineTree, ssa: &SsaFunction) -> Result<(), ReprError> {
        if self.reprs.len() != ssa.values.len() {
            return Err(ReprError::RepresentationCountMismatch {
                expected: ssa.values.len(),
                actual: self.reprs.len(),
            });
        }

        for value in &ssa.values {
            match &value.def {
                ValueDef::Phi { inputs, .. } | ValueDef::InlineResult { inputs, .. } => {
                    let expected = inputs.iter().fold(Representation::Int32, |acc, input| {
                        meet(acc, self.representation(*input))
                    });
                    let actual = self.representation(value.id);
                    if actual != expected {
                        return Err(ReprError::PhiRepresentationMismatch {
                            phi: value.id,
                            expected,
                            actual,
                        });
                    }
                }
                def => {
                    let expected = match def {
                        ValueDef::Op {
                            op: Op::LoadLocal | Op::StoreLocal,
                            inputs,
                            ..
                        } if inputs.len() == 1 => self.representation(inputs[0]),
                        _ => verified_non_phi_representation(tree, def),
                    };
                    let actual = self.representation(value.id);
                    if actual != expected {
                        return Err(ReprError::ValueRepresentationMismatch {
                            value: value.id,
                            expected,
                            actual,
                        });
                    }
                }
            }
        }

        for (index, pair) in self.conversions.windows(2).enumerate() {
            if conversion_key(&pair[0]) > conversion_key(&pair[1]) {
                return Err(ReprError::ConversionOrderMismatch { index: index + 1 });
            }
        }

        // Keyed by frame as well as PC: a PC is canonical only within its own
        // frame, so a caller and a spliced callee both converting at their PC 1
        // are two distinct uses, not a duplicate.
        let mut stored = BTreeMap::<(InlineId, u32, usize), Vec<Conversion>>::new();
        for &conversion in &self.conversions {
            stored
                .entry(conversion_key(&conversion))
                .or_default()
                .push(conversion);
        }
        if let Some((&(_, at_pc, operand_index), _)) =
            stored.iter().find(|(_, conversions)| conversions.len() > 1)
        {
            return Err(ReprError::DuplicateConversion {
                at_pc,
                operand_index,
            });
        }

        for block in &ssa.blocks {
            for instruction in &block.instrs {
                let required = if matches!(instruction.op, Op::LoadLocal | Op::StoreLocal) {
                    self.representation(
                        instruction.result.expect("local moves have one SSA result"),
                    )
                } else {
                    verified_input_representation(instruction.op, feedback_at(tree, instruction.inline, instruction.pc))
                };
                for (operand_index, &value) in instruction.inputs.iter().enumerate() {
                    let key = (instruction.inline, instruction.pc, operand_index);
                    let from = self.representation(value);
                    let matches = stored.remove(&key).unwrap_or_default();
                    if from == required {
                        if !matches.is_empty() {
                            return Err(ReprError::SpuriousConversion {
                                at_pc: instruction.pc,
                                operand_index,
                            });
                        }
                        continue;
                    }
                    let Some((kind, may_deopt)) = verified_conversion_kind(from, required) else {
                        return Err(ReprError::UnsupportedConversion {
                            at_pc: instruction.pc,
                            operand_index,
                            from,
                            to: required,
                        });
                    };
                    let expected = Conversion {
                        inline: instruction.inline,
                        at_pc: instruction.pc,
                        operand_index,
                        value,
                        from,
                        to: required,
                        kind,
                        may_deopt,
                    };
                    match matches.as_slice() {
                        [] => {
                            return Err(ReprError::MissingConversion {
                                at_pc: instruction.pc,
                                operand_index,
                            });
                        }
                        [actual] if *actual != expected => {
                            return Err(ReprError::ConversionMismatch {
                                at_pc: instruction.pc,
                                operand_index,
                                expected,
                                actual: *actual,
                            });
                        }
                        [_] => {}
                        _ => {
                            return Err(ReprError::DuplicateConversion {
                                at_pc: instruction.pc,
                                operand_index,
                            });
                        }
                    }
                }
            }
        }
        if let Some((&(_, at_pc, operand_index), _)) = stored.first_key_value() {
            return Err(ReprError::UnexpectedConversion {
                at_pc,
                operand_index,
            });
        }

        if Self::compute(tree, ssa) != Self::compute(tree, ssa) {
            return Err(ReprError::NonDeterministic);
        }
        Ok(())
    }
}

const fn meet(left: Representation, right: Representation) -> Representation {
    match (left, right) {
        (Representation::Tagged, _) | (_, Representation::Tagged) => Representation::Tagged,
        (Representation::Float64, _) | (_, Representation::Float64) => Representation::Float64,
        (Representation::Int32, Representation::Int32) => Representation::Int32,
    }
}

fn selected_non_phi_representation(tree: &InlineTree, def: &ValueDef) -> Representation {
    match *def {
        ValueDef::Op {
            op: Op::LoadInt32, ..
        } => Representation::Int32,
        ValueDef::Op {
            inline,
            pc,
            op: Op::LoadNumber,
            ..
        } => load_number_at(tree, inline, pc)
            .map_or(Representation::Float64, number_representation),
        ValueDef::Op { inline, pc, op, .. } => {
            selected_result_representation(op, feedback_at(tree, inline, pc))
        }
        _ => Representation::Tagged,
    }
}

fn verified_non_phi_representation(tree: &InlineTree, def: &ValueDef) -> Representation {
    match *def {
        ValueDef::Op {
            op: Op::LoadInt32, ..
        } => Representation::Int32,
        ValueDef::Op {
            inline,
            pc,
            op: Op::LoadNumber,
            ..
        } => match load_number_at(tree, inline, pc) {
            Some(number) if is_exact_i32(number) => Representation::Int32,
            Some(_) => Representation::Float64,
            None => Representation::Float64,
        },
        ValueDef::Op { inline, pc, op, .. } => {
            verified_result_representation(op, feedback_at(tree, inline, pc))
        }
        _ => Representation::Tagged,
    }
}

fn selected_result_representation(op: Op, feedback: ArithFeedback) -> Representation {
    match op {
        Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Increment
        | Op::Neg
        | Op::BitwiseOr
        | Op::BitwiseAnd
        | Op::BitwiseXor
        | Op::Shl
        | Op::Shr
            if feedback.is_int32_only() =>
        {
            Representation::Int32
        }
        Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Increment
        | Op::Neg
        | Op::BitwiseOr
        | Op::BitwiseAnd
        | Op::BitwiseXor
        | Op::Shl
        | Op::Shr
            if feedback.is_numeric_only() =>
        {
            Representation::Float64
        }
        Op::Div | Op::Rem | Op::Pow if feedback.is_numeric_only() => Representation::Float64,
        _ => Representation::Tagged,
    }
}

fn verified_result_representation(op: Op, feedback: ArithFeedback) -> Representation {
    if matches!(
        op,
        Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Increment
            | Op::Neg
            | Op::BitwiseOr
            | Op::BitwiseAnd
            | Op::BitwiseXor
            | Op::Shl
            | Op::Shr
    ) {
        if feedback.is_int32_only() {
            Representation::Int32
        } else if feedback.is_numeric_only() {
            Representation::Float64
        } else {
            Representation::Tagged
        }
    } else if matches!(op, Op::Div | Op::Rem | Op::Pow) && feedback.is_numeric_only() {
        Representation::Float64
    } else {
        Representation::Tagged
    }
}

fn selected_input_representation(op: Op, feedback: ArithFeedback) -> Representation {
    match op {
        Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Increment
        | Op::Neg
        | Op::BitwiseOr
        | Op::BitwiseAnd
        | Op::BitwiseXor
        | Op::Shl
        | Op::Shr
            if feedback.is_int32_only() =>
        {
            Representation::Int32
        }
        Op::LessThan | Op::LessEq | Op::GreaterThan | Op::GreaterEq | Op::Equal | Op::NotEqual
            if feedback.is_int32_only() =>
        {
            Representation::Int32
        }
        Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Increment
        | Op::Neg
        | Op::BitwiseOr
        | Op::BitwiseAnd
        | Op::BitwiseXor
        | Op::Shl
        | Op::Shr
        | Op::Div
        | Op::Rem
        | Op::Pow
        | Op::LessThan
        | Op::LessEq
        | Op::GreaterThan
        | Op::GreaterEq
        | Op::Equal
        | Op::NotEqual
            if feedback.is_numeric_only() =>
        {
            Representation::Float64
        }
        _ => Representation::Tagged,
    }
}

fn verified_input_representation(op: Op, feedback: ArithFeedback) -> Representation {
    let int32_closed = matches!(
        op,
        Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Increment
            | Op::Neg
            | Op::BitwiseOr
            | Op::BitwiseAnd
            | Op::BitwiseXor
            | Op::Shl
            | Op::Shr
    );
    let comparison = matches!(
        op,
        Op::LessThan | Op::LessEq | Op::GreaterThan | Op::GreaterEq | Op::Equal | Op::NotEqual
    );
    let numeric = int32_closed || comparison || matches!(op, Op::Div | Op::Rem | Op::Pow);
    if feedback.is_int32_only() && (int32_closed || comparison) {
        Representation::Int32
    } else if feedback.is_numeric_only() && numeric {
        Representation::Float64
    } else {
        Representation::Tagged
    }
}

fn number_representation(number: f64) -> Representation {
    if is_exact_i32(number) {
        Representation::Int32
    } else {
        Representation::Float64
    }
}

fn is_exact_i32(number: f64) -> bool {
    number.is_finite()
        && !(number == 0.0 && number.is_sign_negative())
        && number >= f64::from(i32::MIN)
        && number <= f64::from(i32::MAX)
        && number == f64::from(number as i32)
}

fn conversion_kind(from: Representation, to: Representation) -> Option<(ConversionKind, bool)> {
    match (from, to) {
        (Representation::Tagged, Representation::Int32) => {
            Some((ConversionKind::CheckedTaggedToInt32, true))
        }
        (Representation::Tagged, Representation::Float64) => {
            Some((ConversionKind::CheckedTaggedToFloat64, true))
        }
        (Representation::Int32, Representation::Float64) => {
            Some((ConversionKind::Int32ToFloat64, false))
        }
        (Representation::Int32, Representation::Tagged) => Some((ConversionKind::BoxInt32, false)),
        (Representation::Float64, Representation::Tagged) => {
            Some((ConversionKind::BoxFloat64, false))
        }
        _ => None,
    }
}

/// Lossless widening required when a phi input enters the phi's least-upper-
/// bound representation. Checked narrowing is never valid on a CFG edge.
pub(crate) fn lossless_phi_conversion(
    from: Representation,
    to: Representation,
) -> Option<ConversionKind> {
    match (from, to) {
        (Representation::Int32, Representation::Float64) => Some(ConversionKind::Int32ToFloat64),
        (Representation::Int32, Representation::Tagged) => Some(ConversionKind::BoxInt32),
        (Representation::Float64, Representation::Tagged) => Some(ConversionKind::BoxFloat64),
        _ => None,
    }
}

fn verified_conversion_kind(
    from: Representation,
    to: Representation,
) -> Option<(ConversionKind, bool)> {
    if from == Representation::Tagged && to == Representation::Int32 {
        Some((ConversionKind::CheckedTaggedToInt32, true))
    } else if from == Representation::Tagged && to == Representation::Float64 {
        Some((ConversionKind::CheckedTaggedToFloat64, true))
    } else if from == Representation::Int32 && to == Representation::Float64 {
        Some((ConversionKind::Int32ToFloat64, false))
    } else if from == Representation::Int32 && to == Representation::Tagged {
        Some((ConversionKind::BoxInt32, false))
    } else if from == Representation::Float64 && to == Representation::Tagged {
        Some((ConversionKind::BoxFloat64, false))
    } else {
        None
    }
}

fn conversion_key(conversion: &Conversion) -> (InlineId, u32, usize) {
    (conversion.inline, conversion.at_pc, conversion.operand_index)
}

#[cfg(test)]
mod tests {
    use otter_bytecode::Operand;
    use otter_vm::{
        jit::JitTestInstruction,
        jit_feedback::{ARITH_FLOAT64, ARITH_INT32, ARITH_STRING},
    };

    use otter_vm::JitCompileSnapshot;

    use super::*;
    use crate::ir::{cfg::ControlFlowGraph, dom::DominatorTree};

    fn build(
        param_count: u16,
        register_count: u16,
        instructions: Vec<(Op, Vec<Operand>)>,
        feedback: &[(u32, u8)],
    ) -> (InlineTree, SsaFunction) {
        let instructions = instructions
            .into_iter()
            .enumerate()
            .map(|(pc, (op, operands))| {
                JitTestInstruction::new(op, pc as u32, pc as u32 * 4, operands)
            })
            .collect();
        let mut view =
            JitCompileSnapshot::without_feedback(0, param_count, register_count, instructions);
        for &(pc, bits) in feedback {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(bits));
        }
        let tree = InlineTree::trivial(&view);
        let cfg = ControlFlowGraph::build_inlined(&tree).expect("CFG builds");
        let ssa = SsaFunction::build_inlined(&tree, &cfg).expect("SSA builds");
        ssa.verify(&cfg, &DominatorTree::compute(&cfg))
            .expect("SSA verifies");
        (tree, ssa)
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

    fn phi_for_register(ssa: &SsaFunction, register: u16) -> ValueId {
        ssa.values
            .iter()
            .find_map(|value| match value.def {
                ValueDef::Phi {
                    register: owner, ..
                } if owner == register => Some(value.id),
                _ => None,
            })
            .expect("register has a phi")
    }

    #[test]
    fn a_spliced_call_result_widens_to_meet_what_the_callee_returns() {
        use crate::ir::inline::InlineTree;
        use otter_vm::jit::JitTestInstruction;

        // Caller: `r0 = r1(r2); return r0`. Callee returns its int32 parameter.
        let mut view = JitCompileSnapshot::without_feedback(
            7,
            1,
            8,
            vec![
                JitTestInstruction::new(
                    Op::Call,
                    0,
                    0,
                    vec![
                        Operand::Register(0),
                        Operand::Register(1),
                        Operand::ConstIndex(1),
                        Operand::Register(2),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 1, 8, vec![Operand::Register(0)]),
            ],
        );
        let callee = JitCompileSnapshot::without_feedback(
            9,
            1,
            4,
            vec![
                JitTestInstruction::new(Op::LoadInt32, 0, 0, vec![Operand::Register(1), Operand::Imm32(7)]),
                JitTestInstruction::new(Op::ReturnValue, 1, 8, vec![Operand::Register(1)]),
            ],
        );
        let call_byte_pc = view.instructions[0].byte_pc;
        view.inline_callees.insert(
            call_byte_pc,
            otter_vm::JitInlineCallee {
                code_block: std::sync::Arc::clone(&callee.code_block),
                function_id: 9,
                param_count: 1,
                register_count: callee.code_block.register_count,
                instructions: callee.instructions,
            },
        );
        let tree = InlineTree::build(&view);
        assert_eq!(tree.frames.len(), 2, "the fixture must splice");
        let cfg = ControlFlowGraph::build_inlined(&tree).expect("a spliced CFG builds");
        let ssa = SsaFunction::build_inlined(&tree, &cfg).expect("a spliced SSA builds");
        let reprs = ReprMap::compute(&tree, &ssa);
        reprs.verify(&tree, &ssa).expect("representations verify");

        // The merge is a lattice merge like a phi, not a Tagged default that
        // missed the fixpoint: it meets the single int32 the callee returns.
        let merge = ssa
            .values
            .iter()
            .find(|value| matches!(value.def, ValueDef::InlineResult { .. }))
            .expect("the continuation merges the call result");
        assert_eq!(reprs.representation(merge.id), Representation::Int32);
    }

    #[test]
    fn int32_add_stays_unboxed_without_conversions() {
        let (view, ssa) = build(
            0,
            3,
            vec![
                (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(4)]),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(5)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnUndefined, vec![]),
            ],
            &[(2, ARITH_INT32)],
        );

        let map = ReprMap::compute(&view, &ssa);
        assert_eq!(
            map.representation(op_value_at(&ssa, 0)),
            Representation::Int32
        );
        assert_eq!(
            map.representation(op_value_at(&ssa, 1)),
            Representation::Int32
        );
        assert_eq!(
            map.representation(op_value_at(&ssa, 2)),
            Representation::Int32
        );
        assert!(map.conversions().is_empty());
        assert_eq!(map.verify(&view, &ssa), Ok(()));
    }

    #[test]
    fn float64_feedback_selects_float64_add() {
        let (view, ssa) = build(
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
                (Op::ReturnUndefined, vec![]),
            ],
            &[(0, ARITH_FLOAT64)],
        );

        let map = ReprMap::compute(&view, &ssa);
        assert_eq!(
            map.representation(op_value_at(&ssa, 0)),
            Representation::Float64
        );
        assert_eq!(map.verify(&view, &ssa), Ok(()));
    }

    #[test]
    fn mixed_or_string_feedback_keeps_add_tagged() {
        for bits in [ARITH_STRING, ARITH_INT32 | ARITH_STRING] {
            let (view, ssa) = build(
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
                    (Op::ReturnUndefined, vec![]),
                ],
                &[(0, bits)],
            );
            let map = ReprMap::compute(&view, &ssa);
            assert_eq!(
                map.representation(op_value_at(&ssa, 0)),
                Representation::Tagged
            );
            assert_eq!(map.verify(&view, &ssa), Ok(()));
        }
    }

    #[test]
    fn phi_fixpoint_widens_int32_with_float64_and_tagged() {
        let (float_view, float_ssa) = build(
            1,
            3,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(8)]),
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(2)]),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(0)],
                ),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(7)]),
                (Op::Jump, vec![Operand::Imm32(1)]),
                (
                    Op::Div,
                    vec![
                        Operand::Register(1),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
            &[(5, ARITH_INT32)],
        );
        let float_map = ReprMap::compute(&float_view, &float_ssa);
        let float_phi = phi_for_register(&float_ssa, 1);
        assert_eq!(float_map.representation(float_phi), Representation::Float64);
        assert_eq!(float_map.verify(&float_view, &float_ssa), Ok(()));

        let (tagged_view, tagged_ssa) = build(
            1,
            2,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(8)]),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(0)],
                ),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(7)]),
                (Op::Jump, vec![Operand::Imm32(1)]),
                (Op::LoadUndefined, vec![Operand::Register(1)]),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
            &[],
        );
        let tagged_map = ReprMap::compute(&tagged_view, &tagged_ssa);
        let tagged_phi = phi_for_register(&tagged_ssa, 1);
        assert_eq!(
            tagged_map.representation(tagged_phi),
            Representation::Tagged
        );
        assert_eq!(tagged_map.verify(&tagged_view, &tagged_ssa), Ok(()));
    }

    #[test]
    fn records_checked_unboxing_and_lossless_widening() {
        let (checked_view, checked_ssa) = build(
            1,
            3,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(1)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnUndefined, vec![]),
            ],
            &[(1, ARITH_INT32)],
        );
        let checked_map = ReprMap::compute(&checked_view, &checked_ssa);
        assert!(checked_map.conversions().iter().any(|conversion| {
            conversion.at_pc == 1
                && conversion.operand_index == 0
                && conversion.kind == ConversionKind::CheckedTaggedToInt32
                && conversion.may_deopt
        }));
        assert_eq!(checked_map.verify(&checked_view, &checked_ssa), Ok(()));

        let (widen_view, widen_ssa) = build(
            0,
            3,
            vec![
                (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(1)]),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(2)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnUndefined, vec![]),
            ],
            &[(2, ARITH_FLOAT64)],
        );
        let widen_map = ReprMap::compute(&widen_view, &widen_ssa);
        assert!(widen_map.conversions().iter().any(|conversion| {
            conversion.at_pc == 2
                && conversion.operand_index == 0
                && conversion.kind == ConversionKind::Int32ToFloat64
                && !conversion.may_deopt
        }));
        assert_eq!(widen_map.verify(&widen_view, &widen_ssa), Ok(()));
    }

    #[test]
    fn verifier_rejects_phi_not_equal_to_input_meet() {
        let (view, ssa) = build(
            1,
            3,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(8)]),
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(2)]),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(0)],
                ),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(7)]),
                (Op::Jump, vec![Operand::Imm32(1)]),
                (
                    Op::Div,
                    vec![
                        Operand::Register(1),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
            &[(5, ARITH_INT32)],
        );
        let phi = phi_for_register(&ssa, 1);
        let mut map = ReprMap::compute(&view, &ssa);
        map.reprs[phi.0 as usize] = Representation::Int32;

        assert_eq!(
            map.verify(&view, &ssa),
            Err(ReprError::PhiRepresentationMismatch {
                phi,
                expected: Representation::Float64,
                actual: Representation::Int32,
            })
        );
    }

    #[test]
    fn repeated_analysis_is_identical() {
        let (view, ssa) = build(
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
                (Op::ReturnUndefined, vec![]),
            ],
            &[(0, ARITH_FLOAT64)],
        );
        assert_eq!(ReprMap::compute(&view, &ssa), ReprMap::compute(&view, &ssa));
    }
}
