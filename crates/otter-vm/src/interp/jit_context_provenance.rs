//! Register-origin proof for captured-context constant preparation.
//!
//! # Contents
//! - Whole-function must-origin classification from schema-declared writes.
//! - Captured reads disjoint from explicit context-slot stores.
//!
//! # Invariants
//! - Created and copied contexts own fresh depth-zero slots disjoint from captured storage.
//! - Parent and unknown slot writes decline without guessing a captured coordinate.
//! - Parameters and exception destinations have implicit unknown definitions.
//! - Existing ContextSlot invalidation still guards indirect and reentrant writes.
//! - This pass runs during compilation preparation and allocates no GC values.
//!
//! # See also
//! - `jit_bake` retains values and schedules install-time watch verification.
//! - `otter_bytecode::opcode_schema` owns register write widths and operands.

use otter_bytecode::{ContextCoord, Op, opcode_schema::register_access_at};
use rustc_hash::FxHashSet;

use crate::executable::CodeBlock;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    Unassigned,
    Fresh,
    Captured,
    Unknown,
}

/// Captured-context reads disjoint from potentially aliasing explicit slot stores.
///
/// The classification is intentionally independent of control-flow order: every
/// explicit or implicit definition must agree. Moves and mixed definitions are
/// unknown. A non-parameter register starts as undefined, which cannot mutate a
/// context before its first fresh definition. The receiver is a separate frame
/// field; loading it into a register is an ordinary schema-declared definition.
pub(super) fn unwritten_captured_slots(code: &CodeBlock) -> Vec<ContextCoord> {
    let mut origins = vec![Origin::Unassigned; usize::from(code.register_count)];
    origins[..usize::from(code.param_count)].fill(Origin::Unknown);
    for handler in code.control_flow().handlers() {
        origins[usize::from(handler.exception)] = Origin::Unknown;
    }
    for instruction in code.code.iter() {
        let op = code.op(instruction);
        for (position, &register) in code.operand_words(instruction).iter().enumerate() {
            let access = register_access_at(op, position);
            if !access.writes() {
                continue;
            }
            let definition = match (op, position) {
                (Op::CreateContext | Op::CopyContext, 0) => Origin::Fresh,
                (Op::LoadClosureContext, 0) => Origin::Captured,
                _ => Origin::Unknown,
            };
            for offset in 0..usize::from(access.width()) {
                let origin = &mut origins[register as usize + offset];
                *origin = match *origin {
                    Origin::Unassigned => definition,
                    previous if previous == definition => previous,
                    _ => Origin::Unknown,
                };
            }
        }
    }

    let mut seen = FxHashSet::default();
    for instruction in code.code.iter() {
        let coordinate = match code.op(instruction) {
            Op::StoreContextSlot | Op::StoreContextSlotChecked | Op::BindThisContextSlot => 2,
            Op::StoreLookupSlot => 3,
            _ => continue,
        };
        let coord = ContextCoord::from_imm32(instruction.imm(coordinate))
            .expect("verified context coordinate");
        match origins[usize::from(instruction.reg(1))] {
            Origin::Fresh if coord.depth == 0 => {}
            Origin::Captured => {
                seen.insert((coord.depth, coord.slot));
            }
            // A fresh parent can be this closure's captured context, and an
            // unknown base can reach any captured coordinate. Neither may be
            // compared directly with a LoadClosureContext-relative depth.
            _ => return Vec::new(),
        }
    }

    code.code
        .iter()
        .filter_map(|instruction| {
            if !matches!(
                code.op(instruction),
                Op::LoadContextSlot | Op::LoadContextSlotChecked
            ) || origins[usize::from(instruction.reg(1))] != Origin::Captured
            {
                return None;
            }
            let coord =
                ContextCoord::from_imm32(instruction.imm(2)).expect("verified context coordinate");
            seen.insert((coord.depth, coord.slot)).then_some(coord)
        })
        .collect()
}
