//! Source-owned named-property programs for Machine lowering.
//!
//! # Contents
//! - Immutable source identity and complete CacheIR programs per operation.
//! - Snapshot capture before selection loses the callee's compilation context.
//!
//! # Invariants
//! - Equal byte PCs in different functions never select each other's facts.
//! - Each operation owns its prepared program; the emitter uses the root view
//!   only for isolate-wide physical layouts, never to look up property facts.
//! - Programs contain stable shape tokens and offsets, not moving references.
//! - Megamorphic loads carry only their source atom; the existing isolate table
//!   remains the sole mutable lookup cache and is probed afresh at execution.
//!
//! # See also
//! - `numeric::hir` retains these programs across graph transformations.

use otter_bytecode::Op;
use otter_vm::{JitCacheIrProgram, JitCompileSnapshot};

/// Immutable semantic source and generated hit program for a named access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineCacheIrSite {
    /// Bytecode owner of the property name and feedback slot.
    pub function_id: u32,
    /// Instruction index in that function.
    pub logical_pc: u32,
    /// Encoded source offset, used for artifact attribution.
    pub byte_pc: u32,
    /// Prepared receiver-shape/slot alternatives from this compilation site.
    pub program: Box<[JitCacheIrProgram]>,
    /// Atom for a load that consults the isolate's existing megamorphic table.
    pub megamorphic_atom: Option<u32>,
}

impl MachineCacheIrSite {
    pub(crate) fn capture(view: &JitCompileSnapshot, byte_pc: u32, op: Op) -> Option<Self> {
        let logical_pc = view.instructions.iter().position(|instruction| {
            instruction.byte_pc == byte_pc && instruction.op(&view.code_block) == op
        })?;
        if !matches!(op, Op::LoadProperty | Op::StoreProperty) {
            return None;
        }
        Some(Self {
            function_id: view.code_block.id,
            logical_pc: u32::try_from(logical_pc).ok()?,
            byte_pc,
            megamorphic_atom: (op == Op::LoadProperty && view.property_lookup_cache.is_some())
                .then(|| view.property_megamorphic_loads.get(&byte_pc).copied())
                .flatten(),
            program: view
                .property_programs
                .get(&byte_pc)
                .cloned()
                .unwrap_or_default()
                .into(),
        })
    }
}
