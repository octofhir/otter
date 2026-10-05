//! Exact generated JavaScript return-site capture during native compilation.
//!
//! # Contents
//! - `ReturnSiteRecorder` appends actual CALL/BLR return offsets for one source.
//! - `template_source_safepoints` prepares call maps in the existing record owner.
//!
//! # Invariants
//! - Entries exist independently of optional artifact or relocation capture.
//! - Safepoint records own source positions and canonical tagged locations.
//! - Emitters record the instruction after the call before stack/result cleanup.
//! - A tail branch has no new return address and never fabricates an entry.
//! - This borrow is compiler state only; runtime frames own no recorder.
//!
//! # See also
//! - `template::code` retains and charges the final immutable table.
//! - `otter_vm::native_abi::return_pc` resolves live caller code and exact sites.

use crate::entry::Unsupported;
use dynasmrt::AssemblyOffset;
use otter_vm::native_abi::{SafepointEntry, SafepointId};

pub(crate) struct ReturnSiteRecorder<'a> {
    pub(crate) entries: &'a mut Vec<SafepointEntry>,
    pub(crate) safepoint_id: SafepointId,
    pub(crate) logical_pc: u32,
}

impl ReturnSiteRecorder<'_> {
    pub(crate) fn record(&mut self, offset: AssemblyOffset) -> Result<(), Unsupported> {
        if self.safepoint_id == otter_vm::native_abi::NO_SAFEPOINT {
            return Err(Unsupported::OperandShape(
                "generated call lacks a safepoint",
            ));
        }
        let native_return_offset = u32::try_from(offset.0)
            .map_err(|_| Unsupported::OperandShape("native return offset exceeds code range"))?;
        self.entries.push(SafepointEntry {
            native_return_offset,
            safepoint_id: self.safepoint_id,
        });
        Ok(())
    }
}

/// Prepare exact Template call sources before entry metadata is frozen.
pub(crate) fn template_source_safepoints(
    plan: &mut crate::template::TemplatePlan,
) -> Result<std::collections::BTreeMap<u32, SafepointId>, Unsupported> {
    let mut next = plan
        .safepoint_records
        .iter()
        .map(|record| record.id)
        .max()
        .map_or(Some(1), |id| id.checked_add(1))
        .ok_or(Unsupported::OperandShape("safepoint ID space exhausted"))?;
    let mut sources = std::collections::BTreeMap::new();
    for instruction in &plan.instructions {
        if !crate::template::operation_is_js_call(instruction.op)
            || sources.contains_key(&instruction.pc)
        {
            continue;
        }
        if next == otter_vm::native_abi::NO_SAFEPOINT {
            return Err(Unsupported::OperandShape("safepoint ID space exhausted"));
        }
        let mut record = otter_vm::native_abi::SafepointRecord::window(
            next,
            otter_vm::native_abi::NO_FRAME_STATE,
        );
        record.call_pc = instruction.pc;
        sources.insert(instruction.pc, next);
        plan.safepoint_records.push(record);
        next = next
            .checked_add(1)
            .ok_or(Unsupported::OperandShape("safepoint ID space exhausted"))?;
    }
    Ok(sources)
}
