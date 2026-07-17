//! Deterministic text rendering of an already-built optimizing unit.
//!
//! # Contents
//! - [`render_optimized_unit`] — stable RPO-oriented IR, representation, and
//!   allocation listing used by `optimized-ir.txt`.
//! - [`instruction_byte_pc`] — source byte-coordinate lookup through an
//!   instruction's owning inline frame.
//!
//! # Invariants
//! - Rendering reads the single [`super::unit::OptimizedUnit`] consumed by the
//!   backend. It never rebuilds CFG, SSA, liveness, representation, allocation,
//!   frame states, or deopt metadata.
//! - Dense vectors and reverse postorder define every output order. Debug
//!   formatting is limited to deterministic enums and slices; no hash table,
//!   pointer, or executable address is rendered.
//! - Callers invoke this only after successful emission and only when artifact
//!   capture was requested.
//!
//! # See also
//! - [`super::pipeline`] for the sole unit constructor.
//! - [`super::arm64`] for native emission and code-map capture.

use std::fmt::Write as _;

use crate::ir::ssa::SsaInstr;

use super::unit::OptimizedUnit;

/// Cold serialized byte PC for one SSA instruction in its owning inline frame.
pub(crate) fn instruction_byte_pc(unit: &OptimizedUnit, instruction: &SsaInstr) -> u32 {
    unit.tree.frames[instruction.inline.0 as usize]
        .instructions
        .get(instruction.pc as usize)
        .map_or(u32::MAX, otter_vm::JitInstructionMetadata::byte_pc)
}

/// Render the already-owned optimizing analysis product.
pub(crate) fn render_optimized_unit(unit: &OptimizedUnit) -> String {
    let mut out = String::from("; otter optimized unit v1\n");
    writeln!(
        out,
        "; frames={} blocks={} values={} linear-scan-spills={} final-spills={}",
        unit.tree.frames.len(),
        unit.cfg.blocks.len(),
        unit.ssa.values.len(),
        unit.linear_scan_spill_slot_count,
        unit.spill_slot_count
    )
    .expect("writing to String cannot fail");

    for frame in &unit.tree.frames {
        writeln!(
            out,
            "frame i{} function={} registers={} parameters={} call-site={:?}",
            frame.id.0,
            frame.function_id,
            frame.code_block.register_count,
            frame.code_block.param_count,
            frame.call_site
        )
        .expect("writing to String cannot fail");
    }

    writeln!(out, "\nvalues").expect("writing to String cannot fail");
    for value in &unit.ssa.values {
        writeln!(
            out,
            "  v{} block=b{} repr={:?} location={:?} def={:?}",
            value.id.0,
            value.def_block.0,
            unit.reprs.representation(value.id),
            unit.allocation.locations[value.id.0 as usize],
            value.def
        )
        .expect("writing to String cannot fail");
    }

    writeln!(out, "\nblocks-rpo").expect("writing to String cannot fail");
    let mut operation_index = 0u32;
    for &block_id in unit.dom.reverse_postorder() {
        let block = &unit.cfg.blocks[block_id.0 as usize];
        writeln!(
            out,
            "b{} inline=i{} start={} idom={:?} loop={} preds={:?} succs={:?} exceptions={:?} terminator={:?}",
            block.id.0,
            block.inline.0,
            block.start_pc,
            unit.dom
                .immediate_dominator(block_id)
                .map(|dominator| dominator.0),
            block.is_loop_header,
            block.preds,
            block.normal_succs,
            block.exception_succs,
            block.terminator
        )
        .expect("writing to String cannot fail");
        for &phi in &unit.ssa.blocks[block_id.0 as usize].phis {
            writeln!(
                out,
                "  phi v{} {:?}",
                phi.0, unit.ssa.values[phi.0 as usize].def
            )
            .expect("writing to String cannot fail");
        }
        for instruction in &unit.ssa.blocks[block_id.0 as usize].instrs {
            writeln!(
                out,
                "  op={operation_index:04} i{}:pc{} byte={} {:?} inputs={:?} input-regs={:?} result={:?} result-reg={:?}",
                instruction.inline.0,
                instruction.pc,
                instruction_byte_pc(unit, instruction),
                instruction.op,
                instruction.inputs,
                instruction.input_registers,
                instruction.result,
                instruction.result_register
            )
            .expect("writing to String cannot fail");
            operation_index = operation_index.saturating_add(1);
        }
    }

    writeln!(out, "\nconversions").expect("writing to String cannot fail");
    for conversion in unit.reprs.conversions() {
        writeln!(out, "  {conversion:?}").expect("writing to String cannot fail");
    }

    writeln!(out, "\nedge-moves").expect("writing to String cannot fail");
    for edge in &unit.allocation.edge_moves {
        writeln!(
            out,
            "  b{} -> b{} {:?}",
            edge.predecessor.0, edge.block.0, edge.moves
        )
        .expect("writing to String cannot fail");
    }
    out
}
