//! Typed SSA graph for the optimizing tier.
//!
//! A function in the optimizing subset is represented as a control-flow graph
//! of basic [`Block`]s over a flat arena of SSA [`Node`]s. Every node carries a
//! machine [`Repr`] (`Tagged` / `Int32` / `Float64` / `Bool`); arithmetic and
//! comparison nodes are produced in a typed form (`Int32Add`, `Float64Mul`,
//! `Int32Compare`, …) guarded by `Check*` speculation nodes inserted from the
//! interpreter's operand-type feedback. The graph is the input to the
//! representation-selection + lowering passes (next stage); on its own it
//! emits no code.
//!
//! # Contents
//! - [`Repr`] — machine representation lattice element.
//! - [`NodeKind`] / [`Node`] — typed SSA operations.
//! - [`Terminator`] — per-block control transfer.
//! - [`Block`] / [`Graph`] — the CFG and node arena.
//!
//! # Invariants
//! - **SSA.** Every node is assigned exactly once. Register-level mutability of
//!   the source bytecode is resolved to `Phi` nodes during construction
//!   ([`super::builder`]).
//! - **Register values are tagged at block boundaries.** A `Phi` always has
//!   [`Repr::Tagged`]; typed results (`Int32Add`, …) that flow into a phi are
//!   boxed at the predecessor edge by the lowering pass. Typed islands are
//!   bounded by `Check*` (unbox on read) and box-on-cross-block.
//! - **Speculation is explicit.** A typed arithmetic node only ever consumes
//!   the result of a matching `Check*` (or another node already in that repr);
//!   a failed `Check*` deoptimizes to the interpreter (deopt is a later stage —
//!   until then the graph is built but not executed).
//!
//! # See also
//! - [`super::builder`] — bytecode → SSA construction.
//! - [`crate::optimizing`] — tier entry and `Unsupported` reasons.

use otter_bytecode::Op;

/// Dense index into [`Graph::nodes`].
pub type NodeId = u32;
/// Dense index into [`Graph::blocks`].
pub type BlockId = u32;

/// Machine representation an SSA value is materialized in.
///
/// `Tagged` is the NaN-boxed `Value` every register slot holds at a block
/// boundary; `Int32` is an unboxed numeric island; `Bool` is an unboxed branch
/// predicate produced by a comparison. (`Float64` joins the lattice with the
/// float subset in a later step.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Repr {
    /// NaN-boxed `Value` (the universal register representation).
    Tagged,
    /// Unboxed 32-bit signed integer.
    Int32,
    /// Unboxed branch predicate.
    Bool,
}

/// Relational / equality comparison kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmpOp {
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `==` / `===` (numeric operands only in this tier)
    Eq,
    /// `!=` / `!==`
    Ne,
}

impl CmpOp {
    /// Map a relational / equality bytecode opcode to its [`CmpOp`], or `None`
    /// for a non-comparison opcode.
    #[must_use]
    pub fn from_op(op: Op) -> Option<Self> {
        Some(match op {
            Op::LessThan => Self::Lt,
            Op::LessEq => Self::Le,
            Op::GreaterThan => Self::Gt,
            Op::GreaterEq => Self::Ge,
            Op::Equal => Self::Eq,
            Op::NotEqual => Self::Ne,
            _ => return None,
        })
    }
}

/// A typed SSA operation.
#[derive(Clone, Debug)]
pub enum NodeKind {
    /// Initial value of source register `n` on function entry (a parameter, or
    /// `undefined` for an uninitialized local — see the builder).
    Param(u16),
    /// Unboxed `int32` literal.
    ConstInt32(i32),
    /// Boxed boolean literal.
    ConstBool(bool),
    /// Boxed `undefined`.
    ConstUndefined,
    /// SSA phi. Operands are aligned 1:1 with the owning block's `preds`. May be
    /// temporarily empty (incomplete) during construction of an unsealed block.
    Phi(Vec<NodeId>),
    /// Speculative "operand is int32" guard; result repr [`Repr::Int32`]. A
    /// non-int32 input deoptimizes.
    CheckInt32(NodeId),
    /// `int32 + int32`, deopt on overflow. Result [`Repr::Int32`].
    Int32Add(NodeId, NodeId),
    /// `int32 - int32`, deopt on overflow.
    Int32Sub(NodeId, NodeId),
    /// `int32 * int32`, deopt on overflow.
    Int32Mul(NodeId, NodeId),
    /// `int32 <cmp> int32`. Result [`Repr::Bool`].
    Int32Compare(CmpOp, NodeId, NodeId),
}

impl NodeKind {
    /// The representation a node of this kind produces.
    #[must_use]
    pub fn repr(&self) -> Repr {
        match self {
            NodeKind::ConstInt32(_)
            | NodeKind::CheckInt32(_)
            | NodeKind::Int32Add(_, _)
            | NodeKind::Int32Sub(_, _)
            | NodeKind::Int32Mul(_, _) => Repr::Int32,
            NodeKind::Int32Compare(_, _, _) => Repr::Bool,
            // Register-carried values are tagged at block boundaries; a phi
            // therefore lives in tagged form (lowering boxes typed inputs).
            NodeKind::Param(_)
            | NodeKind::ConstBool(_)
            | NodeKind::ConstUndefined
            | NodeKind::Phi(_) => Repr::Tagged,
        }
    }
}

/// One SSA node: its operation, cached representation, and owning block.
#[derive(Clone, Debug)]
pub struct Node {
    /// The operation and its operand node ids.
    pub kind: NodeKind,
    /// Representation this node's value is produced in (cached from
    /// [`NodeKind::repr`]).
    pub repr: Repr,
    /// Block this node belongs to.
    pub block: BlockId,
}

/// Per-block control transfer.
#[derive(Clone, Debug)]
pub enum Terminator {
    /// Return the value of `NodeId` from the function.
    Return(NodeId),
    /// Unconditional branch to a block.
    Jump(BlockId),
    /// Two-way branch on a boolean predicate node. `on_true` is taken when the
    /// predicate is true.
    Branch {
        /// Boolean predicate node.
        cond: NodeId,
        /// Target when the predicate is true.
        on_true: BlockId,
        /// Target when the predicate is false.
        on_false: BlockId,
    },
}

/// A basic block: a maximal straight-line instruction range plus its
/// predecessors, phis, body nodes, and terminator.
#[derive(Clone, Debug)]
pub struct Block {
    /// Byte-PC of the block's first bytecode instruction (its label).
    pub start_pc: u32,
    /// Predecessor blocks, in a fixed order phi operands are aligned to.
    pub preds: Vec<BlockId>,
    /// Phi node ids defined at the head of this block.
    pub phis: Vec<NodeId>,
    /// Straight-line body node ids in evaluation order (phis excluded).
    pub body: Vec<NodeId>,
    /// Control transfer leaving the block; `None` only mid-construction.
    pub term: Option<Terminator>,
    /// `true` once every predecessor edge is known and filled, so phi operands
    /// can be finalized (Braun et al. sealing).
    pub sealed: bool,
    /// `true` once the block's instructions have all been translated.
    pub filled: bool,
}

impl Block {
    pub(super) fn new(start_pc: u32) -> Self {
        Self {
            start_pc,
            preds: Vec::new(),
            phis: Vec::new(),
            body: Vec::new(),
            term: None,
            sealed: false,
            filled: false,
        }
    }
}

/// A typed SSA control-flow graph for one function.
#[derive(Clone, Debug)]
pub struct Graph {
    /// Node arena, indexed by [`NodeId`].
    pub nodes: Vec<Node>,
    /// Block arena, indexed by [`BlockId`].
    pub blocks: Vec<Block>,
    /// Entry block id (always `0`).
    pub entry: BlockId,
    /// Source function parameter count.
    pub param_count: u16,
    /// Source register-window length.
    pub register_count: u16,
}

impl Graph {
    /// Construct an empty graph with a single (entry) block at byte-PC 0.
    pub(super) fn new(param_count: u16, register_count: u16) -> Self {
        Self {
            nodes: Vec::new(),
            blocks: vec![Block::new(0)],
            entry: 0,
            param_count,
            register_count,
        }
    }

    /// Append a node in `block`, returning its id. The repr is derived from the
    /// kind.
    pub(super) fn add_node(&mut self, kind: NodeKind, block: BlockId) -> NodeId {
        let repr = kind.repr();
        let id = self.nodes.len() as NodeId;
        self.nodes.push(Node { kind, repr, block });
        id
    }

    /// Borrow a node by id.
    #[must_use]
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    /// Borrow a block by id.
    #[must_use]
    pub fn block(&self, id: BlockId) -> &Block {
        &self.blocks[id as usize]
    }
}
