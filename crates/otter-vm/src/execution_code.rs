//! Shared wordcode with isolate-local instruction identity and IC numbering.
//!
//! # Contents
//! - [`ExecutableCode`] shares the admitted canonical wordcode owner.
//! - [`CodeBlockInstruction`] is a borrowed fetch view, never a stored copy.
//!
//! # Invariants
//! - The physical wordcode owner also owns its lease; snapshots share both.
//! - Function identity belongs to this view; the logical PC comes from its index.
//! - Only IC numbering is isolate-local; opcode and operand words are never copied.
//! - Fetching/iterating allocates nothing and exposes no managed GC handles.
//!
//! # See also
//! - `super::CodeBlock` owns feedback, CFG and the metadata lease.
//! - `otter_bytecode::FunctionCode` owns canonical words and their physical lease.

use super::{NO_PROPERTY_IC_SITE, allocation, has_property_ic_site};
use otter_bytecode::{FunctionCode, wordcode::Instruction};
use otter_resource::{ResourceError, ResourceLease};

/// Isolate-local metadata over one shared immutable instruction body.
#[derive(Debug)]
pub struct ExecutableCode {
    pub(super) body: FunctionCode,
    function_id: u32,
    property_sites: Box<[u32]>,
}

impl ExecutableCode {
    pub(super) fn new(
        body: FunctionCode,
        function_id: u32,
        next_site: &mut u32,
        lease: &mut ResourceLease,
    ) -> Result<Self, ResourceError> {
        let mut sites = allocation::try_vec(body.len(), lease)?;
        for instruction in body.iter() {
            let site = if has_property_ic_site(instruction.op) {
                let site = *next_site;
                *next_site = next_site
                    .checked_add(1)
                    .expect("property IC sites fit admitted module");
                site
            } else {
                NO_PROPERTY_IC_SITE
            };
            sites.push(site);
        }
        Ok(Self {
            body,
            function_id,
            property_sites: sites.into_boxed_slice(),
        })
    }

    pub(super) fn fresh_copy(&self, lease: &mut ResourceLease) -> Result<Self, ResourceError> {
        Ok(Self {
            body: self.body.clone(),
            function_id: self.function_id,
            property_sites: allocation::try_copy(&self.property_sites, lease)?,
        })
    }

    pub(super) fn retained_bytes(&self) -> u64 {
        std::mem::size_of_val(&*self.property_sites) as u64
    }

    /// Canonical wordcode, shared with the compiler/module and native snapshots.
    #[must_use]
    pub fn wordcode(&self) -> &FunctionCode {
        &self.body
    }

    /// Number of canonical instructions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.body.len()
    }
    /// Whether the canonical body has no instructions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.body.is_empty()
    }
    /// Fetch an instruction together with its canonical index and owner.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<CodeBlockInstruction<'_>> {
        self.body.get(index).map(|source| CodeBlockInstruction {
            source,
            code: self,
            instruction_pc: index as u32,
        })
    }
    /// Traverse canonical instructions in logical-PC order without allocation.
    pub fn iter(&self) -> ExecutionIter<'_> {
        ExecutionIter {
            code: self,
            inner: self.body.iter().enumerate(),
        }
    }
}

/// Allocation-free instruction fetch view carrying its exact source owner.
#[derive(Debug, Clone, Copy)]
pub struct CodeBlockInstruction<'a> {
    /// Canonical index of this instruction in its immutable body.
    pub instruction_pc: u32,
    pub(super) source: &'a Instruction,
    pub(super) code: &'a ExecutableCode,
}

impl std::ops::Deref for CodeBlockInstruction<'_> {
    type Target = Instruction;
    fn deref(&self) -> &Instruction {
        self.source
    }
}

impl<'a> CodeBlockInstruction<'a> {
    pub(crate) fn code_block_id(self) -> u32 {
        self.code.function_id
    }
    #[must_use]
    /// Isolate-local named-property IC directory index, when this opcode owns one.
    pub fn property_ic_site(self) -> Option<usize> {
        let site = self.code.property_sites[self.instruction_pc as usize];
        (site != NO_PROPERTY_IC_SITE).then_some(site as usize)
    }
    #[inline]
    pub(crate) fn reg(self, index: usize) -> u16 {
        self.source.inline_word(index) as u16
    }
    #[inline]
    pub(crate) fn reg3(self) -> (u16, u16, u16) {
        (self.reg(0), self.reg(1), self.reg(2))
    }
    #[inline]
    pub(crate) fn const_word(self, index: usize) -> u32 {
        self.source.inline_word(index)
    }
    #[inline]
    pub(crate) fn imm(self, index: usize) -> i32 {
        self.source.inline_word(index) as i32
    }
    pub(super) fn words(self) -> &'a [u32] {
        self.code.body.operand_words(self.source)
    }
}

/// Borrowed logical-PC traversal; each fetch view is built in registers.
#[derive(Clone)]
pub struct ExecutionIter<'a> {
    code: &'a ExecutableCode,
    inner: std::iter::Enumerate<std::slice::Iter<'a, Instruction>>,
}
impl<'a> ExecutionIter<'a> {
    fn view(&self, (index, source): (usize, &'a Instruction)) -> CodeBlockInstruction<'a> {
        CodeBlockInstruction {
            source,
            code: self.code,
            instruction_pc: index as u32,
        }
    }
}
impl<'a> Iterator for ExecutionIter<'a> {
    type Item = CodeBlockInstruction<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        let next = self.inner.next()?;
        Some(self.view(next))
    }
    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        let next = self.inner.nth(n)?;
        Some(self.view(next))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}
impl DoubleEndedIterator for ExecutionIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let next = self.inner.next_back()?;
        Some(self.view(next))
    }
}
impl ExactSizeIterator for ExecutionIter<'_> {}
impl<'a> IntoIterator for &'a ExecutableCode {
    type Item = CodeBlockInstruction<'a>;
    type IntoIter = ExecutionIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
