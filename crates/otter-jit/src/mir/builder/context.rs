use crate::feedback::FeedbackSnapshot;
use crate::mir::graph::{BlockId, DeoptId, DeoptInfo, MirGraph, ResumeMode, ValueId};
use crate::mir::nodes::MirOp;

/// Mutable lowering state shared across MIR builder submodules.
pub(super) struct BuilderContext<'a> {
    pub(super) graph: MirGraph,
    pub(super) feedback: &'a FeedbackSnapshot,
    /// Cache for scratch register values: Register(i) -> current MIR ValueId.
    /// Indexed by scratch register index (0..N), NOT by local index.
    scratch_map: Vec<Option<ValueId>>,
}

impl<'a> BuilderContext<'a> {
    pub(super) fn new(
        name: String,
        local_count: u16,
        register_count: u16,
        param_count: u16,
        feedback: &'a FeedbackSnapshot,
    ) -> Self {
        let scratch_count = register_count.saturating_sub(local_count) as usize;
        Self {
            graph: MirGraph::new(name, local_count, register_count, param_count),
            feedback,
            scratch_map: vec![None; scratch_count.max(16)],
        }
    }

    /// Get the MIR value for a scratch register. Emits LoadRegister if not cached.
    pub(super) fn get_scratch(&mut self, block: BlockId, reg: u16, pc: u32) -> ValueId {
        let idx = reg as usize;
        if idx < self.scratch_map.len()
            && let Some(val) = self.scratch_map[idx]
        {
            return val;
        }
        let val = self.graph.push_instr(block, MirOp::LoadRegister(reg), pc);
        if idx < self.scratch_map.len() {
            self.scratch_map[idx] = Some(val);
        }
        val
    }

    /// Set the MIR value for a scratch register. Emits StoreRegister and updates cache.
    pub(super) fn set_scratch(&mut self, block: BlockId, reg: u16, val: ValueId, pc: u32) {
        let idx = reg as usize;
        if idx < self.scratch_map.len() {
            self.scratch_map[idx] = Some(val);
        }
        self.graph
            .push_instr(block, MirOp::StoreRegister { idx: reg, val }, pc);
    }

    /// Load from a local variable slot. Always emits LoadLocal.
    pub(super) fn load_local(&mut self, block: BlockId, idx: u16, pc: u32) -> ValueId {
        self.graph.push_instr(block, MirOp::LoadLocal(idx), pc)
    }

    /// Store to a local variable slot.
    pub(super) fn store_local(&mut self, block: BlockId, idx: u16, val: ValueId, pc: u32) {
        self.graph
            .push_instr(block, MirOp::StoreLocal { idx, val }, pc);
    }

    /// Invalidate the scratch register cache (at block boundaries).
    pub(super) fn invalidate_scratch_cache(&mut self) {
        for slot in &mut self.scratch_map {
            *slot = None;
        }
    }

    /// Create a deopt point.
    pub(super) fn make_deopt(&mut self, pc: u32) -> DeoptId {
        self.graph.create_deopt(DeoptInfo {
            bytecode_pc: pc,
            live_state: Vec::new(),
            resume_mode: ResumeMode::ResumeAtPc,
        })
    }
}
