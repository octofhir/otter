//! Typed SSA graph for the optimizing tier.
//!
//! A function in the optimizing subset is represented as a control-flow graph
//! of basic [`Block`]s over a flat arena of SSA [`Node`]s. Every node carries a
//! machine [`Repr`] (`Tagged` / `Int32` / `Float64` / `Bool`); arithmetic and
//! comparison nodes are produced in a typed form (`Int32Add`, `Float64Mul`,
//! `Int32Compare`, …) guarded by `Check*` speculation nodes inserted from the
//! interpreter's operand-type feedback. The graph is the input to SSA liveness,
//! linear-scan register allocation, deopt frame-state capture, and arm64
//! lowering; on its own it emits no code.
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
//!   a failed `Check*` deoptimizes to the interpreter at the guard's exact PC.
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
/// boundary; `Int32` and `Float64` are unboxed numeric islands; `Bool` is an
/// unboxed branch predicate produced by a comparison.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Repr {
    /// NaN-boxed `Value` (the universal register representation).
    Tagged,
    /// Unboxed 32-bit signed integer (low 32 bits of a GP register).
    Int32,
    /// Unboxed IEEE-754 `f64` (an FP register / spill slot). A boxed double is
    /// its bits verbatim, so boxing a `Float64` is the identity bit-pattern move
    /// from the FP home into a GP register.
    Float64,
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
    /// Unboxed `f64` literal (a `LoadNumber` constant). Result [`Repr::Float64`].
    ConstF64(f64),
    /// Boxed boolean literal.
    ConstBool(bool),
    /// Boxed `undefined`.
    ConstUndefined,
    /// The running function's own closure (`MakeFunction` self-binding), read
    /// from the entry context. A tagged value never used as a numeric operand in
    /// this subset (a use that needs it as a number deoptimizes); present so the
    /// near-universal leading self-binding does not disqualify a function.
    SelfClosure,
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
    /// Speculative "operand is a number" guard; result repr [`Repr::Float64`]
    /// (the operand unboxed to an `f64`). An int32-tagged input is widened to a
    /// double; a real double is unboxed verbatim; a non-number input
    /// deoptimizes.
    CheckNumber(NodeId),
    /// Widen an unboxed `int32` to `f64` (arm64 `scvtf`). Used to bring an
    /// int32-typed operand into a `Float64` arithmetic site without a guard.
    Int32ToFloat64(NodeId),
    /// `f64 + f64` (no overflow — IEEE arithmetic is total). Result
    /// [`Repr::Float64`].
    Float64Add(NodeId, NodeId),
    /// `f64 - f64`.
    Float64Sub(NodeId, NodeId),
    /// `f64 * f64`.
    Float64Mul(NodeId, NodeId),
    /// `f64 / f64`.
    Float64Div(NodeId, NodeId),
    /// `f64 <cmp> f64`. Result [`Repr::Bool`]. IEEE ordered comparison
    /// (a `NaN` operand yields `false` for every relation, matching JS).
    Float64Compare(CmpOp, NodeId, NodeId),
    /// Speculative "operand is an ordinary object of the baked shape" guard.
    /// Carries the receiver and the receiver shape's compressed `Gc` offset. A
    /// non-object, or a different shape (or dictionary mode), deoptimizes.
    /// Result [`Repr::Tagged`] — the guarded receiver, passed through so a
    /// `LoadSlot` consumes it.
    CheckShape(NodeId, u32),
    /// Load the `Value` at a fixed byte offset within a shape-guarded receiver's
    /// value slab (the offset baked from the monomorphic own-data property IC).
    /// Pure (no deopt, no allocation); the preceding `CheckShape` established the
    /// receiver shape. Result [`Repr::Tagged`].
    LoadSlot(NodeId, u32),
    /// Store a value into a fixed byte offset within a shape-guarded receiver's
    /// value slab. Inputs `(receiver, value)`; the value is always a primitive
    /// (int32 / f64 / bool), so the stored `Value` is never a `Gc` pointer and no
    /// generational write barrier is needed. A side effect — produces no result.
    StoreSlot(NodeId, u32, NodeId),
}

impl NodeKind {
    /// SSA value operands this node consumes, in operand order. For a `Phi`
    /// these are the per-predecessor inputs (aligned to the block's `preds`),
    /// which liveness treats as uses at the predecessor edges rather than as
    /// block-local uses.
    #[must_use]
    pub fn inputs(&self) -> Vec<NodeId> {
        match self {
            NodeKind::CheckInt32(a)
            | NodeKind::CheckNumber(a)
            | NodeKind::Int32ToFloat64(a)
            | NodeKind::CheckShape(a, _)
            | NodeKind::LoadSlot(a, _) => {
                vec![*a]
            }
            NodeKind::Int32Add(a, b)
            | NodeKind::Int32Sub(a, b)
            | NodeKind::Int32Mul(a, b)
            | NodeKind::Int32Compare(_, a, b)
            | NodeKind::Float64Add(a, b)
            | NodeKind::Float64Sub(a, b)
            | NodeKind::Float64Mul(a, b)
            | NodeKind::Float64Div(a, b)
            | NodeKind::Float64Compare(_, a, b)
            | NodeKind::StoreSlot(a, _, b) => vec![*a, *b],
            NodeKind::Phi(ops) => ops.clone(),
            NodeKind::Param(_)
            | NodeKind::ConstInt32(_)
            | NodeKind::ConstF64(_)
            | NodeKind::ConstBool(_)
            | NodeKind::ConstUndefined
            | NodeKind::SelfClosure => Vec::new(),
        }
    }

    /// Rewrite every operand equal to `old` to `new`. Used by trivial-phi
    /// elimination to redirect all uses of a removed phi to its single distinct
    /// input.
    pub fn replace_input(&mut self, old: NodeId, new: NodeId) {
        let fix = |x: &mut NodeId| {
            if *x == old {
                *x = new;
            }
        };
        match self {
            NodeKind::CheckInt32(a)
            | NodeKind::CheckNumber(a)
            | NodeKind::Int32ToFloat64(a)
            | NodeKind::CheckShape(a, _)
            | NodeKind::LoadSlot(a, _) => fix(a),
            NodeKind::Int32Add(a, b)
            | NodeKind::Int32Sub(a, b)
            | NodeKind::Int32Mul(a, b)
            | NodeKind::Int32Compare(_, a, b)
            | NodeKind::Float64Add(a, b)
            | NodeKind::Float64Sub(a, b)
            | NodeKind::Float64Mul(a, b)
            | NodeKind::Float64Div(a, b)
            | NodeKind::Float64Compare(_, a, b)
            | NodeKind::StoreSlot(a, _, b) => {
                fix(a);
                fix(b);
            }
            NodeKind::Phi(ops) => ops.iter_mut().for_each(fix),
            NodeKind::Param(_)
            | NodeKind::ConstInt32(_)
            | NodeKind::ConstF64(_)
            | NodeKind::ConstBool(_)
            | NodeKind::ConstUndefined
            | NodeKind::SelfClosure => {}
        }
    }

    /// The representation a node of this kind produces.
    #[must_use]
    pub fn repr(&self) -> Repr {
        match self {
            NodeKind::ConstInt32(_)
            | NodeKind::CheckInt32(_)
            | NodeKind::Int32Add(_, _)
            | NodeKind::Int32Sub(_, _)
            | NodeKind::Int32Mul(_, _) => Repr::Int32,
            NodeKind::ConstF64(_)
            | NodeKind::CheckNumber(_)
            | NodeKind::Int32ToFloat64(_)
            | NodeKind::Float64Add(_, _)
            | NodeKind::Float64Sub(_, _)
            | NodeKind::Float64Mul(_, _)
            | NodeKind::Float64Div(_, _) => Repr::Float64,
            NodeKind::Int32Compare(_, _, _) | NodeKind::Float64Compare(_, _, _) => Repr::Bool,
            // Register-carried values are tagged at block boundaries; a phi
            // therefore lives in tagged form (lowering boxes typed inputs).
            NodeKind::Param(_)
            | NodeKind::ConstBool(_)
            | NodeKind::ConstUndefined
            | NodeKind::SelfClosure
            | NodeKind::Phi(_)
            | NodeKind::CheckShape(_, _)
            | NodeKind::LoadSlot(_, _)
            | NodeKind::StoreSlot(_, _, _) => Repr::Tagged,
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
    /// Byte-PC of the bytecode instruction this node serves. A node that can
    /// deoptimize (a `Check*` guard, an overflowing `Int32*`) stamps this so the
    /// interpreter resumes at the exact instruction. Synthetic entry defs use
    /// `0`.
    pub byte_pc: u32,
    /// Bytecode register this node's value is written through to, for deopt
    /// coherence: a freshly computed value that is the result of a
    /// register-writing instruction is boxed and stored to this frame slot so a
    /// later bail sees a current frame. `None` for temps (e.g. `Check*`) and
    /// values already resident in their frame slot (`Param`).
    pub frame_dst: Option<u16>,
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
    /// The bytecode register each `Phi` node merges, recorded at construction.
    /// Used by deopt frame-state reconstruction to know which register a header
    /// phi defines on block entry. Entries for trivially-eliminated phis become
    /// stale but are never read (only live `Block::phis` are consulted).
    pub phi_reg: rustc_hash::FxHashMap<NodeId, u16>,
    /// Per-block ordered log of bytecode-register definitions performed while
    /// translating the block's instructions: `(byte_pc, register, value)` in
    /// execution order. Unlike [`Node::frame_dst`] (which records only a value's
    /// *primary* destination), this captures *every* register rebind — including
    /// `LoadLocal` / `StoreLocal` aliasing that binds a register to an existing
    /// SSA value without producing a node. Deopt frame-state reconstruction
    /// replays it to know precisely which SSA value each interpreter register
    /// holds at a guard, which `frame_dst` alone cannot express for aliased
    /// registers.
    pub reg_writes: rustc_hash::FxHashMap<BlockId, Vec<(u32, u16, NodeId)>>,
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
            phi_reg: rustc_hash::FxHashMap::default(),
            reg_writes: rustc_hash::FxHashMap::default(),
        }
    }

    /// Append a node in `block` originating at `byte_pc`, returning its id. The
    /// repr is derived from the kind; `frame_dst` starts `None` (set by
    /// [`Self::set_frame_dst`] for register-writing instructions).
    pub(super) fn add_node(&mut self, kind: NodeKind, block: BlockId, byte_pc: u32) -> NodeId {
        let repr = kind.repr();
        let id = self.nodes.len() as NodeId;
        self.nodes.push(Node {
            kind,
            repr,
            block,
            byte_pc,
            frame_dst: None,
        });
        id
    }

    /// Mark `node` as written through to bytecode register `reg`.
    pub(super) fn set_frame_dst(&mut self, node: NodeId, reg: u16) {
        self.nodes[node as usize].frame_dst = Some(reg);
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
