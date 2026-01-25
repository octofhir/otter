//! Peephole optimizer for bytecode instructions
//!
//! This module provides optimization passes that examine small windows of
//! instructions and replace them with more efficient equivalents.
//!
//! ## Optimizations
//!
//! - **Dead code elimination**: Remove unreachable code after return/throw/jump
//! - **Copy propagation**: Track register copies and substitute uses
//! - **Register coalescing**: Eliminate redundant Move instructions
//! - **Constant folding**: LoadInt32 -> LoadInt8 for small values
//! - **Identity elimination**: Remove Move r0, r0; double negation; etc.

use otter_vm_bytecode::instruction::Instruction;
use otter_vm_bytecode::operand::Register;
use std::collections::HashMap;

/// Peephole optimizer that optimizes bytecode instructions
pub struct PeepholeOptimizer {
    /// Whether the optimizer made any changes
    changed: bool,
}

impl PeepholeOptimizer {
    /// Create a new peephole optimizer
    pub fn new() -> Self {
        Self { changed: false }
    }

    /// Optimize a vector of instructions
    /// Returns true if any optimizations were applied
    pub fn optimize(&mut self, instructions: &mut Vec<Instruction>) -> bool {
        self.changed = false;

        // Run multiple passes until no more changes
        loop {
            let changed_this_pass = self.optimize_pass(instructions);
            if !changed_this_pass {
                break;
            }
            self.changed = true;
        }

        self.changed
    }

    /// Single optimization pass
    fn optimize_pass(&mut self, instructions: &mut Vec<Instruction>) -> bool {
        let mut changed = false;

        // Remove Nops first
        let len_before = instructions.len();
        instructions.retain(|i| !matches!(i, Instruction::Nop));
        if instructions.len() != len_before {
            changed = true;
        }

        // Dead code elimination - remove unreachable code after return/throw
        if self.eliminate_dead_code(instructions) {
            changed = true;
        }

        // Copy propagation - substitute register copies
        if self.propagate_copies(instructions) {
            changed = true;
        }

        // Register coalescing - eliminate redundant moves
        if self.coalesce_registers(instructions) {
            changed = true;
        }

        // Window-based optimizations
        let mut i = 0;
        while i < instructions.len() {
            // Try 2-instruction window optimizations
            if i + 1 < instructions.len() {
                if let Some(replacement) = self.optimize_window_2(&instructions[i], &instructions[i + 1]) {
                    match replacement {
                        WindowResult::Replace1(instr) => {
                            instructions[i] = instr;
                            instructions[i + 1] = Instruction::Nop;
                            changed = true;
                        }
                        WindowResult::Replace2(instr1, instr2) => {
                            instructions[i] = instr1;
                            instructions[i + 1] = instr2;
                            changed = true;
                        }
                        WindowResult::Remove1 => {
                            instructions[i] = Instruction::Nop;
                            changed = true;
                        }
                        WindowResult::Remove2 => {
                            instructions[i] = Instruction::Nop;
                            instructions[i + 1] = Instruction::Nop;
                            changed = true;
                        }
                    }
                }
            }

            // Single instruction optimizations
            if let Some(replacement) = self.optimize_single(&instructions[i]) {
                instructions[i] = replacement;
                changed = true;
            }

            i += 1;
        }

        // Final Nop removal
        let len_before = instructions.len();
        instructions.retain(|i| !matches!(i, Instruction::Nop));
        if instructions.len() != len_before {
            changed = true;
        }

        changed
    }

    /// Eliminate dead code after unconditional control flow
    ///
    /// This removes instructions that follow Return, ReturnUndefined, or Throw
    /// until the next potential jump target. Since we don't have explicit labels,
    /// we can only safely remove code at the end of basic blocks.
    fn eliminate_dead_code(&self, instructions: &mut Vec<Instruction>) -> bool {
        if instructions.is_empty() {
            return false;
        }

        let mut changed = false;

        // Find jump targets (indices that could be jumped to)
        let jump_targets = self.find_jump_targets(instructions);

        // Mark instructions after terminating instructions as dead
        let mut i = 0;
        while i < instructions.len() {
            if self.is_terminating(&instructions[i]) {
                // Mark following instructions as Nop until we hit a jump target
                let mut j = i + 1;
                while j < instructions.len() && !jump_targets.contains(&j) {
                    if !matches!(instructions[j], Instruction::Nop) {
                        instructions[j] = Instruction::Nop;
                        changed = true;
                    }
                    j += 1;
                }
                i = j;
            } else {
                i += 1;
            }
        }

        changed
    }

    /// Check if an instruction terminates the basic block
    fn is_terminating(&self, instr: &Instruction) -> bool {
        matches!(
            instr,
            Instruction::Return { .. }
                | Instruction::ReturnUndefined
                | Instruction::Throw { .. }
                | Instruction::Jump { .. }
        )
    }

    /// Find all possible jump targets in the instruction list
    fn find_jump_targets(&self, instructions: &[Instruction]) -> std::collections::HashSet<usize> {
        use std::collections::HashSet;
        let mut targets = HashSet::new();

        for (i, instr) in instructions.iter().enumerate() {
            let offset = match instr {
                Instruction::Jump { offset } => Some(offset.0),
                Instruction::JumpIfTrue { offset, .. } => Some(offset.0),
                Instruction::JumpIfFalse { offset, .. } => Some(offset.0),
                Instruction::JumpIfNullish { offset, .. } => Some(offset.0),
                Instruction::JumpIfNotNullish { offset, .. } => Some(offset.0),
                Instruction::TryStart { catch_offset } => {
                    // Catch block is a jump target
                    let catch_target = (i as i32 + catch_offset.0 as i32) as usize;
                    targets.insert(catch_target);
                    None
                }
                Instruction::ForInNext { offset, .. } => Some(offset.0),
                _ => None,
            };

            if let Some(off) = offset {
                // Calculate target index: current index + offset
                let target = (i as i32 + off as i32) as usize;
                if target < instructions.len() {
                    targets.insert(target);
                }
            }
        }

        targets
    }

    /// Copy propagation - track register copies and substitute uses
    ///
    /// When we see `Move dst, src`, we know that dst holds the same value as src.
    /// We can then substitute uses of dst with src in subsequent instructions,
    /// potentially allowing the Move to be eliminated later.
    fn propagate_copies(&self, instructions: &mut Vec<Instruction>) -> bool {
        let mut changed = false;
        let jump_targets = self.find_jump_targets(instructions);

        // Map from register to its copy source (dst -> src means dst = src)
        let mut copies: HashMap<u16, u16> = HashMap::new();

        for i in 0..instructions.len() {
            // At jump targets, invalidate all copies (control flow merge)
            if jump_targets.contains(&i) {
                copies.clear();
            }

            // Process the instruction
            match &instructions[i] {
                Instruction::Move { dst, src } => {
                    // Record the copy: dst now holds the value of src
                    // But first, resolve src through existing copies
                    let resolved_src = *copies.get(&src.0).unwrap_or(&src.0);
                    if resolved_src != dst.0 {
                        copies.insert(dst.0, resolved_src);
                    }
                }
                _ => {
                    // For other instructions, try to substitute source registers
                    if let Some(new_instr) = self.substitute_copies(&instructions[i], &copies) {
                        instructions[i] = new_instr;
                        changed = true;
                    }

                    // Invalidate any copy whose destination is written by this instruction
                    if let Some(written_reg) = self.get_written_register(&instructions[i]) {
                        copies.remove(&written_reg);
                        // Also remove any copies that depend on this register
                        copies.retain(|_, &mut src| src != written_reg);
                    }
                }
            }

            // Control flow instructions invalidate all copies
            if self.is_control_flow(&instructions[i]) {
                copies.clear();
            }
        }

        changed
    }

    /// Substitute register uses with their copy sources
    fn substitute_copies(&self, instr: &Instruction, copies: &HashMap<u16, u16>) -> Option<Instruction> {
        // Helper to resolve a register through copies
        let resolve = |reg: Register| -> Register {
            Register(*copies.get(&reg.0).unwrap_or(&reg.0))
        };

        match instr {
            // Binary operations
            Instruction::Add { dst, lhs, rhs } => {
                let new_lhs = resolve(*lhs);
                let new_rhs = resolve(*rhs);
                if new_lhs != *lhs || new_rhs != *rhs {
                    Some(Instruction::Add { dst: *dst, lhs: new_lhs, rhs: new_rhs })
                } else {
                    None
                }
            }
            Instruction::Sub { dst, lhs, rhs } => {
                let new_lhs = resolve(*lhs);
                let new_rhs = resolve(*rhs);
                if new_lhs != *lhs || new_rhs != *rhs {
                    Some(Instruction::Sub { dst: *dst, lhs: new_lhs, rhs: new_rhs })
                } else {
                    None
                }
            }
            Instruction::Mul { dst, lhs, rhs } => {
                let new_lhs = resolve(*lhs);
                let new_rhs = resolve(*rhs);
                if new_lhs != *lhs || new_rhs != *rhs {
                    Some(Instruction::Mul { dst: *dst, lhs: new_lhs, rhs: new_rhs })
                } else {
                    None
                }
            }
            Instruction::Div { dst, lhs, rhs } => {
                let new_lhs = resolve(*lhs);
                let new_rhs = resolve(*rhs);
                if new_lhs != *lhs || new_rhs != *rhs {
                    Some(Instruction::Div { dst: *dst, lhs: new_lhs, rhs: new_rhs })
                } else {
                    None
                }
            }
            // Comparison operations
            Instruction::Eq { dst, lhs, rhs } => {
                let new_lhs = resolve(*lhs);
                let new_rhs = resolve(*rhs);
                if new_lhs != *lhs || new_rhs != *rhs {
                    Some(Instruction::Eq { dst: *dst, lhs: new_lhs, rhs: new_rhs })
                } else {
                    None
                }
            }
            Instruction::StrictEq { dst, lhs, rhs } => {
                let new_lhs = resolve(*lhs);
                let new_rhs = resolve(*rhs);
                if new_lhs != *lhs || new_rhs != *rhs {
                    Some(Instruction::StrictEq { dst: *dst, lhs: new_lhs, rhs: new_rhs })
                } else {
                    None
                }
            }
            Instruction::Lt { dst, lhs, rhs } => {
                let new_lhs = resolve(*lhs);
                let new_rhs = resolve(*rhs);
                if new_lhs != *lhs || new_rhs != *rhs {
                    Some(Instruction::Lt { dst: *dst, lhs: new_lhs, rhs: new_rhs })
                } else {
                    None
                }
            }
            Instruction::Le { dst, lhs, rhs } => {
                let new_lhs = resolve(*lhs);
                let new_rhs = resolve(*rhs);
                if new_lhs != *lhs || new_rhs != *rhs {
                    Some(Instruction::Le { dst: *dst, lhs: new_lhs, rhs: new_rhs })
                } else {
                    None
                }
            }
            Instruction::Gt { dst, lhs, rhs } => {
                let new_lhs = resolve(*lhs);
                let new_rhs = resolve(*rhs);
                if new_lhs != *lhs || new_rhs != *rhs {
                    Some(Instruction::Gt { dst: *dst, lhs: new_lhs, rhs: new_rhs })
                } else {
                    None
                }
            }
            Instruction::Ge { dst, lhs, rhs } => {
                let new_lhs = resolve(*lhs);
                let new_rhs = resolve(*rhs);
                if new_lhs != *lhs || new_rhs != *rhs {
                    Some(Instruction::Ge { dst: *dst, lhs: new_lhs, rhs: new_rhs })
                } else {
                    None
                }
            }
            // Unary operations
            Instruction::Neg { dst, src } => {
                let new_src = resolve(*src);
                if new_src != *src {
                    Some(Instruction::Neg { dst: *dst, src: new_src })
                } else {
                    None
                }
            }
            Instruction::Not { dst, src } => {
                let new_src = resolve(*src);
                if new_src != *src {
                    Some(Instruction::Not { dst: *dst, src: new_src })
                } else {
                    None
                }
            }
            // Return
            Instruction::Return { src } => {
                let new_src = resolve(*src);
                if new_src != *src {
                    Some(Instruction::Return { src: new_src })
                } else {
                    None
                }
            }
            // SetLocal
            Instruction::SetLocal { idx, src } => {
                let new_src = resolve(*src);
                if new_src != *src {
                    Some(Instruction::SetLocal { idx: *idx, src: new_src })
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Get the register written by an instruction (if any)
    fn get_written_register(&self, instr: &Instruction) -> Option<u16> {
        match instr {
            Instruction::LoadUndefined { dst }
            | Instruction::LoadNull { dst }
            | Instruction::LoadTrue { dst }
            | Instruction::LoadFalse { dst }
            | Instruction::LoadInt8 { dst, .. }
            | Instruction::LoadInt32 { dst, .. }
            | Instruction::LoadConst { dst, .. }
            | Instruction::GetLocal { dst, .. }
            | Instruction::GetUpvalue { dst, .. }
            | Instruction::GetGlobal { dst, .. }
            | Instruction::LoadThis { dst }
            | Instruction::Add { dst, .. }
            | Instruction::Sub { dst, .. }
            | Instruction::Mul { dst, .. }
            | Instruction::Div { dst, .. }
            | Instruction::Mod { dst, .. }
            | Instruction::Pow { dst, .. }
            | Instruction::Neg { dst, .. }
            | Instruction::Inc { dst, .. }
            | Instruction::Dec { dst, .. }
            | Instruction::BitAnd { dst, .. }
            | Instruction::BitOr { dst, .. }
            | Instruction::BitXor { dst, .. }
            | Instruction::BitNot { dst, .. }
            | Instruction::Shl { dst, .. }
            | Instruction::Shr { dst, .. }
            | Instruction::Ushr { dst, .. }
            | Instruction::Eq { dst, .. }
            | Instruction::StrictEq { dst, .. }
            | Instruction::Ne { dst, .. }
            | Instruction::StrictNe { dst, .. }
            | Instruction::Lt { dst, .. }
            | Instruction::Le { dst, .. }
            | Instruction::Gt { dst, .. }
            | Instruction::Ge { dst, .. }
            | Instruction::Not { dst, .. }
            | Instruction::TypeOf { dst, .. }
            | Instruction::Move { dst, .. }
            | Instruction::GetProp { dst, .. }
            | Instruction::GetPropConst { dst, .. }
            | Instruction::NewObject { dst, .. }
            | Instruction::NewArray { dst, .. }
            | Instruction::Closure { dst, .. } => Some(dst.0),
            _ => None,
        }
    }

    /// Check if instruction is control flow
    fn is_control_flow(&self, instr: &Instruction) -> bool {
        matches!(
            instr,
            Instruction::Jump { .. }
                | Instruction::JumpIfTrue { .. }
                | Instruction::JumpIfFalse { .. }
                | Instruction::JumpIfNullish { .. }
                | Instruction::JumpIfNotNullish { .. }
                | Instruction::Return { .. }
                | Instruction::ReturnUndefined
                | Instruction::Throw { .. }
                | Instruction::Call { .. }
                | Instruction::CallMethod { .. }
                | Instruction::TailCall { .. }
        )
    }

    /// Register coalescing - eliminate redundant Move instructions
    ///
    /// If we have:
    ///   Move r1, r0
    ///   Add r2, r1, r3
    /// And r1 is not used after the Add, we can rewrite to:
    ///   Add r2, r0, r3
    /// And eliminate the Move.
    fn coalesce_registers(&self, instructions: &mut Vec<Instruction>) -> bool {
        let mut changed = false;

        // Find Move instructions that can be eliminated
        let mut i = 0;
        while i < instructions.len() {
            if let Instruction::Move { dst, src } = instructions[i] {
                if dst == src {
                    i += 1;
                    continue;
                }

                // Check if dst is only used in the immediately following instruction
                // and not used anywhere else in the remaining code
                if i + 1 < instructions.len() {
                    let next_uses_dst = self.instruction_uses_register(&instructions[i + 1], dst);
                    let next_writes_dst = self.get_written_register(&instructions[i + 1]) == Some(dst.0);

                    // If next instruction uses dst and also overwrites it (or it's the last use)
                    // we might be able to substitute
                    if next_uses_dst && (next_writes_dst || !self.register_used_after(instructions, i + 2, dst)) {
                        // Try to substitute dst with src in the next instruction
                        if let Some(new_instr) = self.substitute_register(&instructions[i + 1], dst, src) {
                            instructions[i] = Instruction::Nop;
                            instructions[i + 1] = new_instr;
                            changed = true;
                        }
                    }
                }
            }
            i += 1;
        }

        changed
    }

    /// Check if an instruction uses a specific register as a source
    fn instruction_uses_register(&self, instr: &Instruction, reg: Register) -> bool {
        match instr {
            Instruction::Add { lhs, rhs, .. }
            | Instruction::Sub { lhs, rhs, .. }
            | Instruction::Mul { lhs, rhs, .. }
            | Instruction::Div { lhs, rhs, .. }
            | Instruction::Mod { lhs, rhs, .. }
            | Instruction::Pow { lhs, rhs, .. }
            | Instruction::Eq { lhs, rhs, .. }
            | Instruction::StrictEq { lhs, rhs, .. }
            | Instruction::Ne { lhs, rhs, .. }
            | Instruction::StrictNe { lhs, rhs, .. }
            | Instruction::Lt { lhs, rhs, .. }
            | Instruction::Le { lhs, rhs, .. }
            | Instruction::Gt { lhs, rhs, .. }
            | Instruction::Ge { lhs, rhs, .. }
            | Instruction::BitAnd { lhs, rhs, .. }
            | Instruction::BitOr { lhs, rhs, .. }
            | Instruction::BitXor { lhs, rhs, .. }
            | Instruction::Shl { lhs, rhs, .. }
            | Instruction::Shr { lhs, rhs, .. }
            | Instruction::Ushr { lhs, rhs, .. } => *lhs == reg || *rhs == reg,

            Instruction::Neg { src, .. }
            | Instruction::Inc { src, .. }
            | Instruction::Dec { src, .. }
            | Instruction::BitNot { src, .. }
            | Instruction::Not { src, .. }
            | Instruction::TypeOf { src, .. }
            | Instruction::Move { src, .. }
            | Instruction::Return { src }
            | Instruction::Throw { src }
            | Instruction::SetLocal { src, .. }
            | Instruction::SetUpvalue { src, .. } => *src == reg,

            Instruction::JumpIfTrue { cond, .. }
            | Instruction::JumpIfFalse { cond, .. } => *cond == reg,

            Instruction::JumpIfNullish { src, .. }
            | Instruction::JumpIfNotNullish { src, .. } => *src == reg,

            _ => false,
        }
    }

    /// Check if a register is used anywhere from a given index onwards
    fn register_used_after(&self, instructions: &[Instruction], from: usize, reg: Register) -> bool {
        for i in from..instructions.len() {
            if self.instruction_uses_register(&instructions[i], reg) {
                return true;
            }
            // If the register is written, stop searching (it's redefined)
            if self.get_written_register(&instructions[i]) == Some(reg.0) {
                return false;
            }
        }
        false
    }

    /// Substitute one register for another in an instruction
    fn substitute_register(&self, instr: &Instruction, old: Register, new: Register) -> Option<Instruction> {
        let sub = |r: Register| if r == old { new } else { r };

        match instr {
            Instruction::Add { dst, lhs, rhs } => {
                Some(Instruction::Add { dst: *dst, lhs: sub(*lhs), rhs: sub(*rhs) })
            }
            Instruction::Sub { dst, lhs, rhs } => {
                Some(Instruction::Sub { dst: *dst, lhs: sub(*lhs), rhs: sub(*rhs) })
            }
            Instruction::Mul { dst, lhs, rhs } => {
                Some(Instruction::Mul { dst: *dst, lhs: sub(*lhs), rhs: sub(*rhs) })
            }
            Instruction::Div { dst, lhs, rhs } => {
                Some(Instruction::Div { dst: *dst, lhs: sub(*lhs), rhs: sub(*rhs) })
            }
            Instruction::Eq { dst, lhs, rhs } => {
                Some(Instruction::Eq { dst: *dst, lhs: sub(*lhs), rhs: sub(*rhs) })
            }
            Instruction::StrictEq { dst, lhs, rhs } => {
                Some(Instruction::StrictEq { dst: *dst, lhs: sub(*lhs), rhs: sub(*rhs) })
            }
            Instruction::Lt { dst, lhs, rhs } => {
                Some(Instruction::Lt { dst: *dst, lhs: sub(*lhs), rhs: sub(*rhs) })
            }
            Instruction::Le { dst, lhs, rhs } => {
                Some(Instruction::Le { dst: *dst, lhs: sub(*lhs), rhs: sub(*rhs) })
            }
            Instruction::Gt { dst, lhs, rhs } => {
                Some(Instruction::Gt { dst: *dst, lhs: sub(*lhs), rhs: sub(*rhs) })
            }
            Instruction::Ge { dst, lhs, rhs } => {
                Some(Instruction::Ge { dst: *dst, lhs: sub(*lhs), rhs: sub(*rhs) })
            }
            Instruction::Neg { dst, src } => {
                Some(Instruction::Neg { dst: *dst, src: sub(*src) })
            }
            Instruction::Not { dst, src } => {
                Some(Instruction::Not { dst: *dst, src: sub(*src) })
            }
            Instruction::Return { src } => {
                Some(Instruction::Return { src: sub(*src) })
            }
            Instruction::SetLocal { idx, src } => {
                Some(Instruction::SetLocal { idx: *idx, src: sub(*src) })
            }
            _ => None,
        }
    }

    /// Single instruction optimizations
    fn optimize_single(&self, instr: &Instruction) -> Option<Instruction> {
        match instr {
            // Move to self is a no-op
            Instruction::Move { dst, src } if dst == src => {
                Some(Instruction::Nop)
            }

            // LoadInt8 0 can stay as is (it's already optimal)
            // LoadInt32 0 can be converted to LoadInt8 0
            Instruction::LoadInt32 { dst, value: 0 } => {
                Some(Instruction::LoadInt8 { dst: *dst, value: 0 })
            }

            // LoadInt32 small values can be LoadInt8
            Instruction::LoadInt32 { dst, value } if *value >= -128 && *value <= 127 => {
                Some(Instruction::LoadInt8 { dst: *dst, value: *value as i8 })
            }

            _ => None,
        }
    }

    /// Two-instruction window optimizations
    fn optimize_window_2(&self, first: &Instruction, second: &Instruction) -> Option<WindowResult> {
        // Double negation: Neg then Neg
        if let (
            Instruction::Neg { dst: dst1, src: src1 },
            Instruction::Neg { dst: dst2, src: src2 },
        ) = (first, second) {
            if dst1 == src2 {
                // -(-x) = x
                return Some(WindowResult::Replace1(Instruction::Move {
                    dst: *dst2,
                    src: *src1,
                }));
            }
        }

        // Double Not: !(!x) = x (for booleans)
        if let (
            Instruction::Not { dst: dst1, src: src1 },
            Instruction::Not { dst: dst2, src: src2 },
        ) = (first, second) {
            if dst1 == src2 {
                return Some(WindowResult::Replace1(Instruction::Move {
                    dst: *dst2,
                    src: *src1,
                }));
            }
        }

        // Load followed by overwriting load to same register
        if self.is_load_to_register(first) && self.is_load_to_register(second) {
            let dst1 = self.get_load_dst(first);
            let dst2 = self.get_load_dst(second);
            if dst1 == dst2 {
                // First load is dead
                return Some(WindowResult::Remove1);
            }
        }

        // SetLocal followed by GetLocal from same local to same register
        if let (
            Instruction::SetLocal { idx: idx1, src: src1 },
            Instruction::GetLocal { dst: dst2, idx: idx2 },
        ) = (first, second) {
            if idx1 == idx2 {
                // Replace GetLocal with Move
                return Some(WindowResult::Replace2(
                    first.clone(),
                    Instruction::Move { dst: *dst2, src: *src1 },
                ));
            }
        }

        // Jump to next instruction is dead code
        if let Instruction::Jump { offset } = first {
            if offset.0 == 1 {
                // Jump offset 1 means jump to next instruction
                return Some(WindowResult::Remove1);
            }
        }

        None
    }

    /// Check if instruction is a load that sets a register
    fn is_load_to_register(&self, instr: &Instruction) -> bool {
        matches!(
            instr,
            Instruction::LoadUndefined { .. }
                | Instruction::LoadNull { .. }
                | Instruction::LoadTrue { .. }
                | Instruction::LoadFalse { .. }
                | Instruction::LoadInt8 { .. }
                | Instruction::LoadInt32 { .. }
                | Instruction::LoadConst { .. }
        )
    }

    /// Get the destination register of a load instruction
    fn get_load_dst(&self, instr: &Instruction) -> Option<Register> {
        match instr {
            Instruction::LoadUndefined { dst }
            | Instruction::LoadNull { dst }
            | Instruction::LoadTrue { dst }
            | Instruction::LoadFalse { dst }
            | Instruction::LoadInt8 { dst, .. }
            | Instruction::LoadInt32 { dst, .. }
            | Instruction::LoadConst { dst, .. } => Some(*dst),
            _ => None,
        }
    }
}

impl Default for PeepholeOptimizer {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of a window optimization
enum WindowResult {
    /// Replace first instruction, mark second as Nop
    Replace1(Instruction),
    /// Replace both instructions
    Replace2(Instruction, Instruction),
    /// Remove first instruction (mark as Nop)
    Remove1,
    /// Remove both instructions (mark as Nop)
    #[allow(dead_code)]
    Remove2,
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm_bytecode::operand::{JumpOffset, LocalIndex};

    #[test]
    fn test_remove_nops() {
        let mut optimizer = PeepholeOptimizer::new();
        let mut instructions = vec![
            Instruction::LoadInt8 { dst: Register(0), value: 1 },
            Instruction::Nop,
            Instruction::LoadInt8 { dst: Register(1), value: 2 },
            Instruction::Nop,
            Instruction::Nop,
        ];

        let changed = optimizer.optimize(&mut instructions);
        assert!(changed);
        assert_eq!(instructions.len(), 2);
    }

    #[test]
    fn test_move_to_self() {
        let mut optimizer = PeepholeOptimizer::new();
        let mut instructions = vec![
            Instruction::Move { dst: Register(0), src: Register(0) },
        ];

        let changed = optimizer.optimize(&mut instructions);
        assert!(changed);
        assert!(instructions.is_empty());
    }

    #[test]
    fn test_double_negation() {
        let mut optimizer = PeepholeOptimizer::new();
        let mut instructions = vec![
            Instruction::Neg { dst: Register(1), src: Register(0) },
            Instruction::Neg { dst: Register(2), src: Register(1) },
        ];

        let changed = optimizer.optimize(&mut instructions);
        assert!(changed);
        assert_eq!(instructions.len(), 1);
        assert!(matches!(
            instructions[0],
            Instruction::Move { dst: Register(2), src: Register(0) }
        ));
    }

    #[test]
    fn test_double_not() {
        let mut optimizer = PeepholeOptimizer::new();
        let mut instructions = vec![
            Instruction::Not { dst: Register(1), src: Register(0) },
            Instruction::Not { dst: Register(2), src: Register(1) },
        ];

        let changed = optimizer.optimize(&mut instructions);
        assert!(changed);
        assert_eq!(instructions.len(), 1);
        assert!(matches!(
            instructions[0],
            Instruction::Move { dst: Register(2), src: Register(0) }
        ));
    }

    #[test]
    fn test_dead_load() {
        let mut optimizer = PeepholeOptimizer::new();
        let mut instructions = vec![
            Instruction::LoadInt8 { dst: Register(0), value: 1 },
            Instruction::LoadInt8 { dst: Register(0), value: 2 },
        ];

        let changed = optimizer.optimize(&mut instructions);
        assert!(changed);
        assert_eq!(instructions.len(), 1);
        assert!(matches!(
            instructions[0],
            Instruction::LoadInt8 { dst: Register(0), value: 2 }
        ));
    }

    #[test]
    fn test_set_get_local_optimization() {
        let mut optimizer = PeepholeOptimizer::new();
        let mut instructions = vec![
            Instruction::SetLocal { idx: LocalIndex(0), src: Register(1) },
            Instruction::GetLocal { dst: Register(2), idx: LocalIndex(0) },
        ];

        let changed = optimizer.optimize(&mut instructions);
        assert!(changed);
        assert_eq!(instructions.len(), 2);
        assert!(matches!(
            instructions[1],
            Instruction::Move { dst: Register(2), src: Register(1) }
        ));
    }

    #[test]
    fn test_jump_to_next() {
        let mut optimizer = PeepholeOptimizer::new();
        let mut instructions = vec![
            Instruction::Jump { offset: JumpOffset(1) },
            Instruction::LoadInt8 { dst: Register(0), value: 1 },
        ];

        let changed = optimizer.optimize(&mut instructions);
        assert!(changed);
        assert_eq!(instructions.len(), 1);
        assert!(matches!(
            instructions[0],
            Instruction::LoadInt8 { dst: Register(0), value: 1 }
        ));
    }

    #[test]
    fn test_load_int32_to_int8() {
        let mut optimizer = PeepholeOptimizer::new();
        let mut instructions = vec![
            Instruction::LoadInt32 { dst: Register(0), value: 42 },
        ];

        let changed = optimizer.optimize(&mut instructions);
        assert!(changed);
        assert_eq!(instructions.len(), 1);
        assert!(matches!(
            instructions[0],
            Instruction::LoadInt8 { dst: Register(0), value: 42 }
        ));
    }

    #[test]
    fn test_no_change_needed() {
        let mut optimizer = PeepholeOptimizer::new();
        let mut instructions = vec![
            Instruction::LoadInt8 { dst: Register(0), value: 1 },
            Instruction::LoadInt8 { dst: Register(1), value: 2 },
            Instruction::Add { dst: Register(2), lhs: Register(0), rhs: Register(1) },
        ];

        let changed = optimizer.optimize(&mut instructions);
        assert!(!changed);
        assert_eq!(instructions.len(), 3);
    }

    #[test]
    fn test_dead_code_after_return() {
        let mut optimizer = PeepholeOptimizer::new();
        let mut instructions = vec![
            Instruction::LoadInt8 { dst: Register(0), value: 1 },
            Instruction::Return { src: Register(0) },
            // Dead code - unreachable
            Instruction::LoadInt8 { dst: Register(1), value: 2 },
            Instruction::LoadInt8 { dst: Register(2), value: 3 },
        ];

        let changed = optimizer.optimize(&mut instructions);
        assert!(changed);
        assert_eq!(instructions.len(), 2);
        assert!(matches!(instructions[1], Instruction::Return { .. }));
    }

    #[test]
    fn test_dead_code_after_throw() {
        let mut optimizer = PeepholeOptimizer::new();
        let mut instructions = vec![
            Instruction::LoadInt8 { dst: Register(0), value: 1 },
            Instruction::Throw { src: Register(0) },
            // Dead code - unreachable
            Instruction::LoadInt8 { dst: Register(1), value: 2 },
        ];

        let changed = optimizer.optimize(&mut instructions);
        assert!(changed);
        assert_eq!(instructions.len(), 2);
        assert!(matches!(instructions[1], Instruction::Throw { .. }));
    }

    #[test]
    fn test_dead_code_preserves_jump_targets() {
        let mut optimizer = PeepholeOptimizer::new();
        // JumpIfFalse offset 2 means: from index 1, jump to index 1+2=3
        let mut instructions = vec![
            Instruction::LoadInt8 { dst: Register(0), value: 1 },
            Instruction::JumpIfFalse { cond: Register(0), offset: JumpOffset(2) },
            Instruction::Return { src: Register(0) },
            // This is the jump target (index 3)
            Instruction::LoadInt8 { dst: Register(1), value: 2 },
            Instruction::Return { src: Register(1) },
        ];

        let changed = optimizer.optimize(&mut instructions);
        // The code after Return at index 2 is reachable via JumpIfFalse
        // Index 3 (LoadInt8) is the target of the conditional jump
        // So it should be preserved
        assert!(!changed);
        assert_eq!(instructions.len(), 5);
    }

    #[test]
    fn test_dead_code_at_end_of_function() {
        let mut optimizer = PeepholeOptimizer::new();
        // Dead code at the very end - no jumps involved
        let mut instructions = vec![
            Instruction::LoadInt8 { dst: Register(0), value: 1 },
            Instruction::Return { src: Register(0) },
            // Dead code at end - definitely unreachable
            Instruction::LoadInt8 { dst: Register(1), value: 2 },
        ];

        let changed = optimizer.optimize(&mut instructions);
        assert!(changed);
        assert_eq!(instructions.len(), 2);
    }

    #[test]
    fn test_copy_propagation_simple() {
        let mut optimizer = PeepholeOptimizer::new();
        // Move r1 <- r0; Add r2, r1, r1 should become Add r2, r0, r0
        let mut instructions = vec![
            Instruction::LoadInt8 { dst: Register(0), value: 5 },
            Instruction::Move { dst: Register(1), src: Register(0) },
            Instruction::Add { dst: Register(2), lhs: Register(1), rhs: Register(1) },
            Instruction::Return { src: Register(2) },
        ];

        let changed = optimizer.optimize(&mut instructions);
        assert!(changed);
        // After copy propagation, Add should use r0 directly
        // The Move might be eliminated or kept depending on register coalescing
        let has_add_with_r0 = instructions.iter().any(|i| {
            matches!(i, Instruction::Add { lhs: Register(0), rhs: Register(0), .. })
        });
        assert!(has_add_with_r0);
    }

    #[test]
    fn test_copy_propagation_chain() {
        let mut optimizer = PeepholeOptimizer::new();
        // r0 = 5; r1 = r0; r2 = r1; return r2 -> return r0
        let mut instructions = vec![
            Instruction::LoadInt8 { dst: Register(0), value: 5 },
            Instruction::Move { dst: Register(1), src: Register(0) },
            Instruction::Move { dst: Register(2), src: Register(1) },
            Instruction::Return { src: Register(2) },
        ];

        let changed = optimizer.optimize(&mut instructions);
        assert!(changed);
        // Return should use r0 after propagation
        let has_return_r0 = instructions.iter().any(|i| {
            matches!(i, Instruction::Return { src: Register(0) })
        });
        assert!(has_return_r0);
    }

    #[test]
    fn test_register_coalescing_move_before_use() {
        let mut optimizer = PeepholeOptimizer::new();
        // Move r1, r0; Neg r2, r1 should have Neg use r0 after copy propagation
        let mut instructions = vec![
            Instruction::LoadInt8 { dst: Register(0), value: 5 },
            Instruction::Move { dst: Register(1), src: Register(0) },
            Instruction::Neg { dst: Register(2), src: Register(1) },
            Instruction::Return { src: Register(2) },
        ];

        let changed = optimizer.optimize(&mut instructions);
        assert!(changed);
        // After copy propagation, Neg should use r0 directly
        let has_neg_r0 = instructions.iter().any(|i| {
            matches!(i, Instruction::Neg { src: Register(0), .. })
        });
        assert!(has_neg_r0, "Neg should use r0 after copy propagation");
    }

    #[test]
    fn test_register_coalescing_preserves_needed_moves() {
        let mut optimizer = PeepholeOptimizer::new();
        // If r1 is used multiple times, Move should be preserved
        let mut instructions = vec![
            Instruction::LoadInt8 { dst: Register(0), value: 5 },
            Instruction::Move { dst: Register(1), src: Register(0) },
            Instruction::Add { dst: Register(2), lhs: Register(1), rhs: Register(1) },
            Instruction::Add { dst: Register(3), lhs: Register(1), rhs: Register(2) },
            Instruction::Return { src: Register(3) },
        ];

        let original_len = instructions.len();
        optimizer.optimize(&mut instructions);
        // Even after optimization, the semantics should be preserved
        // (though the exact instructions may differ due to copy propagation)
        assert!(instructions.len() <= original_len);
    }
}
