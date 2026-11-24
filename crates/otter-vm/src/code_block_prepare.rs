//! Admission and preparation of an executable function's instruction tables.
//!
//! # Contents
//! - `CodeBlockLayout` counts table storage in one read-only walk.
//! - `PreparedTables` fills byte PCs, named sites, control flow and feedback
//!   from the same instruction stream.
//!
//! # Invariants
//! - Only admitted wordcode reaches this path. Allocation is charged before
//!   any of its tables are populated, and a failure publishes no CodeBlock.
//! - Byte PCs and feedback cells use canonical logical instruction indices.
//!   The named site interval is assigned during module linking.
//!
//! # See also
//! - `super::code_block_cfg` owns logical flow and exception tables.
//! - `crate::feedback` owns isolate-local feedback storage.

use otter_bytecode::{ExceptionHandler, FunctionCode, Op, encoding::wordcode_instruction_bytes};
use otter_resource::{ResourceError, ResourceLease};

use super::allocation::{array_bytes, try_vec};
use super::code_block_cfg::{CodeBlockControlFlow, ControlFlowBuilder, ControlFlowLayout};
use super::{ExecutableCode, NO_PROPERTY_IC_SITE, has_property_ic_site};
use crate::feedback::{FeedbackLayout, FeedbackVector};

pub(super) struct CodeBlockLayout {
    flow: ControlFlowLayout,
    feedback: FeedbackLayout,
    makes_function: bool,
}

pub(super) struct PreparedTables {
    pub(super) byte_pcs: Box<[u32]>,
    pub(super) byte_len: u32,
    pub(super) code: ExecutableCode,
    pub(super) control_flow: CodeBlockControlFlow,
    pub(super) feedback: FeedbackVector,
    pub(super) makes_function: bool,
}

impl CodeBlockLayout {
    pub(super) fn measure(code: &FunctionCode, handlers: &[ExceptionHandler]) -> Self {
        let mut flow = ControlFlowLayout::new(code.len(), handlers.len());
        let mut feedback = FeedbackLayout::default();
        let mut makes_function = false;
        for (index, instruction) in code.iter().enumerate() {
            let op = instruction.op;
            flow.count(code, index, op);
            feedback.count(op);
            makes_function |= matches!(op, Op::MakeFunction | Op::MakeClosure);
        }
        Self {
            flow,
            feedback,
            makes_function,
        }
    }

    pub(super) fn bytes(&self, count: usize) -> u64 {
        self.flow
            .bytes()
            .saturating_add(self.feedback.bytes())
            .saturating_add(array_bytes::<[u32; 2]>(count))
    }

    pub(super) fn prepare(
        self,
        code: &FunctionCode,
        id: u32,
        mut named_site: u32,
        handlers: &[ExceptionHandler],
        lease: &mut ResourceLease,
    ) -> Result<PreparedTables, ResourceError> {
        let mut byte_pcs = try_vec(code.len(), lease)?;
        let mut named_sites = try_vec(code.len(), lease)?;
        let mut flow = ControlFlowBuilder::new(self.flow, lease)?;
        let mut byte_len = 0;
        let ops = code.iter().enumerate().map(|(index, instruction)| {
            let op = instruction.op;
            byte_pcs.push(byte_len);
            byte_len += u32::from(wordcode_instruction_bytes(instruction));
            flow.record(code, index, op);
            named_sites.push(if has_property_ic_site(op) {
                let current = named_site;
                named_site = named_site
                    .checked_add(1)
                    .expect("linked named site interval");
                current
            } else {
                NO_PROPERTY_IC_SITE
            });
            op
        });
        let feedback = FeedbackVector::for_instruction_ops(ops, self.feedback, lease)?;
        let control_flow = flow.finish(handlers, lease)?;
        Ok(PreparedTables {
            code: ExecutableCode::with_sites(code.clone(), id, named_sites.into_boxed_slice()),
            byte_pcs: byte_pcs.into_boxed_slice(),
            byte_len,
            control_flow,
            feedback,
            makes_function: self.makes_function,
        })
    }
}
