//! Bytecode -> MIR lowering.
//!
//! Translates bytecode instructions into MIR operations, reading IC state
//! from the FeedbackVector to decide guard specialization.
//!
//! ## Register Model
//!
//! Bytecode has two index spaces:
//! - **LocalIndex** (0..local_count-1): local variable slots, including params at 0..param_count-1
//! - **Register** (0..N): scratch registers for temporaries
//!
//! In the interpreter's register window they're laid out as:
//! ```text
//! [local0..local(L-1) | scratch0..scratch(K-1)]
//!  └─── local_count ──┘ └─── scratch regs ─────┘
//! ```
//!
//! MIR LoadLocal/StoreLocal address local slots.
//! MIR LoadRegister/StoreRegister address scratch slots.
//! The MIR builder tracks scratch register values in `scratch_map`
//! so we don't emit redundant loads.

use std::collections::HashMap;

use otter_vm_bytecode::Function;
use otter_vm_bytecode::instruction::Instruction;

use crate::feedback::FeedbackSnapshot;
use crate::mir::graph::{BlockId, MirGraph};
use crate::mir::nodes::MirOp;

mod arithmetic;
mod blocks;
mod context;
mod control;
mod heap_ops;
mod value_ops;

use blocks::find_block_starts;
use context::BuilderContext;

/// Build a MIR graph from a bytecode function.
pub fn build_mir(function: &Function) -> MirGraph {
    let feedback = FeedbackSnapshot::from_function(function);
    let name = function
        .name
        .as_deref()
        .unwrap_or("<anonymous>")
        .to_string();
    let local_count = function.local_count;
    let register_count = function.register_count;
    let param_count = function.param_count;

    let mut ctx = BuilderContext::new(
        name,
        local_count,
        register_count,
        param_count as u16,
        &feedback,
    );
    let instructions = function.instructions.read();

    let block_starts = find_block_starts(instructions);

    let mut pc_to_block = HashMap::new();
    for &pc in &block_starts {
        if pc == 0 {
            pc_to_block.insert(0u32, ctx.graph.entry_block);
        } else {
            let bid = ctx.graph.create_block();
            pc_to_block.insert(pc as u32, bid);
        }
    }

    let mut current_block = ctx.graph.entry_block;
    for (pc, inst) in instructions.iter().enumerate() {
        let pc = pc as u32;

        if let Some(&bid) = pc_to_block.get(&pc)
            && bid != current_block
        {
            if !ctx.graph.block(current_block).is_terminated() {
                ctx.graph.push_instr(current_block, MirOp::Jump(bid), pc);
            }
            current_block = bid;
            ctx.invalidate_scratch_cache();
        }

        lower_instruction(&mut ctx, current_block, pc, inst, &pc_to_block);

        if ctx.graph.block(current_block).is_terminated() {
            if let Some(&next_bid) = pc_to_block.get(&(pc + 1)) {
                current_block = next_bid;
            } else {
                current_block = ctx.graph.create_block();
            }
            ctx.invalidate_scratch_cache();
        }
    }

    ctx.graph.recompute_edges();
    ctx.graph
}

fn lower_instruction(
    ctx: &mut BuilderContext<'_>,
    block: BlockId,
    pc: u32,
    inst: &Instruction,
    pc_to_block: &HashMap<u32, BlockId>,
) {
    if value_ops::lower_instruction(ctx, block, pc, inst) {
        return;
    }

    if heap_ops::lower_instruction(ctx, block, pc, inst, pc_to_block) {
        return;
    }

    if control::lower_instruction(ctx, block, pc, inst, pc_to_block) {
        return;
    }

    let deopt = ctx.make_deopt(pc);
    ctx.graph.push_instr(block, MirOp::Deopt(deopt), pc);
}
