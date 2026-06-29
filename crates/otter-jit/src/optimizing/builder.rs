//! Bytecode → typed SSA construction for the optimizing tier.
//!
//! Two passes over a [`JitFunctionView`]:
//!
//! 1. **CFG discovery** ([`Cfg::discover`]) finds basic-block leaders (the first
//!    instruction, every branch target, and every instruction following a
//!    branch / return), slices the instruction stream into blocks, and records
//!    successor / predecessor edges.
//! 2. **SSA construction** ([`Builder::run`]) translates each block's
//!    instructions into typed nodes, resolving the register machine's mutable
//!    slots into `Phi` nodes on demand using the Braun et al. algorithm
//!    ("Simple and Efficient Construction of SSA Form") — read/write of a
//!    register variable per block, incomplete phis in unsealed blocks, and
//!    sealing once all predecessor edges are filled. This handles loops (a back
//!    edge fills its header's incomplete phis when the loop body is sealed)
//!    without computing dominance.
//!
//! Arithmetic / comparison sites consult the interpreter's per-site operand
//! feedback ([`ArithFeedback`], baked into [`JitInstrView::arith_feedback`]): an
//! int32-only site lowers to an unboxed `Int32*` node guarded by `CheckInt32`; a
//! site that has seen doubles (but only numbers) lowers to a `Float64*` node
//! whose operands are widened / number-checked to `f64`. Any opcode outside the
//! numeric subset — or a site that has seen a string / bigint / object — aborts
//! the whole-function compile with [`Unsupported`], and the VM keeps running the
//! baseline / interpreter.
//!
//! # See also
//! - [`super::ir`] — the graph the builder produces.

use std::collections::{BTreeMap, BTreeSet};

use otter_bytecode::{Op, Operand};
use otter_vm::jit_feedback::ArithFeedback;
use otter_vm::{JitArrayMethodKind, JitFunctionView, JitInlineCallee, JitInlineMethod};
use rustc_hash::{FxHashMap, FxHashSet};

use super::Unsupported;
use super::ir::{BlockId, CmpOp, Float64UnaryOp, Graph, NodeId, NodeKind, Repr, Terminator};

/// Build a typed SSA graph for `view`, or report why the function is outside the
/// optimizing subset. When `osr_pc` is set, build only the region reachable from
/// that loop header, with a synthetic entry edge feeding the header phis.
pub(super) fn build(view: &JitFunctionView, osr_pc: Option<u32>) -> Result<Graph, Unsupported> {
    if view.instructions.is_empty() {
        return Err(Unsupported::Empty);
    }
    let cfg = Cfg::discover(view)?;
    let mut builder = Builder::new(view, cfg, osr_pc)?;
    builder.run()?;
    reject_call_object_mix(&builder.graph)?;
    Ok(builder.graph)
}

/// Decline a graph that mixes a remaining runtime call with a runtime
/// object/array memory op the optimizing tier can only lower through a
/// materialize-frame stub.
///
/// The optimizing tier wins on arithmetic-dominated loops whose callees were
/// spliced inline; it loses to the baseline on loops dominated by un-inlined
/// runtime calls, because every [`NodeKind::Call`] / [`NodeKind::CallMethod`]
/// site must spill and reload the whole frame around the bridge. When such a
/// call coexists with a runtime property/element op (an un-baked store slot,
/// element access, array-length read, or an un-inlined method call) the
/// compiled body churns those stubs on the hot path and runs slower than the
/// baseline's tuned IC sequence — so we keep the baseline.
///
/// A bare [`NodeKind::CheckShape`] does **not** count: it is the cheap inline
/// identity guard an inlined method leaves behind (its property loads are
/// sealed `LoadSlot`s at baked offsets), not a runtime stub. Gating on it would
/// reject exactly the fully-inlined method bodies this tier exists to optimize.
fn reject_call_object_mix(graph: &Graph) -> Result<(), Unsupported> {
    let has_plain_call = graph
        .nodes
        .iter()
        .any(|node| matches!(node.kind, NodeKind::Call { .. }));
    let has_method_call = graph
        .nodes
        .iter()
        .any(|node| matches!(node.kind, NodeKind::CallMethod { .. }));
    if !has_plain_call && !has_method_call {
        return Ok(());
    }
    // A body whose every call is a frameless self-recursion and whose every
    // object op is a primitive slot access (no element / array-length / method
    // runtime stub) allocates nothing and bridges no frame: the recursive call
    // pushes a register window without spilling, and the inline slot ops touch
    // only already-allocated receivers held live across the call. The "call
    // churns object stubs and runs slower than the baseline IC" cost model that
    // motivates this reject does not apply — the optimizing tier's typed
    // arithmetic plus baked slot offsets beat the baseline here (e.g. a
    // self-recursive tree walk that reads/writes primitive node fields).
    if graph_allows_frameless_self_call(graph) && all_calls_are_self(graph) {
        return Ok(());
    }
    // A runtime memory op (an un-baked slot store, element access, or array
    // length read) bridges through the materialize-frame stub. Note `CallMethod`
    // is NOT counted here: it is itself a bridging call, accounted for by
    // `has_method_call`, so a body of purely method calls is not self-rejected.
    let has_mem_op = graph.nodes.iter().any(|node| {
        matches!(
            node.kind,
            NodeKind::StoreSlot(_, _, _)
                | NodeKind::LoadElement(_, _)
                | NodeKind::StoreElement(_, _, _)
                | NodeKind::LoadArrayLength(_)
        )
    });
    // Reject the bridge-churn mixes the cost model loses on: a plain `Call`
    // alongside any other bridging op (a memory op or an un-inlined method
    // call), or an un-inlined `CallMethod` alongside a memory op. Either way the
    // baseline's tuned IC sequence wins, and — for the method-call case — keeps
    // these bodies off the un-inlined-poly-method optimizing path.
    let reject =
        (has_plain_call && (has_mem_op || has_method_call)) || (has_method_call && has_mem_op);
    if reject {
        return Err(Unsupported::Unlowered("call plus object fast path"));
    }
    Ok(())
}

/// Whether every body node belongs to the frameless-self-recursion subset:
/// operations a self-recursive call can be emitted around without
/// materializing and reloading the interpreter frame.
///
/// The subset is exactly the non-allocating ops. Beyond the typed-arithmetic
/// and constant nodes, it admits the primitive slot accesses
/// (`CheckShape` / `LoadSlot` / `StoreSlot`): a body restricted to these reads
/// and writes already-allocated objects' primitive fields, so it allocates
/// nothing. No GC can move the tagged receivers held live across the recursive
/// call, and a primitive `StoreSlot` needs no generational write barrier —
/// which is what makes the frameless self-call (no spill / reload around the
/// recursive `bl`) sound. Element / array-length / method ops are deliberately
/// excluded: they can allocate (array growth) or recurse through a runtime
/// bridge that may GC.
pub(super) fn graph_allows_frameless_self_call(graph: &Graph) -> bool {
    graph.blocks.iter().all(|block| {
        block.body.iter().all(|&nid| {
            matches!(
                graph.node(nid).kind,
                NodeKind::Param(_)
                    | NodeKind::Phi(_)
                    | NodeKind::ConstUndefined
                    | NodeKind::ConstNull
                    | NodeKind::ConstBool(_)
                    | NodeKind::SelfClosure
                    | NodeKind::ConstInt32(_)
                    | NodeKind::CheckInt32(_)
                    | NodeKind::Int32Add(_, _)
                    | NodeKind::Int32Sub(_, _)
                    | NodeKind::Int32Mul(_, _)
                    | NodeKind::TaggedIsNull { .. }
                    | NodeKind::Int32BitOr(_, _)
                    | NodeKind::Int32BitAnd(_, _)
                    | NodeKind::Int32BitXor(_, _)
                    | NodeKind::Int32Shl(_, _)
                    | NodeKind::Int32Shr(_, _)
                    | NodeKind::Int32UshrToFloat64(_, _)
                    | NodeKind::Int32Compare(_, _, _)
                    | NodeKind::CheckShape(_, _)
                    | NodeKind::LoadSlot(_, _)
                    | NodeKind::StoreSlot(_, _, _)
                    | NodeKind::Call { .. }
            )
        })
    })
}

/// Whether every `Call` site denotes the running function itself (a
/// `SelfClosure` callee) — i.e. every call is a self-recursion, the precondition
/// for emitting all of them frameless.
pub(super) fn all_calls_are_self(graph: &Graph) -> bool {
    graph.nodes.iter().all(|node| match &node.kind {
        NodeKind::Call { inputs, .. } => matches!(
            inputs.first().map(|c| &graph.node(*c).kind),
            Some(NodeKind::SelfClosure)
        ),
        _ => true,
    })
}

/// Target byte-PC of a relative branch. Mirrors the interpreter / baseline:
/// `byte_pc + 1 + rel` (relative to the byte after the branch opcode).
fn branch_target(byte_pc: u32, rel: i32) -> i64 {
    i64::from(byte_pc) + 1 + i64::from(rel)
}

fn imm32(operands: &[Operand], i: usize) -> Result<i32, Unsupported> {
    match operands.get(i) {
        Some(Operand::Imm32(v)) => Ok(*v),
        _ => Err(Unsupported::OperandShape("expected imm32")),
    }
}

fn reg(operands: &[Operand], i: usize) -> Result<u16, Unsupported> {
    match operands.get(i) {
        Some(Operand::Register(r)) => Ok(*r),
        _ => Err(Unsupported::OperandShape("expected register")),
    }
}

fn const_index(operands: &[Operand], i: usize) -> Result<u32, Unsupported> {
    match operands.get(i) {
        Some(Operand::ConstIndex(n)) => Ok(*n),
        _ => Err(Unsupported::OperandShape("expected const index")),
    }
}

/// Static control-flow graph over the instruction stream: block leaders, the
/// instruction-index range of each block, and the successor / predecessor edge
/// sets.
struct Cfg {
    /// `[start, end)` instruction-index range for each block, in start-PC order.
    ranges: Vec<(usize, usize)>,
    /// Successor block ids per block.
    succs: Vec<Vec<BlockId>>,
    /// Predecessor block ids per block.
    preds: Vec<Vec<BlockId>>,
    /// Byte-PC → block id for every block leader.
    block_of_pc: BTreeMap<u32, BlockId>,
}

impl Cfg {
    fn discover(view: &JitFunctionView) -> Result<Self, Unsupported> {
        let instrs = &view.instructions;
        // Byte-PC → instruction index, for resolving branch targets and the
        // fallthrough successor.
        let mut index_of_pc: BTreeMap<u32, usize> = BTreeMap::new();
        for (i, instr) in instrs.iter().enumerate() {
            index_of_pc.insert(instr.byte_pc, i);
        }

        // Collect block leaders.
        let mut leaders: BTreeSet<u32> = BTreeSet::new();
        leaders.insert(instrs[0].byte_pc);
        for (i, instr) in instrs.iter().enumerate() {
            match instr.op {
                Op::Jump | Op::JumpIfTrue | Op::JumpIfFalse => {
                    let rel = imm32(&instr.operands, 0)?;
                    let target = branch_target(instr.byte_pc, rel);
                    let target = u32::try_from(target)
                        .ok()
                        .filter(|pc| index_of_pc.contains_key(pc))
                        .ok_or(Unsupported::BranchTarget(target))?;
                    leaders.insert(target);
                    if let Some(next) = instrs.get(i + 1) {
                        leaders.insert(next.byte_pc);
                    }
                }
                Op::Return | Op::ReturnValue | Op::ReturnUndefined => {
                    if let Some(next) = instrs.get(i + 1) {
                        leaders.insert(next.byte_pc);
                    }
                }
                _ => {}
            }
        }

        // Slice into blocks in start-PC order.
        let leader_pcs: Vec<u32> = leaders.iter().copied().collect();
        let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(leader_pcs.len());
        let mut block_of_pc: BTreeMap<u32, BlockId> = BTreeMap::new();
        for (b, &pc) in leader_pcs.iter().enumerate() {
            let start = index_of_pc[&pc];
            let end = leader_pcs
                .get(b + 1)
                .map_or(instrs.len(), |next| index_of_pc[next]);
            ranges.push((start, end));
            block_of_pc.insert(pc, b as BlockId);
        }

        // Build successor edges from each block's last instruction.
        let mut succs: Vec<Vec<BlockId>> = vec![Vec::new(); ranges.len()];
        for (b, &(start, end)) in ranges.iter().enumerate() {
            let last = &instrs[end - 1];
            let fallthrough = |succs: &mut Vec<BlockId>| {
                if end < instrs.len() {
                    succs.push(block_of_pc[&instrs[end].byte_pc]);
                }
            };
            match last.op {
                Op::Jump => {
                    let rel = imm32(&last.operands, 0)?;
                    let target = u32::try_from(branch_target(last.byte_pc, rel)).unwrap();
                    succs[b].push(block_of_pc[&target]);
                }
                Op::JumpIfTrue | Op::JumpIfFalse => {
                    let rel = imm32(&last.operands, 0)?;
                    let target = u32::try_from(branch_target(last.byte_pc, rel)).unwrap();
                    succs[b].push(block_of_pc[&target]);
                    fallthrough(&mut succs[b]);
                }
                Op::Return | Op::ReturnValue | Op::ReturnUndefined => {}
                _ => {
                    // Block ended because the next instruction is a leader:
                    // straight-line fallthrough.
                    fallthrough(&mut succs[b]);
                }
            }
            debug_assert!(start < end);
        }

        // Invert into predecessor edges.
        let mut preds: Vec<Vec<BlockId>> = vec![Vec::new(); ranges.len()];
        for (b, outs) in succs.iter().enumerate() {
            for &s in outs {
                preds[s as usize].push(b as BlockId);
            }
        }

        Ok(Self {
            ranges,
            succs,
            preds,
            block_of_pc,
        })
    }

    fn reachable_from(&self, entry: BlockId) -> FxHashSet<BlockId> {
        let mut seen = FxHashSet::default();
        let mut stack = vec![entry];
        while let Some(block) = stack.pop() {
            if !seen.insert(block) {
                continue;
            }
            for &succ in &self.succs[block as usize] {
                stack.push(succ);
            }
        }
        seen
    }
}

/// SSA construction state.
struct Builder<'a> {
    view: &'a JitFunctionView,
    cfg: Cfg,
    graph: Graph,
    /// Real CFG blocks reachable from the selected entry. Whole-function
    /// compiles include every block; OSR-target compiles include only blocks
    /// reachable from the hot loop header.
    active_blocks: FxHashSet<BlockId>,
    /// `current_def[reg][block]` — the SSA node defining `reg` at the end of (or
    /// within) `block`.
    current_def: Vec<FxHashMap<BlockId, NodeId>>,
    /// Phis created in still-unsealed blocks, pending operand fill-in at seal
    /// time: `block → [(register, phi node)]`.
    incomplete_phis: FxHashMap<BlockId, Vec<(u16, NodeId)>>,
    /// The object literal currently being folded: set when a `NewObject` with a
    /// baked [`crate::ObjectLiteralPlan`] is reached, cleared when its final
    /// `DefineDataProperty` emits the `AllocObjectLiteral`. While set, the
    /// literal's key `LoadString`s are skipped and each `DefineDataProperty`
    /// captures its value SSA instead of running.
    active_literal: Option<ActiveLiteral>,
}

/// In-flight state for folding one object literal in the builder.
struct ActiveLiteral {
    /// Register the literal's object is written to.
    obj_reg: u16,
    /// Final hidden-class shape, compressed `Gc` offset.
    shape_offset: u32,
    /// Byte-PC → slot index for each `DefineDataProperty` to capture.
    define_slot: FxHashMap<u32, usize>,
    /// Byte-PCs of the key `LoadString`s to skip.
    key_pcs: FxHashSet<u32>,
    /// Byte-PC of the final define (where `AllocObjectLiteral` is emitted).
    last_define_pc: u32,
    /// Captured property-value SSA nodes in slot order (filled at each define).
    captured: Vec<Option<NodeId>>,
}

impl<'a> Builder<'a> {
    fn new(view: &'a JitFunctionView, cfg: Cfg, osr_pc: Option<u32>) -> Result<Self, Unsupported> {
        let (entry, active_blocks, synthetic_entry) = if let Some(pc) = osr_pc {
            let target = *cfg
                .block_of_pc
                .get(&pc)
                .ok_or(Unsupported::BranchTarget(i64::from(pc)))?;
            let active = cfg.reachable_from(target);
            (
                cfg.ranges.len() as BlockId,
                active,
                Some(cfg.ranges.len() as BlockId),
            )
        } else {
            (0, (0..cfg.ranges.len() as BlockId).collect(), None)
        };
        // Materialize one block per CFG range (the graph starts with just the
        // entry block).
        let mut graph = Graph::new(view.param_count, view.register_count, entry);
        graph.blocks.clear();
        for &(start, _) in &cfg.ranges {
            let pc = view.instructions[start].byte_pc;
            graph.blocks.push(super::ir::Block::new(pc));
        }
        if synthetic_entry.is_some() {
            graph.blocks.push(super::ir::Block::new(osr_pc.unwrap()));
        }
        for (b, p) in cfg.preds.iter().enumerate() {
            let mut preds: Vec<BlockId> = p
                .iter()
                .copied()
                .filter(|pred| active_blocks.contains(pred))
                .collect();
            if Some(b as BlockId) == osr_pc.and_then(|pc| cfg.block_of_pc.get(&pc).copied()) {
                preds.insert(0, entry);
            }
            graph.blocks[b].preds = preds;
        }
        if let Some(entry) = synthetic_entry {
            let target = cfg.block_of_pc[&osr_pc.unwrap()];
            graph.blocks[entry as usize].term = Some(Terminator::Jump(target));
            graph.blocks[entry as usize].sealed = true;
            graph.blocks[entry as usize].filled = true;
        }
        let reg_count = view.register_count as usize;
        Ok(Self {
            view,
            cfg,
            graph,
            active_blocks,
            current_def: vec![FxHashMap::default(); reg_count],
            incomplete_phis: FxHashMap::default(),
            active_literal: None,
        })
    }

    fn run(&mut self) -> Result<(), Unsupported> {
        // Entry block: a normal function entry sees formal parameters in the
        // leading registers and `undefined` in locals / scratch. An OSR entry is
        // different: every bytecode register is supplied by the interpreter
        // frame at the loop header, so every seed is a frame parameter.
        for r in 0..self.view.register_count {
            let kind = if self.is_osr_target() || r < self.view.param_count {
                NodeKind::Param(r)
            } else {
                NodeKind::ConstUndefined
            };
            let entry = self.graph.entry;
            let node = self.graph.add_node(kind, entry, 0);
            self.graph.blocks[entry as usize].body.push(node);
            self.write_variable(r, entry, node);
        }

        let block_count = self.cfg.ranges.len();
        for b in 0..block_count {
            if !self.active_blocks.contains(&(b as BlockId)) {
                self.graph.blocks[b].filled = true;
                self.graph.blocks[b].sealed = true;
                continue;
            }
            self.seal_ready();
            self.fill_block(b as BlockId)?;
            self.graph.blocks[b].filled = true;
            self.seal_ready();
        }
        self.seal_ready();
        debug_assert!(self.graph.blocks.iter().all(|blk| blk.sealed));

        // On-demand SSA construction can leave a phi trivial after one of its
        // operands collapses on a back edge *after* the phi's own triviality
        // check ran, and the collapse's user-recursion does not always reach it.
        // Such a residue is orphaned from its block's phi list but still named by
        // `reg_writes` (and thus by deopt / OSR frame states), where it has no
        // home and restores garbage. Sweep to a fixpoint so no trivial phi
        // survives into the backend.
        let mut visited: FxHashSet<NodeId> = FxHashSet::default();
        loop {
            let trivial = self.graph.nodes.iter().enumerate().find_map(|(id, node)| {
                let id = id as NodeId;
                if visited.contains(&id) {
                    return None;
                }
                let NodeKind::Phi(ops) = &node.kind else {
                    return None;
                };
                let mut same: Option<NodeId> = None;
                for &op in ops {
                    if op == id || Some(op) == same {
                        continue;
                    }
                    if same.is_some() {
                        return None;
                    }
                    same = Some(op);
                }
                same.map(|_| id)
            });
            match trivial {
                Some(phi) => {
                    visited.insert(phi);
                    self.try_remove_trivial_phi(phi);
                }
                None => break,
            }
        }
        Ok(())
    }

    fn is_osr_target(&self) -> bool {
        self.graph.entry != 0
    }

    fn allow_empty_feedback_for_osr(&self, feedback: ArithFeedback) -> bool {
        feedback.is_empty() && self.is_osr_target()
    }

    fn operand_is_float64(&mut self, block: BlockId, reg: u16) -> bool {
        let node = self.read_variable(reg, block);
        self.graph.node(node).repr == Repr::Float64
    }

    fn node_is_float64(&self, node: NodeId) -> bool {
        self.graph.node(node).repr == Repr::Float64
    }

    /// Seal every unsealed block whose predecessors are all filled, to a
    /// fixpoint. Sealing a block finalizes its incomplete phis.
    fn seal_ready(&mut self) {
        loop {
            let mut progressed = false;
            for b in 0..self.graph.blocks.len() {
                if self.graph.blocks[b].sealed {
                    continue;
                }
                let ready = self.graph.blocks[b]
                    .preds
                    .iter()
                    .all(|&p| self.graph.blocks[p as usize].filled);
                if ready {
                    self.seal_block(b as BlockId);
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }
    }

    fn seal_block(&mut self, block: BlockId) {
        if let Some(list) = self.incomplete_phis.remove(&block) {
            for (reg, phi) in list {
                self.add_phi_operands(reg, phi);
            }
        }
        self.graph.blocks[block as usize].sealed = true;
    }

    fn write_variable(&mut self, reg: u16, block: BlockId, node: NodeId) {
        self.current_def[reg as usize].insert(block, node);
    }

    /// Bind `reg` to `node` in `block` while translating the instruction at
    /// `byte_pc`, and log the rebind for deopt frame-state reconstruction. Used
    /// for every per-instruction register definition (including `LoadLocal` /
    /// `StoreLocal` aliasing) so deopt knows the exact SSA value each register
    /// holds at a guard. Phi creation and the entry-block seeding use
    /// [`Self::write_variable`] directly (their register state is reconstructed by
    /// deopt from block entry, not from this log).
    fn def_register(&mut self, reg: u16, block: BlockId, node: NodeId, byte_pc: u32) {
        self.write_variable(reg, block, node);
        self.graph
            .reg_writes
            .entry(block)
            .or_default()
            .push((byte_pc, reg, node));
    }

    fn read_variable(&mut self, reg: u16, block: BlockId) -> NodeId {
        if let Some(&node) = self.current_def[reg as usize].get(&block) {
            return node;
        }
        self.read_variable_recursive(reg, block)
    }

    fn read_variable_recursive(&mut self, reg: u16, block: BlockId) -> NodeId {
        let val = if !self.graph.blocks[block as usize].sealed {
            // Unknown predecessors: an incomplete phi, filled at seal time.
            let phi = self.new_phi(reg, block);
            self.incomplete_phis
                .entry(block)
                .or_default()
                .push((reg, phi));
            phi
        } else {
            let preds = self.graph.blocks[block as usize].preds.clone();
            if preds.len() == 1 {
                self.read_variable(reg, preds[0])
            } else {
                // Place the phi and record it as this register's def *before*
                // filling operands, so a self-referential loop terminates.
                let phi = self.new_phi(reg, block);
                self.write_variable(reg, block, phi);
                self.add_phi_operands(reg, phi)
            }
        };
        self.write_variable(reg, block, val);
        val
    }

    fn new_phi(&mut self, reg: u16, block: BlockId) -> NodeId {
        let pc = self.graph.blocks[block as usize].start_pc;
        let phi = self.graph.add_node(NodeKind::Phi(Vec::new()), block, pc);
        self.graph.blocks[block as usize].phis.push(phi);
        self.graph.phi_reg.insert(phi, reg);
        phi
    }

    fn add_phi_operands(&mut self, reg: u16, phi: NodeId) -> NodeId {
        let block = self.graph.node(phi).block;
        let preds = self.graph.blocks[block as usize].preds.clone();
        let mut operands = Vec::with_capacity(preds.len());
        for pred in preds {
            operands.push(self.read_variable(reg, pred));
        }
        self.graph.nodes[phi as usize].kind = NodeKind::Phi(operands);
        self.try_remove_trivial_phi(phi)
    }

    /// Braun et al. `tryRemoveTrivialPhi`: a phi all of whose operands are the
    /// same value (ignoring self-references) is redundant — replace every use of
    /// it with that single value, drop it from its block, and recurse on any phi
    /// that used it (it may have become trivial too). Returns the value callers
    /// should use in place of `phi` (the collapsed value, or `phi` if it is a
    /// real merge). Keeps the graph free of the phis-for-unchanged-registers that
    /// on-demand SSA construction otherwise produces — fewer phis, fewer
    /// resolution moves, and a cleaner deopt frame state.
    fn try_remove_trivial_phi(&mut self, phi: NodeId) -> NodeId {
        let NodeKind::Phi(operands) = self.graph.node(phi).kind.clone() else {
            return phi;
        };
        let mut same: Option<NodeId> = None;
        for op in operands {
            if op == phi || Some(op) == same {
                continue; // self-reference or a repeat of the one distinct value
            }
            if same.is_some() {
                return phi; // two distinct inputs: a real merge
            }
            same = Some(op);
        }
        let Some(same) = same else {
            // No distinct operand (empty / pure self-loop): unreachable in
            // practice; leave it rather than invent a value.
            return phi;
        };
        // Collect phi users (which may themselves become trivial) before the
        // rewrite redirects them.
        let phi_users: Vec<NodeId> = self
            .graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(id, node)| {
                *id as NodeId != phi
                    && matches!(node.kind, NodeKind::Phi(_))
                    && node.kind.inputs().contains(&phi)
            })
            .map(|(id, _)| id as NodeId)
            .collect();
        self.replace_all_uses(phi, same);
        let block = self.graph.node(phi).block;
        self.graph.blocks[block as usize].phis.retain(|&p| p != phi);
        for user in phi_users {
            self.try_remove_trivial_phi(user);
        }
        same
    }

    /// Redirect every reference to `old` (node operands, terminators, and the
    /// per-block register definitions) to `new`.
    fn replace_all_uses(&mut self, old: NodeId, new: NodeId) {
        for node in &mut self.graph.nodes {
            node.kind.replace_input(old, new);
        }
        for block in &mut self.graph.blocks {
            match &mut block.term {
                Some(Terminator::Return(v)) if *v == old => *v = new,
                Some(Terminator::Branch { cond, .. }) if *cond == old => *cond = new,
                _ => {}
            }
        }
        for defs in &mut self.current_def {
            for v in defs.values_mut() {
                if *v == old {
                    *v = new;
                }
            }
        }
        // The per-block register-write log is replayed by the deopt / OSR frame
        // reconstruction to map each interpreter register to its SSA value. A
        // collapsed trivial phi must be redirected here too, or a guard's frame
        // state restores a value with no home: read uninitialized, corrupting
        // the resumed interpreter frame.
        for writes in self.graph.reg_writes.values_mut() {
            for (_, _, v) in writes.iter_mut() {
                if *v == old {
                    *v = new;
                }
            }
        }
    }

    /// Translate one block's instructions into nodes and a terminator.
    fn fill_block(&mut self, block: BlockId) -> Result<(), Unsupported> {
        let (start, end) = self.cfg.ranges[block as usize];
        for i in start..end {
            // Capture the instruction's fields up front so the `&self.view`
            // borrow ends before the `&mut self.graph` mutations below.
            let instr = &self.view.instructions[i];
            let op = instr.op;
            let byte_pc = instr.byte_pc;
            let feedback = instr.arith_feedback;
            let make_self = instr.make_self;
            let load_number = instr.load_number;
            let load_array_length = instr.load_array_length;
            let property_feedback = instr.property_feedback;
            let property_ic_site = instr.property_ic_site;
            let object_literal = instr.object_literal.clone();
            let operands = instr.operands.clone();

            // Object-literal folding. While a literal is active, its key
            // `LoadString`s carry no value (the key is implied by the baked
            // shape) and each `DefineDataProperty` captures its value SSA rather
            // than running; the final define emits a single `AllocObjectLiteral`.
            if self
                .active_literal
                .as_ref()
                .is_some_and(|a| a.key_pcs.contains(&byte_pc))
            {
                continue;
            }
            let active_define = self.active_literal.as_ref().and_then(|a| {
                a.define_slot
                    .get(&byte_pc)
                    .map(|&s| (s, byte_pc == a.last_define_pc))
            });
            if let Some((slot, is_last)) = active_define {
                let value_reg = reg(&operands, 2)?;
                let value = self.read_variable(value_reg, block);
                self.active_literal
                    .as_mut()
                    .expect("active literal")
                    .captured[slot] = Some(value);
                if is_last {
                    let active = self.active_literal.take().expect("active literal");
                    let inputs: Vec<NodeId> = active
                        .captured
                        .iter()
                        .map(|c| c.expect("every literal slot captured before the final define"))
                        .collect();
                    let node = self.graph.add_node(
                        NodeKind::AllocObjectLiteral {
                            shape_offset: active.shape_offset,
                            inputs,
                        },
                        block,
                        byte_pc,
                    );
                    self.graph.set_frame_dst(node, active.obj_reg);
                    self.push_body(block, node);
                    self.def_register(active.obj_reg, block, node, byte_pc);
                }
                continue;
            }
            if let Some(plan) = object_literal {
                // A second literal opening before the current one closes (a
                // nested literal) is not folded here; keep it simple and decline.
                if self.active_literal.is_some() {
                    return Err(Unsupported::Unlowered("nested object literal"));
                }
                let last_define_pc = plan
                    .defines
                    .last()
                    .map(|p| p.define_pc)
                    .ok_or(Unsupported::Unlowered("object literal without properties"))?;
                let define_slot = plan
                    .defines
                    .iter()
                    .enumerate()
                    .map(|(slot, p)| (p.define_pc, slot))
                    .collect();
                self.active_literal = Some(ActiveLiteral {
                    obj_reg: plan.obj_reg,
                    shape_offset: plan.shape_offset,
                    define_slot,
                    key_pcs: plan.key_pcs.iter().copied().collect(),
                    last_define_pc,
                    captured: vec![None; plan.defines.len()],
                });
                // The `NewObject` itself emits nothing; the object materializes at
                // the final define.
                continue;
            }

            match op {
                // The leading named-function self-binding. The closure value is
                // never a numeric operand in this subset; only `make_self`
                // (no allocation) is accepted — any other function/closure maker
                // allocates and bails to the baseline.
                Op::MakeFunction | Op::MakeClosure if make_self => {
                    let dst = reg(&operands, 0)?;
                    let node = self.graph.add_node(NodeKind::SelfClosure, block, byte_pc);
                    self.def_register(dst, block, node, byte_pc);
                }
                Op::LoadInt32 => {
                    let dst = reg(&operands, 0)?;
                    let v = imm32(&operands, 1)?;
                    let node = self.graph.add_node(NodeKind::ConstInt32(v), block, byte_pc);
                    self.graph.set_frame_dst(node, dst);
                    self.push_body(block, node);
                    self.def_register(dst, block, node, byte_pc);
                }
                Op::LoadNumber => {
                    let dst = reg(&operands, 0)?;
                    // The f64 value of the number-constant-pool entry is baked
                    // into the view at compile snapshot time; without it the
                    // constant cannot be materialized, so bail.
                    let v = load_number
                        .ok_or(Unsupported::OperandShape("LoadNumber constant unresolved"))?;
                    let node = self.graph.add_node(NodeKind::ConstF64(v), block, byte_pc);
                    self.graph.set_frame_dst(node, dst);
                    self.push_body(block, node);
                    self.def_register(dst, block, node, byte_pc);
                }
                Op::LoadTrue | Op::LoadFalse => {
                    let dst = reg(&operands, 0)?;
                    let node = self.graph.add_node(
                        NodeKind::ConstBool(matches!(op, Op::LoadTrue)),
                        block,
                        byte_pc,
                    );
                    self.graph.set_frame_dst(node, dst);
                    self.push_body(block, node);
                    self.def_register(dst, block, node, byte_pc);
                }
                // `LoadProperty dst, obj, name` — lower a monomorphic own-data
                // site to a `CheckShape` guard plus an inline `LoadSlot`. A site
                // without baked own-data feedback (polymorphic / prototype /
                // dictionary / never observed) bails the function.
                Op::LoadProperty => {
                    let dst = reg(&operands, 0)?;
                    let obj_reg = reg(&operands, 1)?;
                    if load_array_length {
                        if self.view.cage_base == 0 {
                            self.deopt_or_decline(
                                block,
                                byte_pc,
                                Unsupported::Opcode(Op::LoadProperty),
                            )?;
                            return Ok(());
                        }
                        let obj = self.read_variable(obj_reg, block);
                        let load =
                            self.graph
                                .add_node(NodeKind::LoadArrayLength(obj), block, byte_pc);
                        self.graph.set_frame_dst(load, dst);
                        self.push_body(block, load);
                        self.def_register(dst, block, load, byte_pc);
                        continue;
                    }
                    // Inline slot access decompresses object pointers against the
                    // baked GC cage base; without it the layout offsets are absent.
                    let Some((shape, slot_byte)) =
                        property_feedback.filter(|_| self.view.cage_base != 0)
                    else {
                        self.deopt_or_decline(
                            block,
                            byte_pc,
                            Unsupported::Opcode(Op::LoadProperty),
                        )?;
                        return Ok(());
                    };
                    let obj = self.read_variable(obj_reg, block);
                    let checked =
                        self.graph
                            .add_node(NodeKind::CheckShape(obj, shape), block, byte_pc);
                    self.push_body(block, checked);
                    let load =
                        self.graph
                            .add_node(NodeKind::LoadSlot(checked, slot_byte), block, byte_pc);
                    self.graph.set_frame_dst(load, dst);
                    self.push_body(block, load);
                    self.def_register(dst, block, load, byte_pc);
                }
                // `StoreProperty obj, name, src` — `CheckShape` + inline
                // `StoreSlot`. A primitive (int32 / f64) value needs no write
                // barrier (a primitive `Value` is never a `Gc` pointer). A
                // `Tagged` value may be a heap pointer, so its `StoreSlot` carries
                // the inline generational card-mark (parent old + child young →
                // mark the parent's card); the card-mark allocates nothing and
                // never moves GC, so it needs no safepoint. A non-own-data site
                // (no baked shape feedback) still bails to the baseline.
                Op::StoreProperty => {
                    let obj_reg = reg(&operands, 0)?;
                    let src_reg = reg(&operands, 2)?;
                    let Some((shape, slot_byte)) =
                        property_feedback.filter(|_| self.view.cage_base != 0)
                    else {
                        self.deopt_or_decline(
                            block,
                            byte_pc,
                            Unsupported::Opcode(Op::StoreProperty),
                        )?;
                        return Ok(());
                    };
                    let value = self.read_variable(src_reg, block);
                    if !matches!(
                        self.graph.node(value).kind.repr(),
                        Repr::Int32 | Repr::Float64 | Repr::Tagged
                    ) {
                        self.deopt_or_decline(
                            block,
                            byte_pc,
                            Unsupported::Opcode(Op::StoreProperty),
                        )?;
                        return Ok(());
                    }
                    let obj = self.read_variable(obj_reg, block);
                    let checked =
                        self.graph
                            .add_node(NodeKind::CheckShape(obj, shape), block, byte_pc);
                    self.push_body(block, checked);
                    let store = self.graph.add_node(
                        NodeKind::StoreSlot(checked, slot_byte, value),
                        block,
                        byte_pc,
                    );
                    self.push_body(block, store);
                }
                // `LoadElement dst, recv, idx` — inline only dense Array and
                // Float64Array / Int32Array fast paths. Every miss deoptimizes at
                // this exact PC, letting the interpreter perform the full
                // computed `[[Get]]` semantics.
                Op::LoadElement => {
                    let dst = reg(&operands, 0)?;
                    let recv_reg = reg(&operands, 1)?;
                    let idx_reg = reg(&operands, 2)?;
                    if self.view.cage_base == 0 {
                        self.deopt_or_decline(block, byte_pc, Unsupported::Opcode(op))?;
                        return Ok(());
                    }
                    let recv = self.read_variable(recv_reg, block);
                    let idx = match self.int32_index_operand(block, idx_reg, byte_pc) {
                        Ok(idx) => idx,
                        Err(reason) => {
                            self.deopt_or_decline(block, byte_pc, reason)?;
                            return Ok(());
                        }
                    };
                    let load =
                        self.graph
                            .add_node(NodeKind::LoadElement(recv, idx), block, byte_pc);
                    self.graph.set_frame_dst(load, dst);
                    self.push_body(block, load);
                    self.def_register(dst, block, load, byte_pc);
                }
                // `StoreElement recv, idx, src` — inline typed-array element
                // stores for primitive numeric values. Every miss deoptimizes at
                // this exact PC, letting the interpreter perform full `[[Set]]`
                // semantics, including coercion cases this tier does not lower.
                Op::StoreElement => {
                    let recv_reg = reg(&operands, 0)?;
                    let idx_reg = reg(&operands, 1)?;
                    let src_reg = reg(&operands, 2)?;
                    if self.view.cage_base == 0 {
                        self.deopt_or_decline(block, byte_pc, Unsupported::Opcode(op))?;
                        return Ok(());
                    }
                    let recv = self.read_variable(recv_reg, block);
                    let idx = match self.int32_index_operand(block, idx_reg, byte_pc) {
                        Ok(idx) => idx,
                        Err(reason) => {
                            self.deopt_or_decline(block, byte_pc, reason)?;
                            return Ok(());
                        }
                    };
                    let value = self.read_variable(src_reg, block);
                    if !matches!(
                        self.graph.node(value).kind.repr(),
                        Repr::Int32 | Repr::Float64
                    ) {
                        self.deopt_or_decline(block, byte_pc, Unsupported::Opcode(op))?;
                        return Ok(());
                    }
                    let store = self.graph.add_node(
                        NodeKind::StoreElement(recv, idx, value),
                        block,
                        byte_pc,
                    );
                    self.push_body(block, store);
                }
                Op::Call => {
                    const MAX_INLINE_ARGS: usize = 4;
                    let dst = reg(&operands, 0)?;
                    let callee_reg = reg(&operands, 1)?;
                    let argc = const_index(&operands, 2)? as usize;
                    if argc > MAX_INLINE_ARGS {
                        return Err(Unsupported::OperandShape("call arg count"));
                    }
                    let mut inputs = Vec::with_capacity(argc + 1);
                    inputs.push(self.read_variable(callee_reg, block));
                    let mut arg_regs = Vec::with_capacity(argc);
                    for slot in 0..argc {
                        let arg_reg = reg(&operands, 3 + slot)?;
                        arg_regs.push(arg_reg);
                        inputs.push(self.read_variable(arg_reg, block));
                    }
                    if let Some(callee) = self.view.inline_callees.get(&byte_pc).cloned()
                        && let Some(result) = self
                            .try_inline_direct_call(block, byte_pc, callee_reg, &callee, &inputs)?
                    {
                        self.def_register(dst, block, result, byte_pc);
                        continue;
                    }
                    let call = self.graph.add_node(
                        NodeKind::Call {
                            callee_reg,
                            arg_regs,
                            inputs,
                        },
                        block,
                        byte_pc,
                    );
                    self.graph.set_frame_dst(call, dst);
                    self.push_body(block, call);
                    self.def_register(dst, block, call, byte_pc);
                }
                Op::CallMethodValue => {
                    const MAX_METHOD_ARGS: usize = 3;
                    let dst = reg(&operands, 0)?;
                    let recv_reg = reg(&operands, 1)?;
                    let name = const_index(&operands, 2)?;
                    let argc = const_index(&operands, 3)? as usize;
                    if argc > MAX_METHOD_ARGS {
                        return Err(Unsupported::OperandShape("method call arg count"));
                    }
                    let Some(site) = property_ic_site else {
                        self.deopt_or_decline(block, byte_pc, Unsupported::Opcode(op))?;
                        return Ok(());
                    };
                    let recv = self.read_variable(recv_reg, block);
                    let mut args = Vec::with_capacity(argc);
                    let mut arg_regs = Vec::with_capacity(argc);
                    for slot in 0..argc {
                        let arg_reg = reg(&operands, 4 + slot)?;
                        arg_regs.push(arg_reg);
                        args.push(self.read_variable(arg_reg, block));
                    }
                    // Dense-array `pop()` / `push(value)`: lower to a dedicated
                    // speculative node (inline leaf pop; safepointed runtime push)
                    // so the enclosing loop compiles in the optimizing tier instead
                    // of declining on the method call. Other arities fall through.
                    if let Some(am) = self.view.array_methods.get(&byte_pc).copied() {
                        match am.kind {
                            JitArrayMethodKind::Pop if argc == 0 => {
                                let node = self.graph.add_node(
                                    NodeKind::ArrayPop { recv },
                                    block,
                                    byte_pc,
                                );
                                self.graph.set_frame_dst(node, dst);
                                self.push_body(block, node);
                                self.def_register(dst, block, node, byte_pc);
                                continue;
                            }
                            JitArrayMethodKind::Push if argc == 1 => {
                                let value = args[0];
                                let node = self.graph.add_node(
                                    NodeKind::ArrayPush {
                                        recv,
                                        value,
                                        recv_reg,
                                    },
                                    block,
                                    byte_pc,
                                );
                                self.graph.set_frame_dst(node, dst);
                                self.push_body(block, node);
                                self.def_register(dst, block, node, byte_pc);
                                continue;
                            }
                            _ => {}
                        }
                    }
                    if let Some(method) = self.view.inline_methods.get(&byte_pc).cloned() {
                        let unsafe_receiver = matches!(
                            &self.graph.node(recv).kind,
                            NodeKind::ConstUndefined | NodeKind::LoadHole
                        );
                        if unsafe_receiver {
                            self.deopt_or_decline(block, byte_pc, Unsupported::Opcode(op))?;
                            return Ok(());
                        }
                        if let Some(result) =
                            self.try_inline_method(block, byte_pc, recv, &method, argc, &args)?
                        {
                            self.def_register(dst, block, result, byte_pc);
                            continue;
                        }
                    } else if self.block_is_in_loop(block) {
                        return Err(Unsupported::Opcode(op));
                    }
                    let call = self.graph.add_node(
                        NodeKind::CallMethod {
                            recv,
                            recv_reg,
                            name,
                            site: site as u64,
                            arg_regs,
                            args,
                        },
                        block,
                        byte_pc,
                    );
                    self.graph.set_frame_dst(call, dst);
                    self.push_body(block, call);
                    self.def_register(dst, block, call, byte_pc);
                }
                Op::LoadUndefined => {
                    let dst = reg(&operands, 0)?;
                    let node = self
                        .graph
                        .add_node(NodeKind::ConstUndefined, block, byte_pc);
                    self.graph.set_frame_dst(node, dst);
                    self.push_body(block, node);
                    self.def_register(dst, block, node, byte_pc);
                }
                Op::LoadNull => {
                    let dst = reg(&operands, 0)?;
                    let node = self.graph.add_node(NodeKind::ConstNull, block, byte_pc);
                    self.graph.set_frame_dst(node, dst);
                    self.push_body(block, node);
                    self.def_register(dst, block, node, byte_pc);
                }
                Op::LoadUpvalue => {
                    let dst = reg(&operands, 0)?;
                    let idx = imm32(&operands, 1)?;
                    if idx < 0 || self.view.cage_base == 0 {
                        self.deopt_or_decline(block, byte_pc, Unsupported::Opcode(op))?;
                        return Ok(());
                    }
                    let node = self
                        .graph
                        .add_node(NodeKind::LoadUpvalue(idx), block, byte_pc);
                    self.graph.set_frame_dst(node, dst);
                    self.push_body(block, node);
                    self.def_register(dst, block, node, byte_pc);
                }
                // `this` and the TDZ hole sentinel: small Tagged loads that let
                // methods (`this.x`) and hole-using bodies enter the optimizing
                // tier. A hole `this` (derived-ctor before `super`) deopts.
                Op::LoadThis | Op::LoadHole => {
                    let dst = reg(&operands, 0)?;
                    let kind = if matches!(op, Op::LoadThis) {
                        NodeKind::LoadThis
                    } else {
                        NodeKind::LoadHole
                    };
                    let node = self.graph.add_node(kind, block, byte_pc);
                    self.graph.set_frame_dst(node, dst);
                    self.push_body(block, node);
                    self.def_register(dst, block, node, byte_pc);
                }
                // `LoadLocal dst, srcIdx` / `StoreLocal src, dstIdx` are register
                // copies (the local index is an inline immediate). Alias the SSA
                // value; no node needed.
                Op::LoadLocal => {
                    let dst = reg(&operands, 0)?;
                    let src = u16::try_from(imm32(&operands, 1)?)
                        .map_err(|_| Unsupported::OperandShape("local index"))?;
                    let node = self.read_variable(src, block);
                    self.def_register(dst, block, node, byte_pc);
                }
                Op::StoreLocal => {
                    let src = reg(&operands, 0)?;
                    let dst = u16::try_from(imm32(&operands, 1)?)
                        .map_err(|_| Unsupported::OperandShape("local index"))?;
                    let node = self.read_variable(src, block);
                    self.def_register(dst, block, node, byte_pc);
                }
                // `ToPrimitive` / `ToNumeric` are identity on a number, and the
                // arithmetic site's `CheckInt32` / `CheckNumber` guard enforces
                // the numeric speculation: a non-numeric operand bails to the
                // interpreter, which performs the spec-correct coercion. So under
                // numeric speculation these are sound as a register copy.
                Op::ToPrimitive | Op::ToNumeric => {
                    let dst = reg(&operands, 0)?;
                    let src = reg(&operands, 1)?;
                    let node = self.read_variable(src, block);
                    self.def_register(dst, block, node, byte_pc);
                }
                Op::Add | Op::Sub | Op::Mul | Op::Div => {
                    let (dst, lhs, rhs) =
                        (reg(&operands, 0)?, reg(&operands, 1)?, reg(&operands, 2)?);
                    let node = match self.arith_binop(block, op, lhs, rhs, feedback, byte_pc) {
                        Ok(node) => node,
                        Err(reason @ Unsupported::TypeFeedback(_)) => {
                            self.deopt_or_decline(block, byte_pc, reason)?;
                            return Ok(());
                        }
                        Err(reason) => return Err(reason),
                    };
                    self.graph.set_frame_dst(node, dst);
                    self.push_body(block, node);
                    self.def_register(dst, block, node, byte_pc);
                }
                // `Math.fn(x)` for the unary methods that are one exact float
                // instruction. Other methods (ties-to-+Inf `round`, multi-arg
                // `min`/`max`/`pow`, transcendentals needing libm) decline.
                Op::MathCall => {
                    let dst = reg(&operands, 0)?;
                    let method_id = const_index(&operands, 1)?;
                    let argc = const_index(&operands, 2)? as usize;
                    let uop = match otter_bytecode::method_id::MathMethod::from_u32(method_id) {
                        Some(otter_bytecode::method_id::MathMethod::Sqrt) => Float64UnaryOp::Sqrt,
                        Some(otter_bytecode::method_id::MathMethod::Abs) => Float64UnaryOp::Abs,
                        Some(otter_bytecode::method_id::MathMethod::Floor) => Float64UnaryOp::Floor,
                        Some(otter_bytecode::method_id::MathMethod::Ceil) => Float64UnaryOp::Ceil,
                        Some(otter_bytecode::method_id::MathMethod::Trunc) => Float64UnaryOp::Trunc,
                        _ => {
                            self.deopt_or_decline(block, byte_pc, Unsupported::Opcode(op))?;
                            return Ok(());
                        }
                    };
                    if argc != 1 {
                        self.deopt_or_decline(block, byte_pc, Unsupported::Opcode(op))?;
                        return Ok(());
                    }
                    let arg = self.read_variable(reg(&operands, 3)?, block);
                    let f = self.float_node_operand(block, arg, byte_pc);
                    let node = self
                        .graph
                        .add_node(NodeKind::Float64Unary(uop, f), block, byte_pc);
                    self.graph.set_frame_dst(node, dst);
                    self.push_body(block, node);
                    self.def_register(dst, block, node, byte_pc);
                }
                // Bitwise / shift sites are integer ops: JS coerces both operands
                // to int32. `|`, `&`, `^`, `<<`, `>>` stay in signed-int32
                // range; `>>>` widens the unsigned result to float64.
                // Speculate int32 operands from feedback (`CheckInt32`); a
                // non-int32 site bails.
                Op::BitwiseOr | Op::BitwiseAnd | Op::BitwiseXor | Op::Shl | Op::Shr | Op::Ushr => {
                    let (dst, lhs, rhs) =
                        (reg(&operands, 0)?, reg(&operands, 1)?, reg(&operands, 2)?);
                    let node = match self.bitwise_binop(block, op, lhs, rhs, feedback, byte_pc) {
                        Ok(node) => node,
                        Err(reason @ Unsupported::TypeFeedback(_)) => {
                            self.deopt_or_decline(block, byte_pc, reason)?;
                            return Ok(());
                        }
                        Err(reason) => return Err(reason),
                    };
                    self.graph.set_frame_dst(node, dst);
                    self.push_body(block, node);
                    self.def_register(dst, block, node, byte_pc);
                }
                // `Increment dst, src, delta` is `dst = src + delta` (delta is an
                // inline immediate, default 1; a negative delta is `--`). It
                // lowers exactly like `Add` of `src` and a constant step.
                Op::Increment => {
                    let dst = reg(&operands, 0)?;
                    let src = reg(&operands, 1)?;
                    let delta = match operands.get(2) {
                        Some(Operand::Imm32(v)) => *v,
                        None => 1,
                        _ => return Err(Unsupported::OperandShape("increment delta")),
                    };
                    let node = match self.increment(block, src, delta, feedback, byte_pc) {
                        Ok(node) => node,
                        Err(reason @ Unsupported::TypeFeedback(_)) => {
                            self.deopt_or_decline(block, byte_pc, reason)?;
                            return Ok(());
                        }
                        Err(reason) => return Err(reason),
                    };
                    self.graph.set_frame_dst(node, dst);
                    self.push_body(block, node);
                    self.def_register(dst, block, node, byte_pc);
                }
                Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Equal
                | Op::NotEqual
                | Op::LooseEqual
                | Op::LooseNotEqual => {
                    let (dst, lhs, rhs) =
                        (reg(&operands, 0)?, reg(&operands, 1)?, reg(&operands, 2)?);
                    // Loose `==` / `!=` share the relational lowering: against a
                    // nullish literal it becomes a null-or-undefined identity
                    // test, and on numeric feedback it is numeric comparison with
                    // the same operand guards (a non-number operand deopts).
                    let (cmp, loose) = match op {
                        Op::LooseEqual => (CmpOp::Eq, true),
                        Op::LooseNotEqual => (CmpOp::Ne, true),
                        other => (CmpOp::from_op(other).expect("comparison opcode"), false),
                    };
                    let node = match self.compare(block, cmp, lhs, rhs, feedback, loose, byte_pc) {
                        Ok(node) => node,
                        Err(reason @ Unsupported::TypeFeedback(_)) => {
                            self.deopt_or_decline(block, byte_pc, reason)?;
                            return Ok(());
                        }
                        Err(reason) => return Err(reason),
                    };
                    self.graph.set_frame_dst(node, dst);
                    self.push_body(block, node);
                    self.def_register(dst, block, node, byte_pc);
                }
                Op::Jump => {
                    let rel = imm32(&operands, 0)?;
                    let target = u32::try_from(branch_target(byte_pc, rel)).unwrap();
                    let tgt = self.cfg.block_of_pc[&target];
                    self.set_term(block, Terminator::Jump(tgt));
                }
                Op::JumpIfTrue | Op::JumpIfFalse => {
                    let rel = imm32(&operands, 0)?;
                    let cond = reg(&operands, 1)?;
                    let target = u32::try_from(branch_target(byte_pc, rel)).unwrap();
                    let tgt_block = self.cfg.block_of_pc[&target];
                    let succs = &self.cfg.succs[block as usize];
                    if succs.len() < 2 {
                        return Err(Unsupported::OperandShape(
                            "conditional branch without fallthrough",
                        ));
                    }
                    // succs == [target, fallthrough] (built in CFG discovery).
                    let fallthrough = succs[1];
                    let cond_node = self.read_variable(cond, block);
                    // Only an unboxed `Bool` condition (a comparison result) is
                    // compiled. A `Tagged` condition is a value tested for JS
                    // truthiness (`if (x)`, `while (obj)`, a `&&`/`||` result
                    // merged through a phi) — full ToBoolean is outside this tier,
                    // so bail rather than mis-evaluate it.
                    if self.graph.node(cond_node).kind.repr() != Repr::Bool
                        && !self.is_boxed_bool(cond_node, &mut FxHashSet::default())
                    {
                        return Err(Unsupported::OperandShape("non-boolean branch condition"));
                    }
                    let (on_true, on_false) = if matches!(op, Op::JumpIfTrue) {
                        (tgt_block, fallthrough)
                    } else {
                        (fallthrough, tgt_block)
                    };
                    self.set_term(
                        block,
                        Terminator::Branch {
                            cond: cond_node,
                            on_true,
                            on_false,
                        },
                    );
                }
                Op::Return | Op::ReturnValue => {
                    let src = reg(&operands, 0)?;
                    let node = self.read_variable(src, block);
                    self.set_term(block, Terminator::Return(node));
                }
                Op::ReturnUndefined => {
                    let node = self
                        .graph
                        .add_node(NodeKind::ConstUndefined, block, byte_pc);
                    // The node must be in the block body so the emitter
                    // materializes `undefined` into its home before the
                    // terminator reads it; an unlowered return value would box a
                    // garbage register.
                    self.push_body(block, node);
                    self.set_term(block, Terminator::Return(node));
                }
                // An opcode outside the optimizing subset. Deopt-to-interpreter
                // for it only pays off to compile (and OSR) a hot loop while
                // leaving the rest to the interpreter — so require the function to
                // have a loop, and the op to sit OUTSIDE every loop
                // (prologue / epilogue, e.g. `console.log` after the loop). A
                // deopt from inside a loop body could let an OSR'd loop reorder
                // side effects; and a loopless function compiled only to deopt
                // buys nothing. Either way, decline → baseline / interpreter.
                _ => {
                    self.deopt_or_decline(block, byte_pc, Unsupported::Opcode(op))?;
                    return Ok(());
                }
            }
        }
        // A block whose last instruction was not a terminator falls through to
        // its single successor.
        if self.graph.blocks[block as usize].term.is_none() {
            let next = *self.cfg.succs[block as usize]
                .first()
                .ok_or(Unsupported::OperandShape("fallthrough without successor"))?;
            self.set_term(block, Terminator::Jump(next));
        }
        Ok(())
    }

    /// Build a typed `Int32*` arithmetic node, speculating int32 operands from
    /// the site's feedback. Bails the whole function if the site is not
    /// int32-only.
    fn int32_binop(
        &mut self,
        block: BlockId,
        op: Op,
        lhs: u16,
        rhs: u16,
        feedback: u8,
        byte_pc: u32,
    ) -> Result<NodeId, Unsupported> {
        let l = self.int32_operand(block, lhs, feedback, byte_pc)?;
        let r = self.int32_operand(block, rhs, feedback, byte_pc)?;
        let kind = match op {
            Op::Add => NodeKind::Int32Add(l, r),
            Op::Sub => NodeKind::Int32Sub(l, r),
            Op::Mul => NodeKind::Int32Mul(l, r),
            _ => unreachable!("int32_binop on non-arithmetic op"),
        };
        Ok(self.graph.add_node(kind, block, byte_pc))
    }

    /// Build a typed `Int32*` bitwise / shift node, speculating int32 operands
    /// from the site's feedback (`CheckInt32`). Bails the whole function if the
    /// site is not int32-only.
    fn is_boxed_bool(&self, value: NodeId, visiting: &mut FxHashSet<NodeId>) -> bool {
        if !visiting.insert(value) {
            return false;
        }
        let node = self.graph.node(value);
        let result = match &node.kind {
            NodeKind::ConstBool(_) => true,
            NodeKind::Phi(inputs) => {
                let mut saw_value = false;
                let all_bool = inputs.iter().all(|&input| {
                    if visiting.contains(&input) {
                        return true;
                    }
                    saw_value = true;
                    self.is_boxed_bool(input, visiting)
                });
                saw_value && all_bool
            }
            _ => node.repr == Repr::Bool,
        };
        visiting.remove(&value);
        result
    }

    fn bitwise_binop(
        &mut self,
        block: BlockId,
        op: Op,
        lhs: u16,
        rhs: u16,
        feedback: u8,
        byte_pc: u32,
    ) -> Result<NodeId, Unsupported> {
        let fb = ArithFeedback::from_bits(feedback);
        if !fb.is_numeric_only() && !self.allow_empty_feedback_for_osr(fb) {
            return Err(Unsupported::TypeFeedback(feedback));
        }
        let l = self.int32_operand(block, lhs, feedback, byte_pc)?;
        let r = self.int32_operand(block, rhs, feedback, byte_pc)?;
        let kind = match op {
            Op::BitwiseOr => NodeKind::Int32BitOr(l, r),
            Op::BitwiseAnd => NodeKind::Int32BitAnd(l, r),
            Op::BitwiseXor => NodeKind::Int32BitXor(l, r),
            Op::Shl => NodeKind::Int32Shl(l, r),
            Op::Shr => NodeKind::Int32Shr(l, r),
            Op::Ushr => NodeKind::Int32UshrToFloat64(l, r),
            _ => unreachable!("bitwise_binop on non-bitwise op"),
        };
        Ok(self.graph.add_node(kind, block, byte_pc))
    }

    fn int32_node_operand(
        &mut self,
        block: BlockId,
        node: NodeId,
        raw_feedback: u8,
        byte_pc: u32,
    ) -> Result<NodeId, Unsupported> {
        let feedback = ArithFeedback::from_bits(raw_feedback);
        if !feedback.is_numeric_only() && !self.allow_empty_feedback_for_osr(feedback) {
            return Err(Unsupported::TypeFeedback(raw_feedback));
        }
        match self.graph.node(node).repr {
            Repr::Int32 => Ok(node),
            Repr::Float64 => {
                let truncate = self
                    .graph
                    .add_node(NodeKind::Float64ToInt32(node), block, byte_pc);
                self.push_body(block, truncate);
                Ok(truncate)
            }
            Repr::Tagged | Repr::Bool if feedback.is_int32_only() => {
                let check = self
                    .graph
                    .add_node(NodeKind::CheckInt32(node), block, byte_pc);
                self.push_body(block, check);
                Ok(check)
            }
            Repr::Tagged => {
                let number = self
                    .graph
                    .add_node(NodeKind::CheckNumber(node), block, byte_pc);
                self.push_body(block, number);
                let truncate =
                    self.graph
                        .add_node(NodeKind::Float64ToInt32(number), block, byte_pc);
                self.push_body(block, truncate);
                Ok(truncate)
            }
            Repr::Bool => Err(Unsupported::Unlowered("bitwise bool operand")),
        }
    }

    fn float_node_operand(&mut self, block: BlockId, node: NodeId, byte_pc: u32) -> NodeId {
        match self.graph.node(node).repr {
            Repr::Float64 => node,
            Repr::Int32 => {
                let widen = self
                    .graph
                    .add_node(NodeKind::Int32ToFloat64(node), block, byte_pc);
                self.push_body(block, widen);
                widen
            }
            Repr::Tagged | Repr::Bool => {
                let check = self
                    .graph
                    .add_node(NodeKind::CheckNumber(node), block, byte_pc);
                self.push_body(block, check);
                check
            }
        }
    }

    fn arith_node_binop(
        &mut self,
        block: BlockId,
        op: Op,
        lhs: NodeId,
        rhs: NodeId,
        feedback: u8,
        byte_pc: u32,
    ) -> Result<NodeId, Unsupported> {
        let fb = ArithFeedback::from_bits(feedback);
        let has_float_operand = self.node_is_float64(lhs) || self.node_is_float64(rhs);
        if fb.is_int32_only() && !has_float_operand && !matches!(op, Op::Div) {
            let l = self.int32_node_operand(block, lhs, feedback, byte_pc)?;
            let r = self.int32_node_operand(block, rhs, feedback, byte_pc)?;
            let kind = match op {
                Op::Add => NodeKind::Int32Add(l, r),
                Op::Sub => NodeKind::Int32Sub(l, r),
                Op::Mul => NodeKind::Int32Mul(l, r),
                _ => unreachable!("arith_node_binop on non-int arithmetic op"),
            };
            return Ok(self.graph.add_node(kind, block, byte_pc));
        }
        if !fb.is_numeric_only() && !self.allow_empty_feedback_for_osr(fb) {
            return Err(Unsupported::TypeFeedback(feedback));
        }
        let l = self.float_node_operand(block, lhs, byte_pc);
        let r = self.float_node_operand(block, rhs, byte_pc);
        let kind = match op {
            Op::Add => NodeKind::Float64Add(l, r),
            Op::Sub => NodeKind::Float64Sub(l, r),
            Op::Mul => NodeKind::Float64Mul(l, r),
            Op::Div => NodeKind::Float64Div(l, r),
            _ => unreachable!("arith_node_binop on non-arithmetic op"),
        };
        Ok(self.graph.add_node(kind, block, byte_pc))
    }

    fn bitwise_node_binop(
        &mut self,
        block: BlockId,
        op: Op,
        lhs: NodeId,
        rhs: NodeId,
        feedback: u8,
        byte_pc: u32,
    ) -> Result<NodeId, Unsupported> {
        let fb = ArithFeedback::from_bits(feedback);
        if !fb.is_numeric_only() && !self.allow_empty_feedback_for_osr(fb) {
            return Err(Unsupported::TypeFeedback(feedback));
        }
        let l = self.int32_node_operand(block, lhs, feedback, byte_pc)?;
        let r = self.int32_node_operand(block, rhs, feedback, byte_pc)?;
        let kind = match op {
            Op::BitwiseOr => NodeKind::Int32BitOr(l, r),
            Op::BitwiseAnd => NodeKind::Int32BitAnd(l, r),
            Op::BitwiseXor => NodeKind::Int32BitXor(l, r),
            Op::Shl => NodeKind::Int32Shl(l, r),
            Op::Shr => NodeKind::Int32Shr(l, r),
            Op::Ushr => NodeKind::Int32UshrToFloat64(l, r),
            _ => unreachable!("bitwise_node_binop on non-bitwise op"),
        };
        Ok(self.graph.add_node(kind, block, byte_pc))
    }

    fn inline_method_allows_store(method: &JitInlineMethod) -> bool {
        let mut store_seen = false;
        for instr in &method.instructions {
            if store_seen
                && !matches!(
                    instr.op,
                    Op::LoadThis
                        | Op::LoadInt32
                        | Op::LoadLocal
                        | Op::LoadUndefined
                        | Op::LoadHole
                        | Op::LoadTrue
                        | Op::LoadFalse
                        | Op::StoreLocal
                        | Op::Return
                        | Op::ReturnValue
                        | Op::ReturnUndefined
                )
            {
                return false;
            }
            if instr.op == Op::StoreProperty {
                store_seen = true;
            }
        }
        true
    }

    fn try_inline_direct_call(
        &mut self,
        block: BlockId,
        call_pc: u32,
        _callee_reg: u16,
        callee: &JitInlineCallee,
        inputs: &[NodeId],
    ) -> Result<Option<NodeId>, Unsupported> {
        const MAX_INLINE_CALL_REGS: u16 = 64;
        const MAX_INLINE_CALL_INSTRS: usize = 48;
        let argc = inputs.len().saturating_sub(1);
        if argc != usize::from(callee.param_count)
            || callee.register_count > MAX_INLINE_CALL_REGS
            || callee.instructions.len() > MAX_INLINE_CALL_INSTRS
            || !callee.instructions.iter().all(|instr| {
                matches!(
                    instr.op,
                    Op::MakeFunction
                        | Op::MakeClosure
                        | Op::LoadInt32
                        | Op::LoadNumber
                        | Op::LoadTrue
                        | Op::LoadFalse
                        | Op::LoadUndefined
                        | Op::LoadHole
                        | Op::LoadLocal
                        | Op::LoadUpvalue
                        | Op::StoreLocal
                        | Op::ToPrimitive
                        | Op::ToNumeric
                        | Op::Add
                        | Op::Sub
                        | Op::Mul
                        | Op::Div
                        | Op::BitwiseOr
                        | Op::BitwiseAnd
                        | Op::BitwiseXor
                        | Op::Shl
                        | Op::Shr
                        | Op::Ushr
                        | Op::Increment
                        | Op::Return
                        | Op::ReturnValue
                        | Op::ReturnUndefined
                )
            })
        {
            return Ok(None);
        }

        let start_nodes = self.graph.nodes.len();
        let start_body = self.graph.blocks[block as usize].body.len();
        macro_rules! decline_inline {
            () => {{
                self.graph.nodes.truncate(start_nodes);
                self.graph.blocks[block as usize].body.truncate(start_body);
                return Ok(None);
            }};
        }

        let Some(&callee_value) = inputs.first() else {
            return Ok(None);
        };
        let guard = self.graph.add_node(
            NodeKind::CheckFunctionIdentity {
                callee: callee_value,
                function_id: callee.function_id,
            },
            block,
            call_pc,
        );
        self.push_body(block, guard);

        let mut regs: Vec<Option<NodeId>> = vec![None; callee.register_count as usize];
        for (slot, &arg) in inputs.iter().skip(1).enumerate() {
            if slot < regs.len() {
                regs[slot] = Some(arg);
            }
        }
        let read = |regs: &[Option<NodeId>], regn: u16| -> Result<NodeId, Unsupported> {
            regs.get(regn as usize)
                .copied()
                .flatten()
                .ok_or(Unsupported::OperandShape("inline call register"))
        };
        let write =
            |regs: &mut [Option<NodeId>], regn: u16, node: NodeId| -> Result<(), Unsupported> {
                let Some(slot) = regs.get_mut(regn as usize) else {
                    return Err(Unsupported::OperandShape("inline call register"));
                };
                *slot = Some(node);
                Ok(())
            };
        macro_rules! read_inline {
            ($regn:expr) => {
                match read(&regs, $regn) {
                    Ok(node) => node,
                    Err(_) => decline_inline!(),
                }
            };
        }
        macro_rules! write_inline {
            ($regn:expr, $node:expr) => {
                if write(&mut regs, $regn, $node).is_err() {
                    decline_inline!();
                }
            };
        }

        let mut returned = None;
        for instr in &callee.instructions {
            let op = instr.op;
            let operands = instr.operands.as_slice();
            match op {
                // The callee's own self-name binding (`function f(){…}` makes `f`
                // visible in its body). Bind the dst register to the guarded
                // callee value so a following `StoreLocal`/`LoadLocal` of the name
                // resolves instead of declining the inline.
                Op::MakeFunction | Op::MakeClosure if instr.make_self => {
                    let dst = reg(operands, 0)?;
                    write_inline!(dst, callee_value);
                }
                Op::LoadInt32 => {
                    let dst = reg(operands, 0)?;
                    let value = imm32(operands, 1)?;
                    let node = self
                        .graph
                        .add_node(NodeKind::ConstInt32(value), block, call_pc);
                    self.push_body(block, node);
                    write_inline!(dst, node);
                }
                Op::LoadNumber => {
                    let dst = reg(operands, 0)?;
                    let Some(value) = instr.load_number else {
                        decline_inline!();
                    };
                    let node = self
                        .graph
                        .add_node(NodeKind::ConstF64(value), block, call_pc);
                    self.push_body(block, node);
                    write_inline!(dst, node);
                }
                Op::LoadTrue | Op::LoadFalse => {
                    let dst = reg(operands, 0)?;
                    let node = self.graph.add_node(
                        NodeKind::ConstBool(matches!(op, Op::LoadTrue)),
                        block,
                        call_pc,
                    );
                    self.push_body(block, node);
                    write_inline!(dst, node);
                }
                Op::LoadUndefined | Op::LoadHole => {
                    let dst = reg(operands, 0)?;
                    let kind = if matches!(op, Op::LoadUndefined) {
                        NodeKind::ConstUndefined
                    } else {
                        NodeKind::LoadHole
                    };
                    let node = self.graph.add_node(kind, block, call_pc);
                    self.push_body(block, node);
                    write_inline!(dst, node);
                }
                Op::LoadLocal => {
                    let dst = reg(operands, 0)?;
                    let src = u16::try_from(imm32(operands, 1)?)
                        .map_err(|_| Unsupported::OperandShape("inline local index"))?;
                    let node = read_inline!(src);
                    write_inline!(dst, node);
                }
                // A captured binding of the inlined closure: read from the
                // callee's own spine via the fid-guarded callee value, not the
                // running context. `cage_base == 0` (no live IC tables) can't
                // decode the closure body, so decline the splice.
                Op::LoadUpvalue => {
                    let dst = reg(operands, 0)?;
                    let idx = imm32(operands, 1)?;
                    if idx < 0 || self.view.cage_base == 0 {
                        decline_inline!();
                    }
                    let node = self.graph.add_node(
                        NodeKind::InlineUpvalue {
                            closure: callee_value,
                            index: idx as u32,
                        },
                        block,
                        call_pc,
                    );
                    self.push_body(block, node);
                    write_inline!(dst, node);
                }
                Op::StoreLocal => {
                    let src = reg(operands, 0)?;
                    let dst = u16::try_from(imm32(operands, 1)?)
                        .map_err(|_| Unsupported::OperandShape("inline local index"))?;
                    let node = read_inline!(src);
                    write_inline!(dst, node);
                }
                Op::ToPrimitive | Op::ToNumeric => {
                    let dst = reg(operands, 0)?;
                    let src = reg(operands, 1)?;
                    let node = read_inline!(src);
                    write_inline!(dst, node);
                }
                Op::Add | Op::Sub | Op::Mul | Op::Div => {
                    let dst = reg(operands, 0)?;
                    let lhs = read_inline!(reg(operands, 1)?);
                    let rhs = read_inline!(reg(operands, 2)?);
                    let node =
                        self.arith_node_binop(block, op, lhs, rhs, instr.arith_feedback, call_pc)?;
                    self.push_body(block, node);
                    write_inline!(dst, node);
                }
                Op::BitwiseOr | Op::BitwiseAnd | Op::BitwiseXor | Op::Shl | Op::Shr | Op::Ushr => {
                    let dst = reg(operands, 0)?;
                    let lhs = read_inline!(reg(operands, 1)?);
                    let rhs = read_inline!(reg(operands, 2)?);
                    let node = self.bitwise_node_binop(
                        block,
                        op,
                        lhs,
                        rhs,
                        instr.arith_feedback,
                        call_pc,
                    )?;
                    self.push_body(block, node);
                    write_inline!(dst, node);
                }
                Op::Increment => {
                    let dst = reg(operands, 0)?;
                    let src = read_inline!(reg(operands, 1)?);
                    let delta = match operands.get(2) {
                        Some(Operand::Imm32(v)) => *v,
                        None => 1,
                        _ => return Err(Unsupported::OperandShape("increment delta")),
                    };
                    let step = self
                        .graph
                        .add_node(NodeKind::ConstInt32(delta), block, call_pc);
                    self.push_body(block, step);
                    let node = self.arith_node_binop(
                        block,
                        Op::Add,
                        src,
                        step,
                        instr.arith_feedback,
                        call_pc,
                    )?;
                    self.push_body(block, node);
                    write_inline!(dst, node);
                }
                Op::Return | Op::ReturnValue => {
                    let src = reg(operands, 0)?;
                    returned = Some(read_inline!(src));
                    break;
                }
                Op::ReturnUndefined => {
                    let node = self
                        .graph
                        .add_node(NodeKind::ConstUndefined, block, call_pc);
                    self.push_body(block, node);
                    returned = Some(node);
                    break;
                }
                _ => decline_inline!(),
            }
        }

        if returned.is_none() {
            self.graph.nodes.truncate(start_nodes);
            self.graph.blocks[block as usize].body.truncate(start_body);
        }
        Ok(returned)
    }

    fn try_inline_method(
        &mut self,
        block: BlockId,
        call_pc: u32,
        recv: NodeId,
        method: &JitInlineMethod,
        argc: usize,
        args: &[NodeId],
    ) -> Result<Option<NodeId>, Unsupported> {
        const MAX_INLINE_METHOD_REGS: u16 = 64;
        const MAX_INLINE_METHOD_INSTRS: usize = 64;
        if self.view.cage_base == 0
            || argc != usize::from(method.param_count)
            || method.register_count > MAX_INLINE_METHOD_REGS
            || method.instructions.len() > MAX_INLINE_METHOD_INSTRS
            || !Self::inline_method_allows_store(method)
        {
            return Ok(None);
        }
        let start_nodes = self.graph.nodes.len();
        let start_body = self.graph.blocks[block as usize].body.len();
        macro_rules! decline_inline {
            () => {{
                self.graph.nodes.truncate(start_nodes);
                self.graph.blocks[block as usize].body.truncate(start_body);
                return Ok(None);
            }};
        }

        let checked = self.graph.add_node(
            NodeKind::CheckMethodIdentity {
                recv,
                recv_shape: method.recv_shape,
                proto_shape: method.proto_shape,
                method_value_byte: method.method_value_byte,
                method_on_receiver: method.method_on_receiver,
                method_fid: method.method_fid,
            },
            block,
            call_pc,
        );
        self.push_body(block, checked);

        let mut regs: Vec<Option<NodeId>> = vec![None; method.register_count as usize];
        for (slot, &arg) in args.iter().enumerate() {
            if slot < regs.len() {
                regs[slot] = Some(arg);
            }
        }
        let mut returned = None;

        for instr in &method.instructions {
            let op = instr.op;
            let operands = instr.operands.as_slice();
            let read = |regs: &[Option<NodeId>], regn: u16| -> Result<NodeId, Unsupported> {
                regs.get(regn as usize)
                    .copied()
                    .flatten()
                    .ok_or(Unsupported::OperandShape("inline method register"))
            };
            let write =
                |regs: &mut [Option<NodeId>], regn: u16, node: NodeId| -> Result<(), Unsupported> {
                    let Some(slot) = regs.get_mut(regn as usize) else {
                        return Err(Unsupported::OperandShape("inline method register"));
                    };
                    *slot = Some(node);
                    Ok(())
                };
            match op {
                Op::LoadThis => {
                    let dst = reg(operands, 0)?;
                    write(&mut regs, dst, checked)?;
                }
                Op::LoadInt32 => {
                    let dst = reg(operands, 0)?;
                    let value = imm32(operands, 1)?;
                    let node = self
                        .graph
                        .add_node(NodeKind::ConstInt32(value), block, call_pc);
                    self.push_body(block, node);
                    write(&mut regs, dst, node)?;
                }
                Op::LoadNumber => {
                    let dst = reg(operands, 0)?;
                    let Some(value) = instr.load_number else {
                        decline_inline!();
                    };
                    let node = self
                        .graph
                        .add_node(NodeKind::ConstF64(value), block, call_pc);
                    self.push_body(block, node);
                    write(&mut regs, dst, node)?;
                }
                Op::LoadTrue | Op::LoadFalse => {
                    let dst = reg(operands, 0)?;
                    let node = self.graph.add_node(
                        NodeKind::ConstBool(matches!(op, Op::LoadTrue)),
                        block,
                        call_pc,
                    );
                    self.push_body(block, node);
                    write(&mut regs, dst, node)?;
                }
                Op::LoadUndefined | Op::LoadHole => {
                    let dst = reg(operands, 0)?;
                    let kind = if matches!(op, Op::LoadUndefined) {
                        NodeKind::ConstUndefined
                    } else {
                        NodeKind::LoadHole
                    };
                    let node = self.graph.add_node(kind, block, call_pc);
                    self.push_body(block, node);
                    write(&mut regs, dst, node)?;
                }
                Op::LoadLocal => {
                    let dst = reg(operands, 0)?;
                    let src = u16::try_from(imm32(operands, 1)?)
                        .map_err(|_| Unsupported::OperandShape("inline local index"))?;
                    let node = read(&regs, src)?;
                    write(&mut regs, dst, node)?;
                }
                Op::StoreLocal => {
                    let src = reg(operands, 0)?;
                    let dst = u16::try_from(imm32(operands, 1)?)
                        .map_err(|_| Unsupported::OperandShape("inline local index"))?;
                    let node = read(&regs, src)?;
                    write(&mut regs, dst, node)?;
                }
                Op::ToPrimitive | Op::ToNumeric => {
                    let dst = reg(operands, 0)?;
                    let src = reg(operands, 1)?;
                    let node = read(&regs, src)?;
                    write(&mut regs, dst, node)?;
                }
                Op::LoadProperty => {
                    let dst = reg(operands, 0)?;
                    let obj_reg = reg(operands, 1)?;
                    let obj = read(&regs, obj_reg)?;
                    let Some(&slot_byte) = method.prop_offsets.get(&instr.byte_pc) else {
                        decline_inline!();
                    };
                    let checked_obj = self.graph.add_node(
                        NodeKind::CheckShape(obj, method.recv_shape),
                        block,
                        call_pc,
                    );
                    self.push_body(block, checked_obj);
                    let load = self.graph.add_node(
                        NodeKind::LoadSlot(checked_obj, slot_byte),
                        block,
                        call_pc,
                    );
                    self.push_body(block, load);
                    write(&mut regs, dst, load)?;
                }
                Op::StoreProperty => {
                    let obj_reg = reg(operands, 0)?;
                    let src_reg = reg(operands, 2)?;
                    let obj = read(&regs, obj_reg)?;
                    let value = read(&regs, src_reg)?;
                    if !matches!(self.graph.node(value).repr, Repr::Int32 | Repr::Float64) {
                        decline_inline!();
                    }
                    let Some(&slot_byte) = method.prop_offsets.get(&instr.byte_pc) else {
                        decline_inline!();
                    };
                    let checked_obj = self.graph.add_node(
                        NodeKind::CheckShape(obj, method.recv_shape),
                        block,
                        call_pc,
                    );
                    self.push_body(block, checked_obj);
                    let store = self.graph.add_node(
                        NodeKind::StoreSlot(checked_obj, slot_byte, value),
                        block,
                        call_pc,
                    );
                    self.push_body(block, store);
                }
                Op::Add | Op::Sub | Op::Mul | Op::Div => {
                    let dst = reg(operands, 0)?;
                    let lhs = read(&regs, reg(operands, 1)?)?;
                    let rhs = read(&regs, reg(operands, 2)?)?;
                    let node =
                        self.arith_node_binop(block, op, lhs, rhs, instr.arith_feedback, call_pc)?;
                    self.push_body(block, node);
                    write(&mut regs, dst, node)?;
                }
                Op::BitwiseOr | Op::BitwiseAnd | Op::BitwiseXor | Op::Shl | Op::Shr | Op::Ushr => {
                    let dst = reg(operands, 0)?;
                    let lhs = read(&regs, reg(operands, 1)?)?;
                    let rhs = read(&regs, reg(operands, 2)?)?;
                    let node = self.bitwise_node_binop(
                        block,
                        op,
                        lhs,
                        rhs,
                        instr.arith_feedback,
                        call_pc,
                    )?;
                    self.push_body(block, node);
                    write(&mut regs, dst, node)?;
                }
                Op::Increment => {
                    let dst = reg(operands, 0)?;
                    let src = read(&regs, reg(operands, 1)?)?;
                    let delta = match operands.get(2) {
                        Some(Operand::Imm32(v)) => *v,
                        None => 1,
                        _ => return Err(Unsupported::OperandShape("increment delta")),
                    };
                    let step = self
                        .graph
                        .add_node(NodeKind::ConstInt32(delta), block, call_pc);
                    self.push_body(block, step);
                    let node = self.arith_node_binop(
                        block,
                        Op::Add,
                        src,
                        step,
                        instr.arith_feedback,
                        call_pc,
                    )?;
                    self.push_body(block, node);
                    write(&mut regs, dst, node)?;
                }
                Op::Return | Op::ReturnValue => {
                    let src = reg(operands, 0)?;
                    returned = Some(read(&regs, src)?);
                    break;
                }
                Op::ReturnUndefined => {
                    let node = self
                        .graph
                        .add_node(NodeKind::ConstUndefined, block, call_pc);
                    self.push_body(block, node);
                    returned = Some(node);
                    break;
                }
                _ => decline_inline!(),
            }
        }

        if returned.is_none() {
            self.graph.nodes.truncate(start_nodes);
            self.graph.blocks[block as usize].body.truncate(start_body);
        }
        Ok(returned)
    }

    /// Lower an `Add` / `Sub` / `Mul` / `Div` site to a typed arithmetic node,
    /// picking the representation from the site's operand feedback:
    ///
    /// - `is_int32_only` (and not `Div`, whose int32 operands still yield a
    ///   non-integer result) → an unboxed `Int32*` node guarded by `CheckInt32`.
    /// - otherwise `is_numeric_only` → a `Float64*` node whose operands are
    ///   widened to `f64` (`CheckNumber` on a tagged operand, `Int32ToFloat64`
    ///   on an already-unboxed int).
    /// - neither → bail (the site has seen a string / bigint / object).
    fn arith_binop(
        &mut self,
        block: BlockId,
        op: Op,
        lhs: u16,
        rhs: u16,
        feedback: u8,
        byte_pc: u32,
    ) -> Result<NodeId, Unsupported> {
        let fb = ArithFeedback::from_bits(feedback);
        // `Div` of two int32s is generally non-integer (`5/2`), so it always
        // takes the float path; the other three stay int32 when the operands are.
        let has_float_operand =
            self.operand_is_float64(block, lhs) || self.operand_is_float64(block, rhs);
        if fb.is_int32_only() && !has_float_operand && !matches!(op, Op::Div) {
            return self.int32_binop(block, op, lhs, rhs, feedback, byte_pc);
        }
        if !fb.is_numeric_only() && !self.allow_empty_feedback_for_osr(fb) {
            return Err(Unsupported::TypeFeedback(feedback));
        }
        let l = self.float_operand(block, lhs, byte_pc);
        let r = self.float_operand(block, rhs, byte_pc);
        let kind = match op {
            Op::Add => NodeKind::Float64Add(l, r),
            Op::Sub => NodeKind::Float64Sub(l, r),
            Op::Mul => NodeKind::Float64Mul(l, r),
            Op::Div => NodeKind::Float64Div(l, r),
            _ => unreachable!("arith_binop on non-arithmetic op"),
        };
        Ok(self.graph.add_node(kind, block, byte_pc))
    }

    /// Lower a relational / equality site, mirroring [`Self::arith_binop`]'s
    /// representation choice: an int32-only site compares unboxed int32s, an
    /// otherwise-numeric site compares `f64`s, anything else bails.
    fn compare(
        &mut self,
        block: BlockId,
        cmp: CmpOp,
        lhs: u16,
        rhs: u16,
        feedback: u8,
        loose: bool,
        byte_pc: u32,
    ) -> Result<NodeId, Unsupported> {
        let lhs_node = self.read_variable(lhs, block);
        let rhs_node = self.read_variable(rhs, block);
        // A strict (`===`) comparison only collapses against the `null` literal;
        // loose (`==`) also collapses against `undefined`, since both nullish
        // literals make the site a null-or-undefined identity test.
        let is_nullish_lit = |kind: &NodeKind| {
            matches!(kind, NodeKind::ConstNull)
                || (loose && matches!(kind, NodeKind::ConstUndefined))
        };
        let lhs_is_null = is_nullish_lit(&self.graph.node(lhs_node).kind);
        let rhs_is_null = is_nullish_lit(&self.graph.node(rhs_node).kind);
        if matches!(cmp, CmpOp::Eq | CmpOp::Ne) && (lhs_is_null || rhs_is_null) {
            if lhs_is_null && rhs_is_null {
                return Ok(self.graph.add_node(
                    NodeKind::ConstBool(matches!(cmp, CmpOp::Eq)),
                    block,
                    byte_pc,
                ));
            }
            let value = if lhs_is_null { rhs_node } else { lhs_node };
            return Ok(self.graph.add_node(
                NodeKind::TaggedIsNull {
                    value,
                    negate: matches!(cmp, CmpOp::Ne),
                    nullish: loose,
                },
                block,
                byte_pc,
            ));
        }
        let fb = ArithFeedback::from_bits(feedback);
        let has_float_operand = self.node_is_float64(lhs_node) || self.node_is_float64(rhs_node);
        if fb.is_int32_only() && !has_float_operand {
            let l = self.int32_operand(block, lhs, feedback, byte_pc)?;
            let r = self.int32_operand(block, rhs, feedback, byte_pc)?;
            return Ok(self
                .graph
                .add_node(NodeKind::Int32Compare(cmp, l, r), block, byte_pc));
        }
        if !fb.is_numeric_only() && !self.allow_empty_feedback_for_osr(fb) {
            return Err(Unsupported::TypeFeedback(feedback));
        }
        let l = self.float_operand(block, lhs, byte_pc);
        let r = self.float_operand(block, rhs, byte_pc);
        Ok(self
            .graph
            .add_node(NodeKind::Float64Compare(cmp, l, r), block, byte_pc))
    }

    /// Lower an `Increment` (`dst = src + delta`) to a typed add of `src` and a
    /// constant step, mirroring [`Self::arith_binop`]'s int32-vs-float choice
    /// from the site feedback.
    fn increment(
        &mut self,
        block: BlockId,
        src: u16,
        delta: i32,
        feedback: u8,
        byte_pc: u32,
    ) -> Result<NodeId, Unsupported> {
        let fb = ArithFeedback::from_bits(feedback);
        let has_float_operand = self.operand_is_float64(block, src);
        if (fb.is_int32_only() || self.allow_empty_feedback_for_osr(fb)) && !has_float_operand {
            let s = self.int32_operand(block, src, feedback, byte_pc)?;
            let step = self
                .graph
                .add_node(NodeKind::ConstInt32(delta), block, byte_pc);
            self.push_body(block, step);
            return Ok(self
                .graph
                .add_node(NodeKind::Int32Add(s, step), block, byte_pc));
        }
        if !fb.is_numeric_only() && !self.allow_empty_feedback_for_osr(fb) {
            return Err(Unsupported::TypeFeedback(feedback));
        }
        let s = self.float_operand(block, src, byte_pc);
        let step = self
            .graph
            .add_node(NodeKind::ConstF64(f64::from(delta)), block, byte_pc);
        self.push_body(block, step);
        Ok(self
            .graph
            .add_node(NodeKind::Float64Add(s, step), block, byte_pc))
    }

    /// Resolve a register operand to an [`Repr::Int32`] node for integer
    /// arithmetic. Int32-only sites keep the cheaper `CheckInt32` guard; numeric
    /// sites that already widened to float perform JavaScript `ToInt32` instead.
    fn int32_operand(
        &mut self,
        block: BlockId,
        operand_reg: u16,
        raw_feedback: u8,
        byte_pc: u32,
    ) -> Result<NodeId, Unsupported> {
        let feedback = ArithFeedback::from_bits(raw_feedback);
        if !feedback.is_numeric_only() && !self.allow_empty_feedback_for_osr(feedback) {
            return Err(Unsupported::TypeFeedback(raw_feedback));
        }
        let node = self.read_variable(operand_reg, block);
        match self.graph.node(node).repr {
            Repr::Int32 => Ok(node),
            Repr::Float64 => {
                let truncate = self
                    .graph
                    .add_node(NodeKind::Float64ToInt32(node), block, byte_pc);
                self.push_body(block, truncate);
                Ok(truncate)
            }
            Repr::Tagged | Repr::Bool if feedback.is_int32_only() => {
                let check = self
                    .graph
                    .add_node(NodeKind::CheckInt32(node), block, byte_pc);
                self.push_body(block, check);
                Ok(check)
            }
            Repr::Tagged => {
                let number = self
                    .graph
                    .add_node(NodeKind::CheckNumber(node), block, byte_pc);
                self.push_body(block, number);
                let truncate =
                    self.graph
                        .add_node(NodeKind::Float64ToInt32(number), block, byte_pc);
                self.push_body(block, truncate);
                Ok(truncate)
            }
            Repr::Bool => Err(Unsupported::Unlowered("bitwise bool operand")),
        }
    }

    /// Resolve a register operand to an [`Repr::Float64`] node for a numeric
    /// site. An already-`Float64` value is used directly; an unboxed `Int32` is
    /// widened with `Int32ToFloat64`; any other (tagged) value is unboxed by a
    /// `CheckNumber` guard, which deopts on a non-number. The caller establishes
    /// `is_numeric_only` before calling, so the guard's speculation is sound.
    fn float_operand(&mut self, block: BlockId, operand_reg: u16, byte_pc: u32) -> NodeId {
        let node = self.read_variable(operand_reg, block);
        match self.graph.node(node).repr {
            Repr::Float64 => node,
            Repr::Int32 => {
                let widen = self
                    .graph
                    .add_node(NodeKind::Int32ToFloat64(node), block, byte_pc);
                self.push_body(block, widen);
                widen
            }
            Repr::Tagged | Repr::Bool => {
                let check = self
                    .graph
                    .add_node(NodeKind::CheckNumber(node), block, byte_pc);
                self.push_body(block, check);
                check
            }
        }
    }

    /// Resolve a computed-element index to unboxed int32. This guard is not
    /// tied to arithmetic feedback: non-int32 indexes are valid JavaScript, but
    /// outside the inline element fast path, so the guard deopts and the
    /// interpreter handles property-key coercion.
    fn int32_index_operand(
        &mut self,
        block: BlockId,
        operand_reg: u16,
        byte_pc: u32,
    ) -> Result<NodeId, Unsupported> {
        let node = self.read_variable(operand_reg, block);
        match self.graph.node(node).repr {
            Repr::Int32 => Ok(node),
            Repr::Tagged => {
                let check = self
                    .graph
                    .add_node(NodeKind::CheckInt32(node), block, byte_pc);
                self.push_body(block, check);
                Ok(check)
            }
            Repr::Float64 | Repr::Bool => Err(Unsupported::OperandShape("element index repr")),
        }
    }

    fn push_body(&mut self, block: BlockId, node: NodeId) {
        self.graph.blocks[block as usize].body.push(node);
    }

    fn set_term(&mut self, block: BlockId, term: Terminator) {
        self.graph.blocks[block as usize].term = Some(term);
    }

    fn back_edge_pc(&self, header: &super::ir::Block) -> Option<u32> {
        header
            .preds
            .iter()
            .map(|&p| self.graph.blocks[p as usize].start_pc)
            .filter(|&pc| pc > header.start_pc)
            .max()
    }

    fn can_deopt_at(&self, byte_pc: u32) -> bool {
        let has_loop = self
            .graph
            .blocks
            .iter()
            .any(|header| self.back_edge_pc(header).is_some());
        let in_loop = self.graph.blocks.iter().any(|header| {
            matches!(
                self.back_edge_pc(header),
                Some(backedge_pc) if header.start_pc <= byte_pc && byte_pc <= backedge_pc
            )
        });
        has_loop && !in_loop
    }

    fn block_is_in_loop(&self, block: BlockId) -> bool {
        let block_start = self.graph.blocks[block as usize].start_pc;
        self.graph.blocks.iter().any(|header| {
            matches!(
                self.back_edge_pc(header),
                Some(backedge_pc) if header.start_pc <= block_start && block_start <= backedge_pc
            )
        })
    }

    fn deopt_or_decline(
        &mut self,
        block: BlockId,
        byte_pc: u32,
        reason: Unsupported,
    ) -> Result<(), Unsupported> {
        if self.block_is_in_loop(block) {
            return Err(reason);
        }
        if !self.can_deopt_at(byte_pc) {
            return Err(reason);
        }
        self.set_term(block, Terminator::Deopt(byte_pc));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm::jit_feedback::{ARITH_FLOAT64, ARITH_INT32};

    const STRIDE: u32 = 4;

    /// Branch encoding: target = branch_byte_pc + 1 + rel, with branch at
    /// `from * STRIDE` and target at `to * STRIDE`.
    fn rel(from: usize, to: usize) -> i32 {
        (to as i32 - from as i32) * STRIDE as i32 - 1
    }

    /// Build a `JitFunctionView` from `(op, operands, arith_feedback)` triples,
    /// assigning byte-PCs at a fixed stride.
    fn view(
        param_count: u16,
        register_count: u16,
        instrs: &[(Op, Vec<Operand>, u8)],
    ) -> JitFunctionView {
        let instructions = instrs
            .iter()
            .enumerate()
            .map(|(idx, (op, operands, fb))| otter_vm::JitInstrView {
                op: *op,
                byte_pc: idx as u32 * STRIDE,
                byte_len: STRIDE,
                property_ic_site: None,
                operands: operands.clone(),
                make_self: false,
                load_array_length: false,
                load_number: None,
                property_feedback: None,
                object_literal: None,
                arith_feedback: *fb,
            })
            .collect();
        JitFunctionView {
            function_id: 0,
            param_count,
            register_count,
            code_byte_len: instrs.len() as u32 * STRIDE,
            is_strict: true,
            is_async: false,
            is_generator: false,
            is_async_generator: false,
            cage_base: 0,
            ta_layout: otter_vm::JitTypedArrayLayout::default(),
            object_shape_byte: 8,
            object_values_ptr_byte: 16,
            gc_barrier: Default::default(),
            jit_proto_byte: 12,
            closure_fid_byte: 8,
            closure_upvalues_ptr_byte: 16,
            collection_layout: Default::default(),
            native_static_fn_byte: 0,
            instructions,
            inline_callees: Default::default(),
            inline_methods: Default::default(),
            inline_poly_methods: Default::default(),
            collection_leaf_methods: Default::default(),
            collection_alloc_methods: Default::default(),
            array_methods: Default::default(),
            safepoints: Default::default(),
        }
    }

    fn r(n: u16) -> Operand {
        Operand::Register(n)
    }
    fn imm(n: i32) -> Operand {
        Operand::Imm32(n)
    }

    fn count_kind(g: &Graph, pred: impl Fn(&NodeKind) -> bool) -> usize {
        g.nodes.iter().filter(|n| pred(&n.kind)).count()
    }

    fn build_full(view: &JitFunctionView) -> Result<Graph, Unsupported> {
        build(view, None)
    }

    #[test]
    fn monomorphic_tiny_direct_call_inlines_into_graph() {
        let mut v = view(
            3,
            4,
            &[
                (
                    Op::Call,
                    vec![r(3), r(0), Operand::ConstIndex(2), r(1), r(2)],
                    0,
                ),
                (Op::ReturnValue, vec![r(3)], 0),
            ],
        );
        v.inline_callees.insert(
            0,
            JitInlineCallee {
                function_id: 42,
                param_count: 2,
                register_count: 4,
                instructions: vec![
                    otter_vm::JitInstrView {
                        op: Op::Add,
                        byte_pc: 0,
                        byte_len: STRIDE,
                        property_ic_site: None,
                        operands: vec![r(2), r(0), r(1)],
                        make_self: false,
                        load_array_length: false,
                        load_number: None,
                        property_feedback: None,
                        object_literal: None,
                        arith_feedback: ARITH_INT32,
                    },
                    otter_vm::JitInstrView {
                        op: Op::ReturnValue,
                        byte_pc: STRIDE,
                        byte_len: STRIDE,
                        property_ic_site: None,
                        operands: vec![r(2)],
                        make_self: false,
                        load_array_length: false,
                        load_number: None,
                        property_feedback: None,
                        object_literal: None,
                        arith_feedback: 0,
                    },
                ],
            },
        );

        let g = build_full(&v).expect("direct call inlines");
        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::Call { .. })), 0);
        assert_eq!(
            count_kind(&g, |k| matches!(k, NodeKind::CheckFunctionIdentity { .. })),
            1
        );
        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::Int32Add(_, _))), 1);
    }

    #[test]
    fn monomorphic_tiny_method_inlines_into_graph() {
        let method_load_pc = 4;
        let mut prop_offsets = FxHashMap::default();
        prop_offsets.insert(method_load_pc, 0);
        let method = JitInlineMethod {
            method_fid: 42,
            recv_shape: 100,
            proto_shape: 200,
            method_value_byte: 8,
            method_on_receiver: false,
            param_count: 0,
            register_count: 2,
            instructions: vec![
                otter_vm::JitInstrView {
                    op: Op::LoadThis,
                    byte_pc: 0,
                    byte_len: STRIDE,
                    property_ic_site: None,
                    operands: vec![r(0)],
                    make_self: false,
                    load_array_length: false,
                    load_number: None,
                    property_feedback: None,
                    object_literal: None,
                    arith_feedback: 0,
                },
                otter_vm::JitInstrView {
                    op: Op::LoadProperty,
                    byte_pc: method_load_pc,
                    byte_len: STRIDE,
                    property_ic_site: None,
                    operands: vec![r(1), r(0), Operand::ConstIndex(0)],
                    make_self: false,
                    load_array_length: false,
                    load_number: None,
                    property_feedback: None,
                    object_literal: None,
                    arith_feedback: 0,
                },
                otter_vm::JitInstrView {
                    op: Op::ReturnValue,
                    byte_pc: 8,
                    byte_len: STRIDE,
                    property_ic_site: None,
                    operands: vec![r(1)],
                    make_self: false,
                    load_array_length: false,
                    load_number: None,
                    property_feedback: None,
                    object_literal: None,
                    arith_feedback: 0,
                },
            ],
            prop_offsets,
        };
        let mut v = view(
            1,
            3,
            &[
                (
                    Op::CallMethodValue,
                    vec![r(1), r(0), Operand::ConstIndex(0), Operand::ConstIndex(0)],
                    0,
                ),
                (Op::ReturnValue, vec![r(1)], 0),
            ],
        );
        v.cage_base = 1;
        v.instructions[0].property_ic_site = Some(7);
        v.inline_methods.insert(0, method);

        let g = build_full(&v).expect("inline method builds");
        assert_eq!(
            count_kind(&g, |k| matches!(k, NodeKind::CallMethod { .. })),
            0
        );
        assert_eq!(
            count_kind(&g, |k| matches!(k, NodeKind::CheckMethodIdentity { .. })),
            1
        );
        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::LoadSlot(_, _))), 1);
    }

    #[test]
    fn missing_inline_method_lowers_to_runtime_method_call() {
        let mut v = view(
            1,
            3,
            &[
                (
                    Op::CallMethodValue,
                    vec![r(1), r(0), Operand::ConstIndex(0), Operand::ConstIndex(0)],
                    0,
                ),
                (Op::ReturnValue, vec![r(1)], 0),
            ],
        );
        v.instructions[0].property_ic_site = Some(7);

        let g = build_full(&v).expect("generic method call builds");
        assert_eq!(
            count_kind(&g, |k| matches!(k, NodeKind::CallMethod { .. })),
            1
        );
        assert_eq!(
            count_kind(&g, |k| matches!(k, NodeKind::CheckMethodIdentity { .. })),
            0
        );
    }

    /// Build a callee `JitInstrView` for an inline-candidate body.
    fn cinstr(op: Op, operands: Vec<Operand>, make_self: bool, fb: u8) -> otter_vm::JitInstrView {
        otter_vm::JitInstrView {
            op,
            byte_pc: 0,
            byte_len: STRIDE,
            property_ic_site: None,
            operands,
            make_self,
            load_array_length: false,
            load_number: None,
            property_feedback: None,
            object_literal: None,
            arith_feedback: fb,
        }
    }

    #[test]
    fn direct_call_inline_binds_callee_self_name() {
        // The callee's leading self-name binding (`MakeFunction make_self` +
        // `StoreLocal` of that register) must not abort the splice: a value has
        // to flow into the self register so the store resolves.
        let mut v = view(
            1,
            3,
            &[
                (Op::Call, vec![r(2), r(0), Operand::ConstIndex(1), r(1)], 0),
                (Op::ReturnValue, vec![r(2)], 0),
            ],
        );
        v.inline_callees.insert(
            0,
            JitInlineCallee {
                function_id: 7,
                param_count: 1,
                register_count: 4,
                instructions: vec![
                    cinstr(
                        Op::MakeFunction,
                        vec![r(3), Operand::ConstIndex(0)],
                        true,
                        0,
                    ),
                    cinstr(Op::StoreLocal, vec![r(3), imm(2)], false, 0),
                    cinstr(Op::ReturnValue, vec![r(0)], false, 0),
                ],
            },
        );

        let g = build_full(&v).expect("self-binding callee inlines");
        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::Call { .. })), 0);
        assert_eq!(
            count_kind(&g, |k| matches!(k, NodeKind::CheckFunctionIdentity { .. })),
            1
        );
    }

    #[test]
    fn closure_callee_inlines_upvalue_read() {
        // A monomorphic closure callee (`return x * k`, `k` captured) splices its
        // body: the captured read becomes an `InlineUpvalue` off the fid-guarded
        // callee value, never a runtime `Call`.
        let mut v = view(
            1,
            3,
            &[
                (Op::Call, vec![r(2), r(0), Operand::ConstIndex(1), r(1)], 0),
                (Op::ReturnValue, vec![r(2)], 0),
            ],
        );
        v.cage_base = 1;
        v.inline_callees.insert(
            0,
            JitInlineCallee {
                function_id: 11,
                param_count: 1,
                register_count: 3,
                instructions: vec![
                    cinstr(Op::LoadUpvalue, vec![r(2), imm(0)], false, 0),
                    cinstr(Op::Mul, vec![r(2), r(0), r(2)], false, ARITH_INT32),
                    cinstr(Op::ReturnValue, vec![r(2)], false, 0),
                ],
            },
        );

        let g = build_full(&v).expect("closure callee inlines");
        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::Call { .. })), 0);
        assert_eq!(
            count_kind(&g, |k| matches!(k, NodeKind::InlineUpvalue { .. })),
            1
        );
        assert_eq!(
            count_kind(&g, |k| matches!(k, NodeKind::CheckFunctionIdentity { .. })),
            1
        );
        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::Int32Mul(_, _))), 1);
    }

    #[test]
    fn remaining_call_with_runtime_object_op_declines() {
        // A runtime `Call` left in the graph beside a runtime object op (here an
        // un-inlined method call) is the mix the optimizing tier still loses to
        // the baseline on, so it declines.
        let mut v = view(
            2,
            4,
            &[
                (Op::Call, vec![r(2), r(0), Operand::ConstIndex(1), r(1)], 0),
                (
                    Op::CallMethodValue,
                    vec![r(3), r(2), Operand::ConstIndex(0), Operand::ConstIndex(0)],
                    0,
                ),
                (Op::ReturnValue, vec![r(3)], 0),
            ],
        );
        v.instructions[1].property_ic_site = Some(7);

        let err = build_full(&v).expect_err("call + runtime object op declines");
        assert!(matches!(
            err,
            Unsupported::Unlowered("call plus object fast path")
        ));
    }

    #[test]
    fn remaining_call_with_shape_guard_compiles() {
        // A runtime `Call` beside only a shape-guarded property load (a cheap
        // `CheckShape` + sealed `LoadSlot`, no runtime object stub) is allowed:
        // the guard is not the runtime object op the decline guards against.
        let mut v = view(
            2,
            4,
            &[
                (Op::Call, vec![r(2), r(0), Operand::ConstIndex(1), r(1)], 0),
                (
                    Op::LoadProperty,
                    vec![r(3), r(2), Operand::ConstIndex(0)],
                    0,
                ),
                (Op::ReturnValue, vec![r(3)], 0),
            ],
        );
        v.cage_base = 1;
        v.instructions[1].property_feedback = Some((100, 8));

        let g = build_full(&v).expect("call + shape guard compiles");
        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::Call { .. })), 1);
        assert_eq!(
            count_kind(&g, |k| matches!(k, NodeKind::CheckShape(_, _))),
            1
        );
    }

    #[test]
    fn missing_inline_method_inside_loop_declines() {
        let mut v = view(
            1,
            5,
            &[
                (Op::LoadInt32, vec![r(1), imm(0)], 0),
                (Op::LessThan, vec![r(2), r(1), r(0)], ARITH_INT32),
                (Op::JumpIfFalse, vec![imm(rel(2, 6)), r(2)], 0),
                (
                    Op::CallMethodValue,
                    vec![r(3), r(4), Operand::ConstIndex(0), Operand::ConstIndex(0)],
                    0,
                ),
                (Op::Increment, vec![r(1), r(1), imm(1)], ARITH_INT32),
                (Op::Jump, vec![imm(rel(5, 1))], 0),
                (Op::ReturnValue, vec![r(1)], 0),
            ],
        );
        v.instructions[3].property_ic_site = Some(7);

        assert_eq!(
            build_full(&v).unwrap_err(),
            Unsupported::Opcode(Op::CallMethodValue)
        );
    }

    #[test]
    fn strict_null_compare_lowers_to_tagged_null_predicate() {
        let g = build_full(&view(
            1,
            4,
            &[
                (Op::LoadNull, vec![r(1)], 0),
                (Op::Equal, vec![r(2), r(0), r(1)], 0),
                (Op::NotEqual, vec![r(3), r(0), r(1)], 0),
                (Op::ReturnValue, vec![r(2)], 0),
            ],
        ))
        .expect("null identity compare builds");

        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::ConstNull)), 1);
        assert_eq!(
            count_kind(&g, |k| matches!(
                k,
                NodeKind::TaggedIsNull { negate: false, .. }
            )),
            1
        );
        assert_eq!(
            count_kind(&g, |k| matches!(
                k,
                NodeKind::TaggedIsNull { negate: true, .. }
            )),
            1
        );
        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::CheckNumber(_))), 0);
    }

    #[test]
    fn straight_line_int_add_guards_param_only() {
        // r2 = r0 + 1; return r2   (r0 is a parameter, 1 is an int32 const)
        let g = build_full(&view(
            1,
            4,
            &[
                (Op::LoadInt32, vec![r(1), imm(1)], 0),
                (Op::Add, vec![r(2), r(0), r(1)], ARITH_INT32),
                (Op::ReturnValue, vec![r(2)], 0),
            ],
        ))
        .expect("int32 add builds");

        assert_eq!(g.blocks.len(), 1);
        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::Int32Add(..))), 1);
        // Only the tagged parameter needs a guard; the int32 const does not.
        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::CheckInt32(_))), 1);
        assert!(matches!(g.blocks[0].term, Some(Terminator::Return(_))));
    }

    #[test]
    fn existing_float_operand_overrides_int32_feedback() {
        let mut v = view(
            0,
            4,
            &[
                (Op::LoadNumber, vec![r(0), Operand::ConstIndex(0)], 0),
                (Op::LoadInt32, vec![r(1), imm(1)], 0),
                (Op::Add, vec![r(2), r(0), r(1)], ARITH_INT32),
                (Op::ReturnValue, vec![r(2)], 0),
            ],
        );
        v.instructions[0].load_number = Some(1.5);
        let g = build_full(&v).expect("float add builds despite stale int32 feedback");

        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::Float64Add(..))), 1);
        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::Int32Add(..))), 0);
        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::CheckInt32(_))), 0);
        assert_eq!(
            count_kind(&g, |k| matches!(k, NodeKind::Int32ToFloat64(_))),
            1
        );
    }

    #[test]
    fn bitwise_float_operand_lowers_through_to_int32() {
        let mut v = view(
            0,
            4,
            &[
                (Op::LoadNumber, vec![r(0), Operand::ConstIndex(0)], 0),
                (Op::LoadInt32, vec![r(1), imm(0)], 0),
                (
                    Op::BitwiseOr,
                    vec![r(2), r(0), r(1)],
                    ARITH_INT32 | ARITH_FLOAT64,
                ),
                (Op::ReturnValue, vec![r(2)], 0),
            ],
        );
        v.instructions[0].load_number = Some(2_500_000.0);
        let g = build_full(&v).expect("bitwise float coercion builds");

        assert_eq!(
            count_kind(&g, |k| matches!(k, NodeKind::Float64ToInt32(_))),
            1
        );
        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::Int32BitOr(..))), 1);
        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::CheckInt32(_))), 0);
    }

    #[test]
    fn diamond_merge_inserts_phi() {
        // if (r0 < r1) r2 = 1 else r2 = 2; return r2
        //  0 LessThan r2, r0, r1
        //  1 JumpIfFalse ->else(4), r2
        //  2 LoadInt32 r2, 1
        //  3 Jump ->merge(5)
        //  4 LoadInt32 r2, 2          (else)
        //  5 ReturnValue r2           (merge)
        let g = build_full(&view(
            2,
            4,
            &[
                (Op::LessThan, vec![r(2), r(0), r(1)], ARITH_INT32),
                (Op::JumpIfFalse, vec![imm(rel(1, 4)), r(2)], 0),
                (Op::LoadInt32, vec![r(2), imm(1)], 0),
                (Op::Jump, vec![imm(rel(3, 5))], 0),
                (Op::LoadInt32, vec![r(2), imm(2)], 0),
                (Op::ReturnValue, vec![r(2)], 0),
            ],
        ))
        .expect("diamond builds");

        // The merge block (start_pc 5*STRIDE) has two predecessors and a phi for
        // r2.
        let merge = g
            .blocks
            .iter()
            .find(|b| b.start_pc == 5 * STRIDE)
            .expect("merge block");
        assert_eq!(merge.preds.len(), 2);
        assert_eq!(merge.phis.len(), 1);
        assert!(matches!(g.node(merge.phis[0]).kind, NodeKind::Phi(ref ops) if ops.len() == 2));
    }

    #[test]
    fn counting_loop_inserts_header_phis() {
        // i=0; acc=0; while (i < n) { acc += i; i += 1 } return acc
        //  0 LoadInt32 r1, 0         (i)
        //  1 LoadInt32 r2, 0         (acc)
        //  2 LessThan  r3, r1, r0    (header)
        //  3 JumpIfFalse ->exit(8), r3
        //  4 Add r2, r2, r1
        //  5 LoadInt32 r4, 1
        //  6 Add r1, r1, r4
        //  7 Jump ->header(2)
        //  8 ReturnValue r2          (exit)
        let g = build_full(&view(
            1,
            5,
            &[
                (Op::LoadInt32, vec![r(1), imm(0)], 0),
                (Op::LoadInt32, vec![r(2), imm(0)], 0),
                (Op::LessThan, vec![r(3), r(1), r(0)], ARITH_INT32),
                (Op::JumpIfFalse, vec![imm(rel(3, 8)), r(3)], 0),
                (Op::Add, vec![r(2), r(2), r(1)], ARITH_INT32),
                (Op::LoadInt32, vec![r(4), imm(1)], 0),
                (Op::Add, vec![r(1), r(1), r(4)], ARITH_INT32),
                (Op::Jump, vec![imm(rel(7, 2))], 0),
                (Op::ReturnValue, vec![r(2)], 0),
            ],
        ))
        .expect("loop builds");

        let header = g
            .blocks
            .iter()
            .find(|b| b.start_pc == 2 * STRIDE)
            .expect("header block");
        assert_eq!(header.preds.len(), 2, "entry + back edge");
        // Exactly the two genuinely loop-carried values (i and acc) get a header
        // phi; trivial-phi elimination collapses the invariant n's phi.
        assert_eq!(header.phis.len(), 2, "phis for i and acc only");
        assert!(g.blocks.iter().all(|b| b.sealed), "all blocks sealed");

        // No trivial phi survives anywhere: every live phi merges at least two
        // distinct inputs (ignoring self-references).
        for block in &g.blocks {
            for &phi in &block.phis {
                let NodeKind::Phi(ops) = &g.node(phi).kind else {
                    unreachable!("block.phis holds phi nodes");
                };
                let distinct: std::collections::HashSet<NodeId> =
                    ops.iter().copied().filter(|&op| op != phi).collect();
                assert!(
                    distinct.len() >= 2,
                    "phi {phi} is trivial (inputs {ops:?}) and should be eliminated"
                );
            }
        }
    }

    #[test]
    fn bails_on_unsupported_opcode() {
        // `Rem` (`%`) is outside the arithmetic subset and bails the function.
        let err = build_full(&view(
            2,
            4,
            &[
                (Op::Rem, vec![r(2), r(0), r(1)], ARITH_INT32),
                (Op::ReturnValue, vec![r(2)], 0),
            ],
        ))
        .unwrap_err();
        assert_eq!(err, Unsupported::Opcode(Op::Rem));
    }

    #[test]
    fn unknown_feedback_after_loop_deopts_epilogue_only() {
        // A cold/unobserved arithmetic op after a hot loop should not prevent OSR
        // into the loop. The epilogue resumes in the interpreter at that op.
        let g = build_full(&view(
            1,
            6,
            &[
                (Op::LoadInt32, vec![r(1), imm(0)], 0),
                (Op::LoadInt32, vec![r(2), imm(0)], 0),
                (Op::LessThan, vec![r(3), r(1), r(0)], ARITH_INT32),
                (Op::JumpIfFalse, vec![imm(rel(3, 8)), r(3)], 0),
                (Op::Add, vec![r(2), r(2), r(1)], ARITH_INT32),
                (Op::LoadInt32, vec![r(4), imm(1)], 0),
                (Op::Add, vec![r(1), r(1), r(4)], ARITH_INT32),
                (Op::Jump, vec![imm(rel(7, 2))], 0),
                (Op::Add, vec![r(5), r(2), r(4)], 0),
                (Op::ReturnValue, vec![r(5)], 0),
            ],
        ))
        .expect("unknown feedback after loop deopts");

        let epilogue = g
            .blocks
            .iter()
            .find(|b| b.start_pc == 8 * STRIDE)
            .expect("epilogue block");
        assert!(matches!(epilogue.term, Some(Terminator::Deopt(pc)) if pc == 8 * STRIDE));
    }

    #[test]
    fn osr_target_skips_unsupported_earlier_loop() {
        // A setup loop contains an unsupported `%`, then execution reaches a
        // later simple hot loop. Whole-function compilation must decline the
        // setup loop, but an OSR-target compile rooted at the later header should
        // build only the region reachable from that header.
        let v = view(
            1,
            8,
            &[
                (Op::LoadInt32, vec![r(1), imm(0)], 0),
                (Op::LessThan, vec![r(2), r(1), r(0)], ARITH_INT32),
                (Op::JumpIfFalse, vec![imm(rel(2, 6)), r(2)], 0),
                (Op::Rem, vec![r(3), r(1), r(0)], ARITH_INT32),
                (Op::Increment, vec![r(1), r(1), imm(1)], ARITH_INT32),
                (Op::Jump, vec![imm(rel(5, 1))], 0),
                (Op::LoadInt32, vec![r(4), imm(0)], 0),
                (Op::LoadInt32, vec![r(6), imm(1)], 0),
                (Op::LessThan, vec![r(5), r(4), r(0)], ARITH_INT32),
                (Op::JumpIfFalse, vec![imm(rel(9, 12)), r(5)], 0),
                (Op::Add, vec![r(4), r(4), r(6)], ARITH_INT32),
                (Op::Jump, vec![imm(rel(11, 8))], 0),
                (Op::ReturnValue, vec![r(4)], 0),
            ],
        );
        assert_eq!(
            build_full(&v).unwrap_err(),
            Unsupported::Opcode(Op::Rem),
            "whole-function compile still rejects the setup loop"
        );

        let g = build(&v, Some(8 * STRIDE)).expect("target OSR graph builds");
        assert_ne!(g.entry, 0, "OSR graph uses a synthetic entry");
        let header_id = g
            .blocks
            .iter()
            .position(|b| b.start_pc == 8 * STRIDE && b.preds.contains(&g.entry))
            .expect("target header block") as BlockId;
        let header = g.block(header_id);
        assert_eq!(header.preds.len(), 2, "synthetic entry + backedge");
        assert!(
            header.phis.iter().any(|&phi| {
                matches!(g.node(phi).kind, NodeKind::Phi(ref ops) if ops.len() == 2)
            }),
            "loop-carried value keeps a real phi"
        );
    }

    #[test]
    fn osr_target_declines_unsupported_inside_target_loop() {
        // Deopt terminators are only safe outside the optimized loop. An
        // unsupported opcode inside the target loop would execute a compiled
        // prefix and then re-run the bytecode body from the unsupported PC, so
        // target compilation must decline instead.
        let v = view(
            1,
            7,
            &[
                (Op::LoadInt32, vec![r(1), imm(0)], 0),
                (Op::LoadInt32, vec![r(5), imm(1)], 0),
                (Op::LessThan, vec![r(2), r(1), r(0)], ARITH_INT32),
                (Op::JumpIfFalse, vec![imm(rel(3, 7)), r(2)], 0),
                (Op::Rem, vec![r(3), r(1), r(5)], ARITH_INT32),
                (Op::Increment, vec![r(1), r(1), imm(1)], ARITH_INT32),
                (Op::Jump, vec![imm(rel(6, 2))], 0),
                (Op::ReturnValue, vec![r(1)], 0),
            ],
        );

        assert_eq!(
            build(&v, Some(2 * STRIDE)).unwrap_err(),
            Unsupported::Opcode(Op::Rem)
        );
    }

    #[test]
    fn div_lowers_to_float() {
        // `Div` always takes the float path (int32 operands still yield a
        // non-integer result), so its operands are widened/checked to f64 and
        // the node is a `Float64Div`.
        let g = build_full(&view(
            2,
            4,
            &[
                (Op::Div, vec![r(2), r(0), r(1)], ARITH_INT32),
                (Op::ReturnValue, vec![r(2)], 0),
            ],
        ))
        .expect("div builds on the float path");
        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::Float64Div(..))), 1);
        // Both tagged params are guarded by a `CheckNumber`.
        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::CheckNumber(_))), 2);
    }

    #[test]
    fn float_site_widens_int_operand() {
        // `r2 = r0 + 1` with a site that has observed a double: the int32 const
        // is widened to f64 (`Int32ToFloat64`) and the tagged param is
        // number-checked, feeding a `Float64Add`.
        let g = build_full(&view(
            1,
            4,
            &[
                (Op::LoadInt32, vec![r(1), imm(1)], 0),
                (Op::Add, vec![r(2), r(0), r(1)], ARITH_FLOAT64 | ARITH_INT32),
                (Op::ReturnValue, vec![r(2)], 0),
            ],
        ))
        .expect("float add builds");
        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::Float64Add(..))), 1);
        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::CheckNumber(_))), 1);
        assert_eq!(
            count_kind(&g, |k| matches!(k, NodeKind::Int32ToFloat64(_))),
            1
        );
    }

    #[test]
    fn bails_on_non_int32_feedback() {
        // Unobserved arithmetic site (feedback 0) cannot be speculated int32.
        let err = build_full(&view(
            2,
            4,
            &[
                (Op::Add, vec![r(2), r(0), r(1)], 0),
                (Op::ReturnValue, vec![r(2)], 0),
            ],
        ))
        .unwrap_err();
        assert_eq!(err, Unsupported::TypeFeedback(0));
    }
}
