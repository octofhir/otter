//! Typed x86 instruction proofs over exact captured operation regions.
//!
//! # Contents
//! - Complete decoding with instruction-boundary validation.
//! - Base/displacement operands for canonical homes and allocation payloads.
//!
//! # Invariants
//! Every byte belongs to a decoded instruction; invalid bytes and truncated
//! regions fail the proof instead of disappearing into a textual fallback.
//!
//! # See also
//! `jit_empty_allocations` and `jit_canonical_homes_gc` join native bytes to
//! independently captured VM allocation and deoptimization recipes.

use yaxpeax_arch::LengthedInstruction;
use yaxpeax_x86::amd64::{InstDecoder, Instruction, Operand, RegSpec};

pub fn decode(code: &[u8], start: usize, end: usize) -> Vec<Instruction> {
    let decoder = InstDecoder::default();
    let bytes = &code[start..end];
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let instruction = decoder
            .decode_slice(&bytes[offset..])
            .unwrap_or_else(|error| {
                panic!("invalid native instruction at {}: {error}", start + offset)
            });
        let length = instruction.len().to_const() as usize;
        assert!(length > 0 && offset + length <= bytes.len());
        offset += length;
        result.push(instruction);
    }
    assert_eq!(offset, bytes.len(), "exact operation instruction boundary");
    result
}

pub fn base_offset(operand: Operand) -> Option<(RegSpec, i32)> {
    match operand {
        Operand::MemDeref { base } => Some((base, 0)),
        Operand::Disp { base, disp } => Some((base, disp)),
        _ => None,
    }
}
