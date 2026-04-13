//! On-Stack Replacement (OSR) — tier-up during loop execution.
//!
//! OSR allows the JIT to compile and enter optimized code for a function
//! while it is already executing in the interpreter, at a loop header.
//!
//! ## Design (from V8/JSC/SpiderMonkey)
//!
//! - OSR entry only at **loop headers** (back-edge targets), not arbitrary PCs.
//!   This simplifies frame state mapping — only need Phi nodes at loop headers.
//! - Interrupt budget: single `i32` decremented on function entry (-15) and
//!   loop back-edges (-1). When ≤ 0, trigger tier-up check.
//! - Frame layout must be compatible between interpreter and JIT.
//!
//! ## Pipeline
//!
//! ```text
//! Interpreter loop back-edge
//!   → budget ≤ 0?
//!   → compile function with OSR entry at loop header
//!   → transfer register window to JIT frame
//!   → type guards on all live values
//!   → execute JIT code from loop header
//!   → (on deopt) reconstruct interpreter frame, resume
//! ```
//!
//! Spec: Phase 3 of JIT_INCREMENTAL_PLAN.md

use std::collections::HashMap;

// ============================================================
// Interrupt Budget
// ============================================================

/// Per-function interrupt budget for tier-up decisions.
///
/// V8 model: budget decremented on function entry (-15) and loop back-edges (-1).
/// When budget ≤ 0, the tier-up check fires.
#[derive(Debug, Clone)]
pub struct InterruptBudget {
    /// Current budget value. Starts positive, counts down.
    pub value: i32,
    /// Initial budget (for reset after tier-up).
    pub initial: i32,
}

impl InterruptBudget {
    /// Create a budget with the given initial value.
    #[must_use]
    pub fn new(initial: i32) -> Self {
        Self {
            value: initial,
            initial,
        }
    }

    /// Decrement budget for a function entry (V8: -15).
    /// Returns true if budget expired (tier-up should be considered).
    pub fn on_function_entry(&mut self) -> bool {
        self.value -= 15;
        self.value <= 0
    }

    /// Decrement budget for a loop back-edge (V8: -1).
    /// Returns true if budget expired.
    pub fn on_back_edge(&mut self) -> bool {
        self.value -= 1;
        self.value <= 0
    }

    /// Reset to initial value (after tier-up or recompilation).
    pub fn reset(&mut self) {
        self.value = self.initial;
    }

    /// Whether the budget is expired.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.value <= 0
    }
}

// ============================================================
// Loop Header Metadata
// ============================================================

/// Metadata about a loop header for OSR entry.
#[derive(Debug, Clone)]
pub struct LoopHeaderInfo {
    /// Bytecode PC of the loop header (back-edge target).
    pub header_pc: u32,
    /// Bytecode PC of the back-edge instruction (Jump with negative offset).
    pub backedge_pc: u32,
    /// Number of live local variables at the loop header.
    pub live_local_count: u16,
    /// Number of times this back-edge has been taken.
    pub backedge_count: u32,
}

/// Identifies loop headers in a function's bytecode.
///
/// A loop header is any bytecode PC that is the target of a backward jump
/// (i.e., a jump instruction with a negative offset landing at this PC).
pub fn find_loop_headers(bytecodes: &[otter_vm::bytecode::Instruction]) -> Vec<LoopHeaderInfo> {
    use otter_vm::bytecode::Opcode;

    let mut headers: HashMap<u32, LoopHeaderInfo> = HashMap::new();

    for (pc, instr) in bytecodes.iter().enumerate() {
        let pc = pc as u32;
        match instr.opcode() {
            Opcode::Jump | Opcode::JumpIfTrue | Opcode::JumpIfFalse => {
                let offset = instr.immediate_i32();
                if offset < 0 {
                    // Back-edge: target = pc + offset (wrapping add for signed).
                    let target = (pc as i32 + offset) as u32;
                    headers
                        .entry(target)
                        .and_modify(|h| h.backedge_count += 1)
                        .or_insert_with(|| LoopHeaderInfo {
                            header_pc: target,
                            backedge_pc: pc,
                            live_local_count: 0, // Filled in by liveness analysis.
                            backedge_count: 1,
                        });
                }
            }
            _ => {}
        }
    }

    let mut result: Vec<LoopHeaderInfo> = headers.into_values().collect();
    result.sort_by_key(|h| h.header_pc);
    result
}

// ============================================================
// OSR Entry Info (per compiled function)
// ============================================================

/// OSR entry point in a compiled function.
///
/// The JIT compiler emits a special entry block at the loop header.
/// On OSR, the interpreter's register window is transferred to the JIT frame,
/// and type guards validate all live values.
#[derive(Debug, Clone)]
pub struct OsrEntryPoint {
    /// Bytecode PC of the loop header.
    pub header_pc: u32,
    /// Expected types for live locals at entry (for guard insertion).
    /// Index = local index, value = expected type tag.
    pub expected_types: Vec<OsrValueType>,
    /// Offset into the compiled code for this OSR entry (in bytes).
    pub code_offset: u32,
}

/// Expected value type at an OSR entry point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsrValueType {
    /// Any tagged value (no specialization).
    Tagged,
    /// Expected to be Int32.
    Int32,
    /// Expected to be Float64.
    Float64,
    /// Expected to be an object.
    Object,
    /// Expected to be a boolean.
    Bool,
}

// ============================================================
// OSR State Manager
// ============================================================

/// Tier-up decision for a function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierUpAction {
    /// Stay in current tier (budget not expired or already at max tier).
    None,
    /// Compile to Tier 1 baseline JIT.
    CompileTier1,
    /// Compile to Tier 2 optimized JIT (function already has Tier 1).
    CompileTier2,
    /// Function has been permanently demoted (too many deopts).
    Blacklisted,
}

/// Per-function OSR state, managed by the runtime.
#[derive(Debug, Clone)]
pub struct OsrState {
    /// Interrupt budget for this function.
    pub budget: InterruptBudget,
    /// Current tier (0 = interpreter, 1 = baseline, 2 = optimized).
    pub current_tier: u8,
    /// Number of times this function has been compiled.
    pub compile_count: u32,
    /// Number of deopts since last compile.
    pub deopt_count: u32,
    /// Whether this function is blacklisted (permanently in interpreter).
    pub blacklisted: bool,
    /// Loop headers detected in this function.
    pub loop_headers: Vec<LoopHeaderInfo>,
    /// OSR entry points in the compiled code (if any).
    pub osr_entries: Vec<OsrEntryPoint>,
}

impl OsrState {
    /// Create initial OSR state for a function.
    #[must_use]
    pub fn new(initial_budget: i32) -> Self {
        Self {
            budget: InterruptBudget::new(initial_budget),
            current_tier: 0,
            compile_count: 0,
            deopt_count: 0,
            blacklisted: false,
            loop_headers: Vec::new(),
            osr_entries: Vec::new(),
        }
    }

    /// Check if tier-up should happen and return the appropriate action.
    pub fn check_tier_up(&mut self, max_deopts: u32, max_compiles: u32) -> TierUpAction {
        if self.blacklisted {
            return TierUpAction::Blacklisted;
        }
        if self.deopt_count >= max_deopts {
            self.blacklisted = true;
            return TierUpAction::Blacklisted;
        }
        if self.compile_count >= max_compiles {
            return TierUpAction::None; // Too many recompiles.
        }

        match self.current_tier {
            0 => TierUpAction::CompileTier1,
            1 => TierUpAction::CompileTier2,
            _ => TierUpAction::None, // Already at max tier.
        }
    }

    /// Record that compilation happened.
    pub fn record_compile(&mut self, tier: u8) {
        self.current_tier = tier;
        self.compile_count += 1;
        self.deopt_count = 0;
        // Reset budget with exponential backoff: base * 2^compile_count.
        let backoff = self
            .budget
            .initial
            .saturating_mul(1 << self.compile_count.min(4));
        self.budget = InterruptBudget::new(backoff);
    }

    /// Record a deoptimization.
    pub fn record_deopt(&mut self) {
        self.deopt_count += 1;
        // Demote to interpreter.
        self.current_tier = 0;
        self.osr_entries.clear();
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interrupt_budget_function_entry() {
        let mut budget = InterruptBudget::new(100);
        assert!(!budget.is_expired());

        // 6 function entries: 6 * 15 = 90, budget = 10.
        for _ in 0..6 {
            assert!(!budget.on_function_entry());
        }
        // 7th: budget = -5 → expired.
        assert!(budget.on_function_entry());
        assert!(budget.is_expired());
    }

    #[test]
    fn test_interrupt_budget_back_edges() {
        let mut budget = InterruptBudget::new(10);
        for _ in 0..9 {
            assert!(!budget.on_back_edge());
        }
        // 10th back-edge → expired.
        assert!(budget.on_back_edge());
    }

    #[test]
    fn test_interrupt_budget_reset() {
        let mut budget = InterruptBudget::new(100);
        budget.value = -5;
        assert!(budget.is_expired());
        budget.reset();
        assert!(!budget.is_expired());
        assert_eq!(budget.value, 100);
    }

    #[test]
    fn test_find_loop_headers() {
        use otter_vm::bytecode::{Instruction, JumpOffset};

        let bytecodes = vec![
            Instruction::nop(),                     // 0
            Instruction::nop(),                     // 1: loop header
            Instruction::nop(),                     // 2
            Instruction::jump(JumpOffset::new(-2)), // 3: back-edge to 1
            Instruction::nop(),                     // 4
        ];

        let headers = find_loop_headers(&bytecodes);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].header_pc, 1);
        assert_eq!(headers[0].backedge_pc, 3);
    }

    #[test]
    fn test_nested_loops() {
        use otter_vm::bytecode::{Instruction, JumpOffset};

        let bytecodes = vec![
            Instruction::nop(),                     // 0: outer header
            Instruction::nop(),                     // 1: inner header
            Instruction::nop(),                     // 2
            Instruction::jump(JumpOffset::new(-2)), // 3: inner back-edge → 1
            Instruction::jump(JumpOffset::new(-4)), // 4: outer back-edge → 0
        ];

        let headers = find_loop_headers(&bytecodes);
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0].header_pc, 0);
        assert_eq!(headers[1].header_pc, 1);
    }

    #[test]
    fn test_osr_state_tier_up() {
        let mut state = OsrState::new(100);
        assert_eq!(state.current_tier, 0);

        // Budget expired → should compile Tier 1.
        state.budget.value = 0;
        let action = state.check_tier_up(20, 5);
        assert_eq!(action, TierUpAction::CompileTier1);

        // Record compile.
        state.record_compile(1);
        assert_eq!(state.current_tier, 1);

        // Expire again → should compile Tier 2.
        state.budget.value = 0;
        let action = state.check_tier_up(20, 5);
        assert_eq!(action, TierUpAction::CompileTier2);
    }

    #[test]
    fn test_osr_state_blacklist() {
        let mut state = OsrState::new(100);
        state.deopt_count = 20;

        let action = state.check_tier_up(20, 5);
        assert_eq!(action, TierUpAction::Blacklisted);
        assert!(state.blacklisted);

        // Once blacklisted, stays blacklisted.
        let action = state.check_tier_up(20, 5);
        assert_eq!(action, TierUpAction::Blacklisted);
    }

    #[test]
    fn test_exponential_backoff() {
        let mut state = OsrState::new(100);

        state.record_compile(1); // compile_count = 1, budget = 100 * 2 = 200
        assert_eq!(state.budget.initial, 200);

        state.record_compile(2); // compile_count = 2, budget = 200 * 4 = 800
        assert_eq!(state.budget.initial, 800);
    }
}
