//! Authoritative compiler/wire wordcode storage and mutable construction.
//!
//! Compiler and linker code build one [`FunctionCode`] through
//! [`FunctionCodeBuilder`]. The VM verifies and translates that frozen product
//! once into its execution-specific layout; decoded operand DTOs are reserved
//! for cold serialization tooling.
//!
//! # Contents
//! - [`Instruction`] — compact opcode plus inline words or one overflow range.
//! - [`FunctionCode`] — frozen instruction array and shared overflow table.
//! - [`FunctionCodeBuilder`] — emission, patching, and linker transformation.
//! - [`OperandView`] — borrowed schema-typed operand decoding.
//!
//! # Invariants
//! - Operand kinds come only from the opcode schema.
//! - Up to four words are instruction-local; longer forms use one function-wide
//!   dense overflow table and never allocate per instruction.
//! - Instruction position is the only logical PC; records carry no PC field.
//! - A frozen [`FunctionCode`] is the canonical compiler, wire, and debug
//!   representation, not a required interpreter memory layout.
//!
//! # See also
//! - [`crate::opcode_schema`]
//! - [`crate::encoding`]

use serde::{Deserialize, Serialize};

use crate::{Op, Operand, opcode_schema};

/// Operand words stored directly in the common instruction record.
pub const INLINE_OPERAND_WORDS: usize = 4;
const NO_OVERFLOW_OPERANDS: u32 = u32::MAX;

/// One compact logical instruction in authoritative compiler wordcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instruction {
    /// Opcode identity; operand formats come from its schema row.
    pub op: Op,
    operand_count: u8,
    inline_operand_words: [u32; INLINE_OPERAND_WORDS],
    overflow_operand_offset: u32,
}

const _: [(); 24] = [(); std::mem::size_of::<Instruction>()];
const _: [(); 4] = [(); std::mem::align_of::<Instruction>()];

impl Instruction {
    /// Number of schema-typed operand words.
    #[must_use]
    pub const fn operand_count(self) -> usize {
        self.operand_count as usize
    }

    /// Whether all words are held directly in this record.
    #[must_use]
    pub const fn operands_are_inline(self) -> bool {
        self.overflow_operand_offset == NO_OVERFLOW_OPERANDS
    }

    fn operand_word(self, overflow: &[u32], index: usize) -> Option<u32> {
        if index >= self.operand_count() {
            return None;
        }
        if self.operands_are_inline() {
            return self.inline_operand_words.get(index).copied();
        }
        overflow
            .get(self.overflow_operand_offset as usize + index)
            .copied()
    }
}

/// Frozen authoritative compiler/wire wordcode for one function.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionCode {
    instructions: Box<[Instruction]>,
    overflow_operand_words: Box<[u32]>,
}

impl FunctionCode {
    /// Number of logical instructions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.instructions.len()
    }

    /// Whether the body contains no instructions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.instructions.is_empty()
    }

    /// Iterate in canonical logical-PC order.
    pub fn iter(&self) -> std::slice::Iter<'_, Instruction> {
        self.instructions.iter()
    }

    /// Instruction at one logical PC.
    #[must_use]
    pub fn get(&self, pc: usize) -> Option<&Instruction> {
        self.instructions.get(pc)
    }

    /// Borrow schema-typed operands for an instruction owned by this body.
    #[must_use]
    pub const fn operands<'a>(&'a self, instruction: &'a Instruction) -> OperandView<'a> {
        OperandView {
            code: self,
            instruction,
        }
    }

    /// Decode one operand.
    #[must_use]
    pub fn operand(&self, instruction: &Instruction, index: usize) -> Option<Operand> {
        let word = instruction.operand_word(&self.overflow_operand_words, index)?;
        let kind = opcode_schema::operand_kind_at(instruction.op, index)?;
        opcode_schema::decode_operand_word(kind, word)
    }

    /// Re-open this body for linker/compiler transformations.
    #[must_use]
    pub fn to_builder(&self) -> FunctionCodeBuilder {
        FunctionCodeBuilder {
            instructions: self.instructions.to_vec(),
            overflow_operand_words: self.overflow_operand_words.to_vec(),
        }
    }
}

impl std::ops::Index<usize> for FunctionCode {
    type Output = Instruction;

    fn index(&self, index: usize) -> &Self::Output {
        &self.instructions[index]
    }
}

impl From<Vec<crate::Instruction>> for FunctionCode {
    fn from(decoded: Vec<crate::Instruction>) -> Self {
        let mut builder = FunctionCodeBuilder::new();
        for instruction in decoded {
            builder.push(instruction.op, &instruction.operands);
        }
        builder.finish()
    }
}

impl<'a> IntoIterator for &'a FunctionCode {
    type Item = &'a Instruction;
    type IntoIter = std::slice::Iter<'a, Instruction>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Mutable single-owner construction surface for [`FunctionCode`].
#[derive(Debug, Default)]
pub struct FunctionCodeBuilder {
    instructions: Vec<Instruction>,
    overflow_operand_words: Vec<u32>,
}

impl FunctionCodeBuilder {
    /// Construct an empty builder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            instructions: Vec::new(),
            overflow_operand_words: Vec::new(),
        }
    }

    /// Logical PC assigned to the next instruction.
    #[must_use]
    pub fn next_pc(&self) -> u32 {
        u32::try_from(self.instructions.len()).expect("function exceeds u32 logical PCs")
    }

    /// Number of emitted instructions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.instructions.len()
    }

    /// Whether no instructions have been emitted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.instructions.is_empty()
    }

    /// Emit one verified-shape candidate and return its logical PC.
    pub fn push(&mut self, op: Op, operands: &[Operand]) -> u32 {
        if let Err(error) = opcode_schema::verify_operand_shape(op, operands) {
            panic!("invalid {op:?} wordcode operands: {error}");
        }
        let pc = self.next_pc();
        let operand_count =
            u8::try_from(operands.len()).expect("instruction operand count exceeds u8");
        let mut inline_operand_words = [0; INLINE_OPERAND_WORDS];
        let overflow_operand_offset = if operands.len() <= INLINE_OPERAND_WORDS {
            for (slot, operand) in inline_operand_words.iter_mut().zip(operands) {
                *slot = opcode_schema::encode_operand_word(*operand);
            }
            NO_OVERFLOW_OPERANDS
        } else {
            let offset = u32::try_from(self.overflow_operand_words.len())
                .expect("function operand overflow table exceeds u32");
            self.overflow_operand_words.extend(
                operands
                    .iter()
                    .copied()
                    .map(opcode_schema::encode_operand_word),
            );
            offset
        };
        self.instructions.push(Instruction {
            op,
            operand_count,
            inline_operand_words,
            overflow_operand_offset,
        });
        pc
    }

    /// Opcode at one logical PC.
    #[must_use]
    pub fn op(&self, pc: u32) -> Option<Op> {
        self.instructions
            .get(pc as usize)
            .map(|instruction| instruction.op)
    }

    /// Operand count at one logical PC.
    #[must_use]
    pub fn operand_count(&self, pc: u32) -> Option<usize> {
        self.instructions
            .get(pc as usize)
            .map(|instruction| instruction.operand_count())
    }

    /// Decode one operand from the mutable body.
    #[must_use]
    pub fn operand(&self, pc: u32, index: usize) -> Option<Operand> {
        let instruction = self.instructions.get(pc as usize)?;
        let word = instruction.operand_word(&self.overflow_operand_words, index)?;
        let kind = opcode_schema::operand_kind_at(instruction.op, index)?;
        opcode_schema::decode_operand_word(kind, word)
    }

    /// Replace one operand while preserving its schema-declared kind.
    pub fn set_operand(&mut self, pc: u32, index: usize, operand: Operand) -> bool {
        let Some(instruction) = self.instructions.get_mut(pc as usize) else {
            return false;
        };
        if index >= instruction.operand_count() {
            return false;
        }
        let Some(expected) = opcode_schema::operand_kind_at(instruction.op, index) else {
            return false;
        };
        let actual = match operand {
            Operand::Register(_) => opcode_schema::OperandKind::Register,
            Operand::ConstIndex(_) => opcode_schema::OperandKind::ConstIndex,
            Operand::Imm32(_) => opcode_schema::OperandKind::Imm32,
        };
        if actual != expected {
            return false;
        }
        let word = opcode_schema::encode_operand_word(operand);
        if instruction.operands_are_inline() {
            instruction.inline_operand_words[index] = word;
        } else {
            self.overflow_operand_words[instruction.overflow_operand_offset as usize + index] =
                word;
        }
        true
    }

    /// Replace an instruction in place, rebuilding the body when its storage
    /// class changes. This is a cold compiler/linker patching operation.
    pub fn replace(&mut self, pc: u32, op: Op, operands: &[Operand]) -> bool {
        let index = pc as usize;
        if index >= self.instructions.len() {
            return false;
        }
        let mut rebuilt = FunctionCodeBuilder::new();
        for current in 0..self.instructions.len() {
            if current == index {
                rebuilt.push(op, operands);
            } else {
                let instruction = self.instructions[current];
                let decoded = (0..instruction.operand_count())
                    .map(|operand_index| {
                        let word = instruction
                            .operand_word(&self.overflow_operand_words, operand_index)
                            .expect("builder instruction operand word");
                        let kind = opcode_schema::operand_kind_at(instruction.op, operand_index)
                            .expect("builder instruction schema kind");
                        opcode_schema::decode_operand_word(kind, word)
                            .expect("builder instruction operand decode")
                    })
                    .collect::<Vec<_>>();
                rebuilt.push(instruction.op, &decoded);
            }
        }
        *self = rebuilt;
        true
    }

    /// Iterate over compact instruction records in logical-PC order.
    pub fn iter(&self) -> std::slice::Iter<'_, Instruction> {
        self.instructions.iter()
    }

    /// Freeze the sole execution product.
    #[must_use]
    pub fn finish(self) -> FunctionCode {
        FunctionCode {
            instructions: self.instructions.into_boxed_slice(),
            overflow_operand_words: self.overflow_operand_words.into_boxed_slice(),
        }
    }
}

/// Borrowed schema-decoded operands over authoritative wordcode.
#[derive(Clone, Copy)]
pub struct OperandView<'a> {
    code: &'a FunctionCode,
    instruction: &'a Instruction,
}

impl<'a> OperandView<'a> {
    /// Operand count.
    #[must_use]
    pub const fn len(self) -> usize {
        self.instruction.operand_count()
    }

    /// Whether there are no operands.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Decode one operand.
    #[must_use]
    pub fn get(self, index: usize) -> Option<Operand> {
        self.code.operand(self.instruction, index)
    }

    /// Iterate without materialising a collection.
    pub fn iter(self) -> impl ExactSizeIterator<Item = Operand> + 'a {
        (0..self.len()).map(move |index| {
            self.get(index)
                .expect("frozen wordcode operand must match opcode schema")
        })
    }
}

impl std::fmt::Debug for OperandView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_and_overflow_words_share_one_body() {
        let mut builder = FunctionCodeBuilder::new();
        builder.push(
            Op::Add,
            &[
                Operand::Register(0),
                Operand::Register(1),
                Operand::Register(2),
            ],
        );
        builder.push(
            Op::MakeClass,
            &[
                Operand::Register(3),
                Operand::Register(4),
                Operand::Register(5),
                Operand::Register(6),
                Operand::Register(7),
            ],
        );
        let code = builder.finish();
        assert!(code[0].operands_are_inline());
        assert!(!code[1].operands_are_inline());
        assert_eq!(
            code.operands(&code[1]).iter().collect::<Vec<_>>(),
            vec![
                Operand::Register(3),
                Operand::Register(4),
                Operand::Register(5),
                Operand::Register(6),
                Operand::Register(7),
            ]
        );
    }

    #[test]
    fn patch_preserves_authoritative_kind() {
        let mut builder = FunctionCodeBuilder::new();
        builder.push(Op::Jump, &[Operand::Imm32(0)]);
        assert!(builder.set_operand(0, 0, Operand::Imm32(-7)));
        assert!(!builder.set_operand(0, 0, Operand::Register(1)));
        assert_eq!(builder.operand(0, 0), Some(Operand::Imm32(-7)));
    }
}
