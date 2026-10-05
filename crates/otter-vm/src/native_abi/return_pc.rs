//! Exact expected-generation resolution of suspended JavaScript return sites.
//!
//! # Contents
//! - Installation validation and checked return-address lookup.
//! - One compact association table referencing existing safepoint records.
//!
//! # Invariants
//! Lookup never scans mappings or generation history. A nonzero native anchor
//! must resolve in the immediate physical caller's exact code object; missing
//! or malformed metadata is a refusal, never an old-PC fallback. Invalid active
//! generations keep this same table until their mapping retires.
//!
//! # See also
//! - `super::Frame` and `super::CallRequest` own the transferred live anchor.
//! - `crate::jit_registry` retains executable mappings and the metadata together.

use super::{NO_CALL_PC, NO_SAFEPOINT, SafepointRecord};
use crate::jit::JitFunctionCode;

/// Validate all associations before a code object can become installed.
/// Empty tables need no native mapping address (pure fixture owners may omit it).
pub(crate) fn valid_return_sites(code: &dyn JitFunctionCode) -> bool {
    let sites = code.return_sites();
    if sites.is_empty() {
        return true;
    }
    let Some(base) = code.native_code_address().filter(|base| *base != 0) else {
        return false;
    };
    if base.checked_add(code.code_len() as u64).is_none() {
        return false;
    }
    sites
        .windows(2)
        .all(|pair| pair[0].native_return_offset < pair[1].native_return_offset)
        && sites.iter().all(|site| {
            site.native_return_offset != 0
                && (site.native_return_offset as usize) < code.code_len()
                && site.safepoint_id != NO_SAFEPOINT
                && code
                    .safepoint_record(site.safepoint_id)
                    .is_some_and(|record| {
                        record.id == site.safepoint_id && record.call_pc != NO_CALL_PC
                    })
        })
}

/// Resolve an absolute address in the complete mapping of exactly `code`.
/// The offset must name a registered return, not merely fall inside its bytes.
pub(crate) fn return_pc_record(
    code: &dyn JitFunctionCode,
    return_pc: u64,
) -> Option<&SafepointRecord> {
    let offset = return_pc.checked_sub(code.native_code_address()?)?;
    if offset == 0 || offset >= code.code_len() as u64 {
        return None;
    }
    let offset = u32::try_from(offset).ok()?;
    let sites = code.return_sites();
    let site = sites.get(
        sites
            .binary_search_by_key(&offset, |site| site.native_return_offset)
            .ok()?,
    )?;
    let record = code.safepoint_record(site.safepoint_id)?;
    (record.id == site.safepoint_id && record.call_pc != NO_CALL_PC).then_some(record)
}

#[cfg(test)]
mod tests;
