//! SSA graph representation for MIR.
//!
//! The graph consists of basic blocks connected by edges. Each block contains
//! a sequence of MIR instructions. Values are identified by `ValueId` and
//! follow SSA: each value is defined exactly once.

use super::nodes::MirOp;
use super::types::MirType;

/// Identifies a value in the MIR graph. Values are defined by instructions
/// and referenced as operands by subsequent instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ValueId(pub u32);

/// Identifies a basic block in the MIR graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u32);

/// Identifies a deopt point. Maps to a `DeoptInfo` in the deopt table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeoptId(pub u32);

impl std::fmt::Display for ValueId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "v{}", self.0)
    }
}

impl std::fmt::Display for BlockId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "bb{}", self.0)
    }
}

impl std::fmt::Display for DeoptId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "deopt{}", self.0)
    }
}

/// A single instruction in the MIR graph: an operation that produces a value.
#[derive(Debug, Clone)]
pub struct MirInstr {
    /// The value this instruction defines.
    pub value: ValueId,
    /// The operation.
    pub op: MirOp,
    /// The type of the produced value.
    pub ty: MirType,
    /// Source bytecode PC (for diagnostics and deopt mapping).
    pub bytecode_pc: u32,
}

/// A basic block: a straight-line sequence of instructions ending with
/// a terminator (Jump, Branch, Return, Deopt, Throw).
#[derive(Debug, Clone)]
pub struct BasicBlock {
    /// Block identifier.
    pub id: BlockId,
    /// Instructions in order.
    pub instrs: Vec<MirInstr>,
    /// Predecessor block IDs (populated during graph construction).
    pub predecessors: Vec<BlockId>,
    /// Successor block IDs (derived from the terminator).
    pub successors: Vec<BlockId>,
    /// Bytecode PC range this block covers (start, end-exclusive).
    pub pc_range: (u32, u32),
}

impl BasicBlock {
    /// Create a new empty basic block.
    pub fn new(id: BlockId) -> Self {
        Self {
            id,
            instrs: Vec::new(),
            predecessors: Vec::new(),
            successors: Vec::new(),
            pc_range: (0, 0),
        }
    }

    /// The terminator instruction, if any.
    pub fn terminator(&self) -> Option<&MirInstr> {
        self.instrs.last().filter(|i| i.op.is_terminator())
    }

    /// Whether this block has a terminator.
    pub fn is_terminated(&self) -> bool {
        self.terminator().is_some()
    }
}

/// How to resume in the interpreter after a deopt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeMode {
    /// Resume at the specified bytecode PC.
    ResumeAtPc,
    /// Restart the function from PC 0.
    Restart,
}

/// Which kind of slot a deopt live value refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    Local,
    Register,
}

/// A live value that must be materialized on deopt.
#[derive(Debug, Clone)]
pub struct DeoptLiveValue {
    pub kind: SlotKind,
    pub index: u16,
    pub value: ValueId,
}

/// Deopt metadata for a single deopt point.
#[derive(Debug, Clone)]
pub struct DeoptInfo {
    /// Bytecode PC to resume at.
    pub bytecode_pc: u32,
    /// Live values that must be materialized in the interpreter frame.
    pub live_state: Vec<DeoptLiveValue>,
    /// How to resume.
    pub resume_mode: ResumeMode,
}

/// The complete MIR graph for a single function.
#[derive(Debug)]
pub struct MirGraph {
    /// All basic blocks, indexed by BlockId.
    pub blocks: Vec<BasicBlock>,
    /// All deopt points, indexed by DeoptId.
    pub deopts: Vec<DeoptInfo>,
    /// Next value ID to allocate.
    next_value: u32,
    /// The entry block.
    pub entry_block: BlockId,
    /// Function name (for diagnostics).
    pub function_name: String,
    /// Number of local variables.
    pub local_count: u16,
    /// Number of scratch registers.
    pub register_count: u16,
    /// Number of parameters.
    pub param_count: u16,
}

impl MirGraph {
    /// Create a new empty MIR graph.
    pub fn new(function_name: String, local_count: u16, register_count: u16, param_count: u16) -> Self {
        let entry = BasicBlock::new(BlockId(0));
        Self {
            blocks: vec![entry],
            deopts: Vec::new(),
            next_value: 0,
            entry_block: BlockId(0),
            function_name,
            local_count,
            register_count,
            param_count,
        }
    }

    /// Allocate a fresh ValueId.
    pub fn next_value(&mut self) -> ValueId {
        let id = ValueId(self.next_value);
        self.next_value += 1;
        id
    }

    /// Create a new basic block and return its ID.
    pub fn create_block(&mut self) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(BasicBlock::new(id));
        id
    }

    /// Create a new deopt point.
    pub fn create_deopt(&mut self, info: DeoptInfo) -> DeoptId {
        let id = DeoptId(self.deopts.len() as u32);
        self.deopts.push(info);
        id
    }

    /// Add an instruction to a block and return the value it defines.
    pub fn push_instr(
        &mut self,
        block: BlockId,
        op: MirOp,
        bytecode_pc: u32,
    ) -> ValueId {
        let ty = op.result_type();
        let value = self.next_value();
        self.blocks[block.0 as usize].instrs.push(MirInstr {
            value,
            op,
            ty,
            bytecode_pc,
        });
        value
    }

    /// Add an instruction with an explicit type override (for Phi nodes, Move).
    pub fn push_instr_typed(
        &mut self,
        block: BlockId,
        op: MirOp,
        ty: MirType,
        bytecode_pc: u32,
    ) -> ValueId {
        let value = self.next_value();
        self.blocks[block.0 as usize].instrs.push(MirInstr {
            value,
            op,
            ty,
            bytecode_pc,
        });
        value
    }

    /// Get a block by ID.
    pub fn block(&self, id: BlockId) -> &BasicBlock {
        &self.blocks[id.0 as usize]
    }

    /// Get a mutable block by ID.
    pub fn block_mut(&mut self, id: BlockId) -> &mut BasicBlock {
        &mut self.blocks[id.0 as usize]
    }

    /// Look up the instruction that defines a value.
    pub fn value_def(&self, val: ValueId) -> Option<&MirInstr> {
        for block in &self.blocks {
            for instr in &block.instrs {
                if instr.value == val {
                    return Some(instr);
                }
            }
        }
        None
    }

    /// Get the type of a value.
    pub fn value_type(&self, val: ValueId) -> MirType {
        self.value_def(val).map(|i| i.ty).unwrap_or(MirType::Boxed)
    }

    /// Total number of values allocated.
    pub fn value_count(&self) -> u32 {
        self.next_value
    }

    /// Total number of blocks.
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Compute predecessor/successor edges from terminators.
    /// Call this after the graph is fully built.
    pub fn recompute_edges(&mut self) {
        // Clear existing edges.
        for block in &mut self.blocks {
            block.predecessors.clear();
            block.successors.clear();
        }

        // Collect successor edges from terminators.
        let mut edges: Vec<(BlockId, BlockId)> = Vec::new();
        for block in &self.blocks {
            let block_id = block.id;
            if let Some(term) = block.terminator() {
                match &term.op {
                    MirOp::Jump(target) => {
                        edges.push((block_id, *target));
                    }
                    MirOp::Branch {
                        true_block,
                        false_block,
                        ..
                    } => {
                        edges.push((block_id, *true_block));
                        edges.push((block_id, *false_block));
                    }
                    MirOp::TryStart { catch_block } => {
                        edges.push((block_id, *catch_block));
                    }
                    _ => {} // Return, Deopt, Throw have no successors
                }
            }
        }

        // Apply edges.
        for (from, to) in edges {
            self.blocks[from.0 as usize].successors.push(to);
            self.blocks[to.0 as usize].predecessors.push(from);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_graph() {
        let mut graph = MirGraph::new("test".into(), 3, 5, 2);
        assert_eq!(graph.block_count(), 1); // entry block
        assert_eq!(graph.entry_block, BlockId(0));

        let bb1 = graph.create_block();
        assert_eq!(bb1, BlockId(1));
        assert_eq!(graph.block_count(), 2);
    }

    #[test]
    fn test_push_instruction() {
        let mut graph = MirGraph::new("test".into(), 2, 4, 1);
        let entry = graph.entry_block;

        let v0 = graph.push_instr(entry, MirOp::ConstInt32(42), 0);
        assert_eq!(v0, ValueId(0));
        assert_eq!(graph.value_type(v0), MirType::Int32);

        let v1 = graph.push_instr(entry, MirOp::Undefined, 1);
        assert_eq!(v1, ValueId(1));
        assert_eq!(graph.value_type(v1), MirType::Boxed);

        assert_eq!(graph.block(entry).instrs.len(), 2);
    }

    #[test]
    fn test_deopt_info() {
        let mut graph = MirGraph::new("test".into(), 2, 4, 1);
        let deopt = graph.create_deopt(DeoptInfo {
            bytecode_pc: 42,
            live_state: vec![DeoptLiveValue {
                kind: SlotKind::Local,
                index: 0,
                value: ValueId(0),
            }],
            resume_mode: ResumeMode::ResumeAtPc,
        });
        assert_eq!(deopt, DeoptId(0));
        assert_eq!(graph.deopts[0].bytecode_pc, 42);
    }

    #[test]
    fn test_recompute_edges() {
        let mut graph = MirGraph::new("test".into(), 0, 2, 0);
        let entry = graph.entry_block;
        let bb1 = graph.create_block();
        let bb2 = graph.create_block();

        // entry -> branch to bb1 or bb2
        let cond = graph.push_instr(entry, MirOp::True, 0);
        graph.push_instr(
            entry,
            MirOp::Branch {
                cond,
                true_block: bb1,
                false_block: bb2,
            },
            1,
        );

        // bb1 -> jump to bb2
        graph.push_instr(bb1, MirOp::Jump(bb2), 2);

        // bb2 -> return
        let undef = graph.push_instr(bb2, MirOp::Undefined, 3);
        graph.push_instr(bb2, MirOp::Return(undef), 4);

        graph.recompute_edges();

        assert_eq!(graph.block(entry).successors, vec![bb1, bb2]);
        assert_eq!(graph.block(bb1).predecessors, vec![entry]);
        assert_eq!(graph.block(bb1).successors, vec![bb2]);
        assert_eq!(graph.block(bb2).predecessors, vec![entry, bb1]);
        assert!(graph.block(bb2).successors.is_empty());
    }
}
