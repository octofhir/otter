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
use otter_vm::jit_feedback::{ARITH_STRING, ArithFeedback};
use otter_vm::{
    JitArrayMethodKind, JitFunctionView, JitInlineCallee, JitInlineMethod, JitInstrView,
};
use rustc_hash::{FxHashMap, FxHashSet};

use super::ir::{
    BlockId, CmpOp, ElementLoadKind, Float64MathCall, Float64UnaryOp, Graph, NodeId, NodeKind,
    Repr, Terminator,
};

/// How a `Math.*` unary lowers: a single float instruction, or a leaf libm call.
enum MathLowering {
    Unary(Float64UnaryOp),
    Call(Float64MathCall),
}
use super::{Unsupported, deopt};

/// Build a typed SSA graph for `view`, or report why the function is outside the
/// optimizing subset. When `osr_pc` is set, build only the region reachable from
/// that loop header, with a synthetic entry edge feeding the header phis.
pub(super) fn build(view: &JitFunctionView, osr_pc: Option<u32>) -> Result<Graph, Unsupported> {
    if view.instructions.is_empty() {
        return Err(Unsupported::Empty);
    }
    let cfg = Cfg::discover(&view.instructions)?;
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
    // A polymorphic method inline replaces the `CallMethod` bridge with an inline
    // dispatch chain, so it no longer trips the call-plus-mem-op reject below. But
    // when the enclosing loop also indexes an array, the optimizing tier's element
    // access bridges through the materialize-frame stub and the whole loop runs
    // slower than the baseline's tuned IC sequence — the same cost model that
    // motivates the reject. Keep such bodies on the baseline until the optimizing
    // tier's element access is competitive; a poly dispatch over object fields
    // (no array op) still compiles.
    let has_poly_method_inline = graph
        .nodes
        .iter()
        .any(|node| matches!(node.kind, NodeKind::MethodIdentityMatches { .. }));
    if has_poly_method_inline {
        let has_array_op = graph.nodes.iter().any(|node| {
            matches!(
                node.kind,
                NodeKind::LoadElement(_, _)
                    | NodeKind::LoadElementUnboxed(_, _, _)
                    | NodeKind::StoreElement(_, _, _)
                    | NodeKind::LoadArrayLength(_)
            )
        });
        if has_array_op {
            return Err(Unsupported::Unlowered(
                "polymorphic method inline plus array access",
            ));
        }
    }
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
                | NodeKind::LoadElementUnboxed(_, _, _)
                | NodeKind::StoreElement(_, _, _)
                | NodeKind::LoadArrayLength(_)
        )
    });
    // Reject the bridge-churn mix the cost model loses on: a plain `Call`
    // alongside any other bridging op (a memory op or an un-inlined method
    // call). Method calls with baked memory ops are allowed through; richards
    // hot method bodies use that shape, and the method-call cache keeps the
    // remaining bridge predictable enough for the typed slot/arithmetic body to
    // pay for itself.
    let reject = has_plain_call && (has_mem_op || has_method_call);
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
/// and constant nodes, it admits the primitive slot accesses (`CheckShape` /
/// `LoadSlot` / `LoadProtoSlot` / `StoreSlot`): a body restricted to these
/// reads and writes already-allocated objects' primitive fields, so it allocates
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
                    | NodeKind::CheckBool(_)
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
                    | NodeKind::LoadProtoSlot { .. }
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

fn local_slot(operands: &[Operand], i: usize) -> Option<u16> {
    match operands.get(i) {
        Some(Operand::Imm32(v)) => u16::try_from(*v).ok(),
        _ => None,
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
    fn discover(instrs: &[JitInstrView]) -> Result<Self, Unsupported> {
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
    /// Per-block cache of already-proven receiver shape guards, keyed by
    /// `(receiver SSA, compressed shape)` and mapping to the `CheckShape` node
    /// that proved it. A second same-shape access to the same SSA receiver reuses
    /// the earlier guard instead of re-checking. Reset at each block boundary
    /// (the guard only dominates within its straight-line block) and cleared
    /// after any instruction that re-enters the VM, allocates, or could
    /// transition an object's shape.
    checked_shapes: FxHashMap<(NodeId, u32), NodeId>,
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
        let mut graph = Graph::new(view.function_id, view.param_count, view.register_count, entry);
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
            checked_shapes: FxHashMap::default(),
        })
    }

    /// Emit (or reuse) a receiver shape guard. Within a straight-line block the
    /// same `(receiver SSA, shape)` is invariant, so a repeated access — the
    /// common `o.x`, `o.y`, `o.z` cluster — shares one `CheckShape` instead of
    /// re-proving it. The cache is reset per block and cleared after any
    /// shape-mutating or VM-re-entering instruction, so a reused guard always
    /// still dominates and still holds.
    fn guarded_receiver(
        &mut self,
        obj: NodeId,
        shape: u32,
        block: BlockId,
        byte_pc: u32,
    ) -> NodeId {
        if let Some(&checked) = self.checked_shapes.get(&(obj, shape)) {
            return checked;
        }
        let checked = self
            .graph
            .add_node(NodeKind::CheckShape(obj, shape), block, byte_pc);
        self.push_body(block, checked);
        self.checked_shapes.insert((obj, shape), checked);
        checked
    }

    /// Whether a node emitted while lowering an instruction leaves every cached
    /// receiver shape guard valid. Only pure, single-block, non-allocating,
    /// non-reshaping nodes qualify; anything that re-enters the VM, allocates
    /// (a safepoint), calls user code, or transitions a shape invalidates the
    /// whole cache. Conservative: an unlisted kind clears the cache.
    fn node_preserves_shape_cache(kind: &NodeKind) -> bool {
        matches!(
            kind,
            NodeKind::CheckShape(_, _)
                | NodeKind::CheckInt32(_)
                | NodeKind::CheckNumber(_)
                | NodeKind::CheckBool(_)
                | NodeKind::CheckFunctionIdentity { .. }
                | NodeKind::CheckMethodIdentity { .. }
                | NodeKind::LoadSlot(_, _)
                | NodeKind::LoadProtoSlot { .. }
                | NodeKind::LoadSlotPoly(_, _)
                | NodeKind::StoreSlot(_, _, _)
                | NodeKind::StoreSlotPoly(_, _, _)
                | NodeKind::LoadElement(_, _)
                | NodeKind::LoadElementUnboxed(_, _, _)
                | NodeKind::StoreElement(_, _, _)
                | NodeKind::LoadArrayLength(_)
                | NodeKind::ArrayPop { .. }
                | NodeKind::Param(_)
                | NodeKind::Phi(_)
                | NodeKind::ConstInt32(_)
                | NodeKind::ConstF64(_)
                | NodeKind::ConstBool(_)
                | NodeKind::ConstUndefined
                | NodeKind::ConstNull
                | NodeKind::SelfClosure
                | NodeKind::LoadThis
                | NodeKind::LoadHole
                | NodeKind::LoadUpvalue(_)
                | NodeKind::InlineUpvalue { .. }
                | NodeKind::Int32Add(_, _)
                | NodeKind::Int32Sub(_, _)
                | NodeKind::Int32Mul(_, _)
                | NodeKind::Int32Rem(_, _)
                | NodeKind::Int32Compare(_, _, _)
                | NodeKind::Int32ToFloat64(_)
                | NodeKind::Float64ToInt32(_)
                | NodeKind::Float64Add(_, _)
                | NodeKind::Float64Sub(_, _)
                | NodeKind::Float64Mul(_, _)
                | NodeKind::Float64Div(_, _)
                | NodeKind::Float64Rem(_, _)
                | NodeKind::Float64Compare(_, _, _)
                | NodeKind::Float64Unary(_, _)
                | NodeKind::Float64UnaryCall(_, _)
        )
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
    fn fill_block(&mut self, cfg_block: BlockId) -> Result<(), Unsupported> {
        let (start, end) = self.cfg.ranges[cfg_block as usize];
        // `cfg_block` indexes the CFG (successor lookups at the block's exit);
        // `block` is where nodes and the terminator are placed. A polymorphic
        // method inline splits this bytecode block, redirecting `block` to the
        // merge block of its guard chain so the instructions after the call build
        // into the merge (whose successors are `cfg_block`'s, rewired there).
        let mut block = cfg_block;
        // Shape guards proven earlier in the program do not dominate this block's
        // entry (predecessors may take other paths), so start with an empty cache.
        self.checked_shapes.clear();
        // Nodes emitted so far; the receiver-guard cache is invalidated whenever
        // the previous instruction added a node that could re-enter the VM,
        // allocate, or transition a shape (checked at the top of each iteration so
        // the object-literal / active-define early `continue`s are covered too).
        let mut nodes_before = self.graph.nodes.len();
        for i in start..end {
            if self.graph.nodes[nodes_before..]
                .iter()
                .any(|n| !Self::node_preserves_shape_cache(&n.kind))
            {
                self.checked_shapes.clear();
            }
            nodes_before = self.graph.nodes.len();
            // Capture the instruction's fields up front so the `&self.view`
            // borrow ends before the `&mut self.graph` mutations below.
            let instr = &self.view.instructions[i];
            let op = instr.op;
            let byte_pc = instr.byte_pc;
            let feedback = instr.arith_feedback;
            let make_self = instr.make_self;
            let load_number = instr.load_number;
            let load_array_length = instr.load_array_length;
            let method_hint = instr.method_hint;
            let property_feedback = instr.property_feedback;
            let property_feedback_poly = instr.property_feedback_poly.clone();
            let property_proto_feedback = instr.property_proto_feedback;
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
                // Folding defers every property store to a single
                // `AllocObjectLiteral` at the final define, holding each captured
                // property value as an SSA value until then. The optimizing tier's
                // GC safepoints root the interpreter frame-slot window, not the
                // machine homes those SSA values occupy, and the folded value is
                // never written back to its frame slot — so a garbage-collecting op
                // between the `NewObject` and the final define moves the captured
                // object and leaves the pending store dangling. Decline the fold
                // (the whole function drops to the baseline, which stores each
                // property into the already-rooted object as it goes) whenever a
                // property value spans such an op. A key `LoadString` is folded
                // away and never allocates, so it does not count.
                let key_pcs: FxHashSet<u32> = plan.key_pcs.iter().copied().collect();
                let spans_gc_op = self.view.instructions.iter().any(|i| {
                    i.byte_pc > byte_pc
                        && i.byte_pc <= last_define_pc
                        && !key_pcs.contains(&i.byte_pc)
                        && matches!(
                            i.op,
                            Op::Call
                                | Op::CallMethodValue
                                | Op::New
                                | Op::NewArray
                                | Op::NewObject
                                | Op::LoadString
                        )
                });
                if spans_gc_op {
                    return Err(Unsupported::Unlowered(
                        "object literal value spans a garbage-collecting op",
                    ));
                }
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
                Op::NewArray => {
                    let dst = reg(&operands, 0)?;
                    let node = self.graph.add_node(NodeKind::NewArray, block, byte_pc);
                    self.graph.set_frame_dst(node, dst);
                    self.push_body(block, node);
                    self.def_register(dst, block, node, byte_pc);
                }
                Op::LoadString => {
                    let dst = reg(&operands, 0)?;
                    let node = self.graph.add_node(NodeKind::LoadString, block, byte_pc);
                    self.graph.set_frame_dst(node, dst);
                    self.push_body(block, node);
                    self.def_register(dst, block, node, byte_pc);
                }
                // `LoadGlobalOrThrow dst, nameIdx` — read a free global identifier
                // through the VM lookup (global lexical cell, then global object),
                // throwing when unbound. Lowered as a GC-safe bridge: the runtime
                // helper re-decodes the operands from the bytecode, so the node
                // carries only its destination register for liveness / reload.
                Op::LoadGlobalOrThrow => {
                    let dst = reg(&operands, 0)?;
                    let node = self
                        .graph
                        .add_node(NodeKind::LoadGlobalOrThrow, block, byte_pc);
                    self.graph.set_frame_dst(node, dst);
                    self.push_body(block, node);
                    self.def_register(dst, block, node, byte_pc);
                }
                // `LoadProperty dst, obj, name` — lower baked IC feedback into
                // inline slot access. Own-data sites become receiver shape
                // guards; simple direct-prototype data sites become receiver and
                // prototype guards; unsupported sites bail to the interpreter.
                // `StoreGlobalBinding value, nameIdx, strict` — write a free global
                // identifier's binding. The runtime helper re-decodes the operands
                // and performs the §9.1.1.4 SetMutableBinding lookup; the value is
                // kept as an input so it is materialized into its frame slot first.
                Op::StoreGlobalBinding => {
                    let value_reg = reg(&operands, 0)?;
                    let value = self.read_variable(value_reg, block);
                    let node = self
                        .graph
                        .add_node(NodeKind::StoreGlobalBinding { value }, block, byte_pc);
                    self.push_body(block, node);
                }
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
                    if self.view.cage_base == 0 {
                        self.deopt_or_decline(
                            block,
                            byte_pc,
                            Unsupported::Opcode(Op::LoadProperty),
                        )?;
                        return Ok(());
                    }
                    let obj = self.read_variable(obj_reg, block);
                    let load = if let Some((shape, slot_byte)) = property_feedback {
                        // Monomorphic: a single shape guard then the slot load. The
                        // guard is shared with an earlier same-shape access to this
                        // receiver in the block.
                        let checked = self.guarded_receiver(obj, shape, block, byte_pc);
                        self.graph
                            .add_node(NodeKind::LoadSlot(checked, slot_byte), block, byte_pc)
                    } else if let Some((recv_shape, proto_shape, slot_byte)) =
                        property_proto_feedback
                    {
                        self.graph.add_node(
                            NodeKind::LoadProtoSlot {
                                recv: obj,
                                recv_shape,
                                proto_shape,
                                slot_byte,
                            },
                            block,
                            byte_pc,
                        )
                    } else if !property_feedback_poly.is_empty() {
                        // Polymorphic: an inline structure-guard chain over the
                        // baked `(shape, slot)` cases, deopt on the final miss.
                        self.graph.add_node(
                            NodeKind::LoadSlotPoly(
                                obj,
                                property_feedback_poly.clone().into_boxed_slice(),
                            ),
                            block,
                            byte_pc,
                        )
                    } else if self.is_osr_target() {
                        // An OSR-entry compile already speculates numeric on
                        // arithmetic with empty feedback (`allow_empty_feedback_for_osr`);
                        // a no-feedback property load has no shape to warm those
                        // sites, so declining here keeps the whole function on the
                        // interpreter until it tiers up from its function entry with
                        // real feedback, rather than compiling a body whose loads
                        // feed unguarded OSR speculation.
                        self.deopt_or_decline(
                            block,
                            byte_pc,
                            Unsupported::Opcode(Op::LoadProperty),
                        )?;
                        return Ok(());
                    } else {
                        // No inline-cacheable shape feedback (a site cold at
                        // compile time, or a polymorphic / megamorphic miss): run
                        // the full runtime load through the bridge so a caller body
                        // that reads such a site still compiles instead of
                        // declining the whole function. The receiver stays live
                        // (materialized by the call safepoint) for the bridge to
                        // re-decode; the result reloads into `dst`.
                        let node = self
                            .graph
                            .add_node(NodeKind::LoadPropertyGeneric, block, byte_pc);
                        self.graph.set_frame_dst(node, dst);
                        self.push_body(block, node);
                        self.def_register(dst, block, node, byte_pc);
                        continue;
                    };
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
                    // No inline-cacheable shape feedback (a shape-transition add, an
                    // accessor, or a polymorphic miss): run the full runtime store
                    // through the bridge so a constructor body that grows objects
                    // still compiles instead of declining the whole function. The
                    // receiver and value stay live (modeled in `reg_effects`) so the
                    // call safepoint materializes them for the bridge to re-decode.
                    if property_feedback.is_none() && property_feedback_poly.is_empty() {
                        let node =
                            self.graph
                                .add_node(NodeKind::StorePropertyGeneric, block, byte_pc);
                        self.push_body(block, node);
                        continue;
                    }
                    if self.view.cage_base == 0 {
                        self.deopt_or_decline(
                            block,
                            byte_pc,
                            Unsupported::Opcode(Op::StoreProperty),
                        )?;
                        return Ok(());
                    }
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
                    if let Some((shape, slot_byte)) = property_feedback {
                        // Monomorphic: a single shape guard then the slot store. The
                        // guard is shared with an earlier same-shape access to this
                        // receiver in the block; a slot store writes a value and
                        // never transitions the shape, so the cache stays valid.
                        let checked = self.guarded_receiver(obj, shape, block, byte_pc);
                        let store = self.graph.add_node(
                            NodeKind::StoreSlot(checked, slot_byte, value),
                            block,
                            byte_pc,
                        );
                        self.push_body(block, store);
                    } else {
                        // Polymorphic: an inline structure-guard chain over the
                        // baked `(shape, slot)` cases, deopt on the final miss.
                        let store = self.graph.add_node(
                            NodeKind::StoreSlotPoly(
                                obj,
                                property_feedback_poly.clone().into_boxed_slice(),
                                value,
                            ),
                            block,
                            byte_pc,
                        );
                        self.push_body(block, store);
                    }
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
                    // A site the interpreter only ever saw loading from one
                    // unboxable typed-array kind lowers to a native-representation
                    // load (no box on load, no unbox in the numeric consumer); any
                    // other kind deopts at the guard. Everything else keeps the
                    // generic boxed load.
                    let load_kind = match instr.element_load_kind {
                        otter_vm::jit::JitElementLoadKind::Float64 => {
                            Some(NodeKind::LoadElementUnboxed(recv, idx, ElementLoadKind::Float64))
                        }
                        otter_vm::jit::JitElementLoadKind::Int32 => {
                            Some(NodeKind::LoadElementUnboxed(recv, idx, ElementLoadKind::Int32))
                        }
                        otter_vm::jit::JitElementLoadKind::Any => None,
                    };
                    let load = self.graph.add_node(
                        load_kind.unwrap_or(NodeKind::LoadElement(recv, idx)),
                        block,
                        byte_pc,
                    );
                    self.graph.set_frame_dst(load, dst);
                    self.push_body(block, load);
                    self.def_register(dst, block, load, byte_pc);
                }
                // `StoreElement recv, idx, src` — inline dense-array and
                // typed-array element stores for primitive numeric values. The
                // RHS representation comes from warmup feedback recorded at the
                // store site; a miss deoptimizes before writing so the
                // interpreter owns full `[[Set]]` semantics.
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
                    let value = match self.graph.node(value).kind.repr() {
                        Repr::Int32 | Repr::Float64 => value,
                        Repr::Tagged | Repr::Bool => {
                            let fb = ArithFeedback::from_bits(feedback);
                            if fb.is_int32_only() {
                                self.int32_operand(block, src_reg, feedback, byte_pc)?
                            } else if fb.is_numeric_only() {
                                self.float_operand(block, src_reg, byte_pc)
                            } else {
                                self.deopt_or_decline(
                                    block,
                                    byte_pc,
                                    Unsupported::TypeFeedback(feedback),
                                )?;
                                return Ok(());
                            }
                        }
                    };
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
                    const MAX_METHOD_ARGS: usize = 4;
                    let dst = reg(&operands, 0)?;
                    let recv_reg = reg(&operands, 1)?;
                    let name = const_index(&operands, 2)?;
                    let argc = const_index(&operands, 3)? as usize;
                    if argc > MAX_METHOD_ARGS {
                        return Err(Unsupported::OperandShape("method call arg count"));
                    }
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
                        if let Some(result) = self
                            .try_inline_method(block, byte_pc, recv, &method, argc, &args, None)?
                        {
                            self.def_register(dst, block, result, byte_pc);
                            continue;
                        }
                        // The linear replay declines a branchy body; a pure
                        // (side-effect-free) one splices its whole control-flow
                        // graph in instead, continuing the caller in the merge
                        // block the callee's returns feed.
                        if let Some((result, cont)) = self.try_inline_method_cfg(
                            cfg_block, block, byte_pc, recv, &method, argc, &args,
                        )? {
                            block = cont;
                            self.def_register(dst, block, result, byte_pc);
                            continue;
                        }
                    } else if let Some(arms) = self.view.inline_poly_methods.get(&byte_pc).cloned()
                        && let Some((result, merge)) = self
                            .try_inline_poly_method(block, byte_pc, recv, &arms, argc, &args, dst)?
                    {
                        block = merge;
                        self.def_register(dst, block, result, byte_pc);
                        continue;
                    }
                    let call = self.graph.add_node(
                        NodeKind::CallMethod {
                            recv,
                            recv_reg,
                            name,
                            site: property_ic_site.map(|site| site as u64),
                            method_hint,
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
                Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Rem => {
                    let (dst, lhs, rhs) =
                        (reg(&operands, 0)?, reg(&operands, 1)?, reg(&operands, 2)?);
                    let node = if op == Op::Add && feedback == ARITH_STRING {
                        let lhs_node = self.read_variable(lhs, block);
                        let rhs_node = self.read_variable(rhs, block);
                        self.graph.add_node(
                            NodeKind::StringConcat {
                                lhs: lhs_node,
                                rhs: rhs_node,
                            },
                            block,
                            byte_pc,
                        )
                    } else {
                        match self.arith_binop(block, op, lhs, rhs, feedback, byte_pc) {
                            Ok(node) => node,
                            Err(reason @ Unsupported::TypeFeedback(_)) => {
                                self.deopt_or_decline(block, byte_pc, reason)?;
                                return Ok(());
                            }
                            Err(reason) => return Err(reason),
                        }
                    };
                    self.graph.set_frame_dst(node, dst);
                    self.push_body(block, node);
                    self.def_register(dst, block, node, byte_pc);
                }
                // `Math.fn(x)` for the unary methods that are one exact float
                // instruction. Other methods (ties-to-+Inf `round`, multi-arg
                // `min`/`max`/`pow`, transcendentals needing libm) decline.
                Op::MathCall => {
                    use otter_bytecode::method_id::MathMethod as M;
                    let dst = reg(&operands, 0)?;
                    let method_id = const_index(&operands, 1)?;
                    let argc = const_index(&operands, 2)? as usize;
                    if argc != 1 {
                        self.deopt_or_decline(block, byte_pc, Unsupported::Opcode(op))?;
                        return Ok(());
                    }
                    // A single-instruction `Math.*` lowers to `Float64Unary`; a
                    // transcendental with no exact hardware op lowers to a leaf
                    // libm call (`Float64UnaryCall`) matching the interpreter's
                    // `f64` method bit-for-bit. Anything else declines.
                    let kind = match M::from_u32(method_id) {
                        Some(M::Sqrt) => MathLowering::Unary(Float64UnaryOp::Sqrt),
                        Some(M::Abs) => MathLowering::Unary(Float64UnaryOp::Abs),
                        Some(M::Floor) => MathLowering::Unary(Float64UnaryOp::Floor),
                        Some(M::Ceil) => MathLowering::Unary(Float64UnaryOp::Ceil),
                        Some(M::Trunc) => MathLowering::Unary(Float64UnaryOp::Trunc),
                        Some(M::Sin) => MathLowering::Call(Float64MathCall::Sin),
                        Some(M::Cos) => MathLowering::Call(Float64MathCall::Cos),
                        Some(M::Tan) => MathLowering::Call(Float64MathCall::Tan),
                        Some(M::Asin) => MathLowering::Call(Float64MathCall::Asin),
                        Some(M::Acos) => MathLowering::Call(Float64MathCall::Acos),
                        Some(M::Atan) => MathLowering::Call(Float64MathCall::Atan),
                        Some(M::Sinh) => MathLowering::Call(Float64MathCall::Sinh),
                        Some(M::Cosh) => MathLowering::Call(Float64MathCall::Cosh),
                        Some(M::Tanh) => MathLowering::Call(Float64MathCall::Tanh),
                        Some(M::Asinh) => MathLowering::Call(Float64MathCall::Asinh),
                        Some(M::Acosh) => MathLowering::Call(Float64MathCall::Acosh),
                        Some(M::Atanh) => MathLowering::Call(Float64MathCall::Atanh),
                        Some(M::Exp) => MathLowering::Call(Float64MathCall::Exp),
                        Some(M::Expm1) => MathLowering::Call(Float64MathCall::Expm1),
                        Some(M::Log) => MathLowering::Call(Float64MathCall::Log),
                        Some(M::Log2) => MathLowering::Call(Float64MathCall::Log2),
                        Some(M::Log10) => MathLowering::Call(Float64MathCall::Log10),
                        Some(M::Log1p) => MathLowering::Call(Float64MathCall::Log1p),
                        Some(M::Cbrt) => MathLowering::Call(Float64MathCall::Cbrt),
                        _ => {
                            self.deopt_or_decline(block, byte_pc, Unsupported::Opcode(op))?;
                            return Ok(());
                        }
                    };
                    let arg = self.read_variable(reg(&operands, 3)?, block);
                    let f = self.float_node_operand(block, arg, byte_pc);
                    let node_kind = match kind {
                        MathLowering::Unary(uop) => NodeKind::Float64Unary(uop, f),
                        MathLowering::Call(call) => NodeKind::Float64UnaryCall(call, f),
                    };
                    let node = self.graph.add_node(node_kind, block, byte_pc);
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
                    let succs = &self.cfg.succs[cfg_block as usize];
                    if succs.len() < 2 {
                        return Err(Unsupported::OperandShape(
                            "conditional branch without fallthrough",
                        ));
                    }
                    // succs == [target, fallthrough] (built in CFG discovery).
                    let fallthrough = succs[1];
                    let mut cond_node = self.read_variable(cond, block);
                    // Branches consume an unboxed predicate. A known boxed
                    // boolean (constant or boolean-only phi) can be consumed as
                    // tagged and tested against `false`; an unknown tagged value
                    // gets a boolean guard that deopts on every non-boolean so
                    // the interpreter still owns full ToBoolean semantics.
                    if self.graph.node(cond_node).kind.repr() != Repr::Bool
                        && !self.is_boxed_bool(cond_node, &mut FxHashSet::default())
                    {
                        let check =
                            self.graph
                                .add_node(NodeKind::CheckBool(cond_node), block, byte_pc);
                        self.push_body(block, check);
                        cond_node = check;
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
            let next = *self.cfg.succs[cfg_block as usize]
                .first()
                .ok_or(Unsupported::OperandShape("fallthrough without successor"))?;
            self.set_term(block, Terminator::Jump(next));
        }
        // A polymorphic inline redirected building into a synthetic merge block;
        // mark it filled so `seal_ready` can seal the CFG successors that now take
        // their control (and loop back-edge values) from it.
        if block != cfg_block {
            self.graph.blocks[block as usize].filled = true;
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
            Op::Rem => NodeKind::Int32Rem(l, r),
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

    /// Inline a *polymorphic* `Op::CallMethodValue` site as a synthetic-block
    /// guard chain: one guard per candidate receiver shape branches to an inlined
    /// copy of that shape's method body, falling through to the next candidate on
    /// a miss; a receiver matching none deoptimizes at the final arm's own
    /// identity guard (V8 Maglev `CheckMaps` eager deopt / JSC `handleInlining`
    /// per-target dispatch). All arms merge their result through a phi in a fresh
    /// merge block, which becomes the continuation for the rest of the enclosing
    /// bytecode block.
    ///
    /// Returns `(phi, merge)` — the destination value and the block the caller
    /// continues building into — or `None` (bridge the site) when the feedback is
    /// absent/degenerate or any arm body is not inlinable. On `None` nothing is
    /// committed: the arm bodies are built first (touching only the node/block
    /// arenas, never the SSA register maps), so a decline rolls back with two
    /// truncations and no stale state.
    #[allow(clippy::too_many_arguments)]
    fn try_inline_poly_method(
        &mut self,
        block: BlockId,
        call_pc: u32,
        recv: NodeId,
        arms: &[JitInlineMethod],
        argc: usize,
        args: &[NodeId],
        dst: u16,
    ) -> Result<Option<(NodeId, BlockId)>, Unsupported> {
        const MAX_POLY_ARMS: usize = 4;
        if self.view.cage_base == 0 || arms.len() < 2 || arms.len() > MAX_POLY_ARMS {
            return Ok(None);
        }
        // A statically undefined / hole receiver has no shape to dispatch on.
        if matches!(
            self.graph.node(recv).kind,
            NodeKind::ConstUndefined | NodeKind::LoadHole
        ) {
            return Ok(None);
        }
        let n = arms.len();
        let nodes0 = self.graph.nodes.len();
        let blocks0 = self.graph.blocks.len();

        // Inline every arm body into a fresh arm block first. This only appends to
        // the node/block arenas (arm bodies use a local register map, never the
        // builder's `current_def`), so a decline is a clean rollback.
        let mut arm_blocks: Vec<BlockId> = Vec::with_capacity(n);
        let mut arm_results: Vec<NodeId> = Vec::with_capacity(n);
        for (i, arm) in arms.iter().enumerate() {
            let a = self.graph.blocks.len() as BlockId;
            self.graph.blocks.push(super::ir::Block::new(call_pc));
            // Every arm but the last was proven by its guard's
            // `MethodIdentityMatches`, so it inlines with the receiver passed
            // straight through; the last arm keeps its `CheckMethodIdentity` as
            // the chain's final-miss deopt.
            let guarded_this = if i + 1 < n { Some(recv) } else { None };
            match self.try_inline_method(a, call_pc, recv, arm, argc, args, guarded_this)? {
                Some(result) => {
                    arm_blocks.push(a);
                    arm_results.push(result);
                }
                None => {
                    self.graph.nodes.truncate(nodes0);
                    self.graph.blocks.truncate(blocks0);
                    return Ok(None);
                }
            }
        }

        // Merge block and the guard blocks (one per non-final candidate).
        let merge = self.graph.blocks.len() as BlockId;
        self.graph.blocks.push(super::ir::Block::new(call_pc));
        let mut guards: Vec<BlockId> = Vec::with_capacity(n - 1);
        for _ in 0..n - 1 {
            let g = self.graph.blocks.len() as BlockId;
            self.graph.blocks.push(super::ir::Block::new(call_pc));
            guards.push(g);
        }

        // Guard `i` tests candidate `i` and branches to its arm or to the next
        // candidate; the last guard's miss falls to the final arm, whose own
        // `CheckMethodIdentity` (emitted by `try_inline_method`) deopts if the
        // receiver matches no candidate shape.
        for i in 0..n - 1 {
            let arm = &arms[i];
            let matches = self.graph.add_node(
                NodeKind::MethodIdentityMatches {
                    recv,
                    recv_shape: arm.recv_shape,
                    proto_shape: arm.proto_shape,
                    method_value_byte: arm.method_value_byte,
                    method_on_receiver: arm.method_on_receiver,
                    method_fid: arm.method_fid,
                },
                guards[i],
                call_pc,
            );
            self.push_body(guards[i], matches);
            let on_false = if i + 1 < n - 1 {
                guards[i + 1]
            } else {
                arm_blocks[n - 1]
            };
            self.set_term(
                guards[i],
                Terminator::Branch {
                    cond: matches,
                    on_true: arm_blocks[i],
                    on_false,
                },
            );
        }

        // Wire predecessors and the per-arm merge edge.
        self.graph.blocks[guards[0] as usize].preds.push(block);
        for i in 1..n - 1 {
            self.graph.blocks[guards[i] as usize]
                .preds
                .push(guards[i - 1]);
        }
        for i in 0..n {
            let pred = if i < n - 1 { guards[i] } else { guards[n - 2] };
            self.graph.blocks[arm_blocks[i] as usize].preds.push(pred);
            self.set_term(arm_blocks[i], Terminator::Jump(merge));
            self.graph.blocks[merge as usize].preds.push(arm_blocks[i]);
        }

        // The original block now enters the guard chain, and its CFG successors
        // take control from the merge block instead (so their phi operands read
        // the post-call environment).
        self.set_term(block, Terminator::Jump(guards[0]));
        for succ in self.cfg.succs[block as usize].clone() {
            for p in &mut self.graph.blocks[succ as usize].preds {
                if *p == block {
                    *p = merge;
                }
            }
        }

        // Merge the per-arm results into the destination phi.
        let phi = self.new_phi(dst, merge);
        self.graph.nodes[phi as usize].kind = NodeKind::Phi(arm_results);

        // Every synthetic block's predecessors are now final. Mark the guard and
        // arm blocks filled (fully built) so `seal_ready` treats them as complete
        // predecessors; the merge block is filled once the caller finishes
        // building the rest of the bytecode block into it (see `fill_block`).
        for &g in &guards {
            self.seal_block(g);
            self.graph.blocks[g as usize].filled = true;
        }
        for &a in &arm_blocks {
            self.seal_block(a);
            self.graph.blocks[a as usize].filled = true;
        }
        self.seal_block(merge);

        Ok(Some((phi, merge)))
    }

    #[allow(clippy::too_many_arguments)]
    fn try_inline_method(
        &mut self,
        block: BlockId,
        call_pc: u32,
        recv: NodeId,
        method: &JitInlineMethod,
        argc: usize,
        args: &[NodeId],
        guarded_this: Option<NodeId>,
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

        // `this` is the guarded receiver. A monomorphic site guards it here with a
        // deopt-on-miss `CheckMethodIdentity`; a polymorphic arm was already
        // proven by the chain's `MethodIdentityMatches` branch, so it passes the
        // receiver straight through and skips the redundant identity probe.
        let this = match guarded_this {
            Some(recv) => recv,
            None => {
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
                checked
            }
        };

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
                    write(&mut regs, dst, this)?;
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

    /// Whether a callee opcode is admissible in a *side-effect-free* CFG-splice
    /// inline: reads, typed arithmetic, comparisons, and intra-body branches, but
    /// nothing that writes memory, allocates, or re-enters user code. A method
    /// built entirely from these can be re-executed from its call site with no
    /// observable effect, which is what makes the re-run-the-call deopt of an
    /// inlined guard correct (see [`Self::try_inline_method_cfg`]).
    fn inline_cfg_op_is_pure(op: Op) -> bool {
        matches!(
            op,
            Op::LoadThis
                | Op::LoadInt32
                | Op::LoadNumber
                | Op::LoadTrue
                | Op::LoadFalse
                | Op::LoadUndefined
                | Op::LoadHole
                | Op::LoadLocal
                | Op::StoreLocal
                | Op::LoadUpvalue
                | Op::ToPrimitive
                | Op::ToNumeric
                | Op::LoadProperty
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
                | Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Equal
                | Op::NotEqual
                | Op::LooseEqual
                | Op::LooseNotEqual
                | Op::Jump
                | Op::JumpIfTrue
                | Op::JumpIfFalse
                | Op::Return
                | Op::ReturnValue
                | Op::ReturnUndefined
        )
    }

    /// Inline a *branchy, side-effect-free* monomorphic method by splicing the
    /// callee's control-flow graph into the caller at the call site.
    ///
    /// The linear replay ([`Self::try_inline_method`]) handles only straight-line
    /// method bodies — a method with an `if` or a short-circuit (`||` / `&&`)
    /// declines there. This builds the callee's basic blocks as fresh caller
    /// blocks and translates each with the builder's on-demand SSA machinery, so
    /// internal merges get real phis. Callee registers are offset above the
    /// caller's register file: they are named only inside the spliced blocks and
    /// never appear in a deopt frame state (which is keyed on the caller's live
    /// bytecode registers at the call PC), so a guard inside the body deopts by
    /// re-running the whole call at `call_pc` with the caller's frame — exactly
    /// what the existing single-frame machinery already captures.
    ///
    /// Restricted to **pure** callees (see [`Self::inline_cfg_op_is_pure`]):
    /// re-running the call is only observably-equivalent when the body has no
    /// side effect. A branchy method that mutates before a guard needs a
    /// mid-callee resume frame (a nested deopt state) and is left to the bridge.
    /// The callee CFG must also be a forward DAG (no internal loop), so the
    /// spliced blocks seal in topological order.
    ///
    /// Returns `(result, continuation)` — the call's destination value and the
    /// merge block the caller continues building into — or `None` (bridge the
    /// call). On `None` every arena touched is rolled back to a clean state.
    #[allow(clippy::too_many_arguments)]
    fn try_inline_method_cfg(
        &mut self,
        cfg_block: BlockId,
        block: BlockId,
        call_pc: u32,
        recv: NodeId,
        method: &JitInlineMethod,
        argc: usize,
        args: &[NodeId],
    ) -> Result<Option<(NodeId, BlockId)>, Unsupported> {
        const MAX_INLINE_METHOD_REGS: u16 = 64;
        const MAX_INLINE_METHOD_INSTRS: usize = 64;
        // Splice only at a real CFG block: the successor rewiring below reads
        // `self.cfg.succs[block]`, which a synthetic merge block (produced by an
        // earlier splice in this same bytecode block) does not have.
        if block != cfg_block
            || self.view.cage_base == 0
            || argc != usize::from(method.param_count)
            || method.register_count == 0
            || method.register_count > MAX_INLINE_METHOD_REGS
            || method.instructions.len() < 2
            || method.instructions.len() > MAX_INLINE_METHOD_INSTRS
            || !method.instructions.iter().all(|i| Self::inline_cfg_op_is_pure(i.op))
        {
            return Ok(None);
        }
        // The callee registers occupy `[base, base + register_count)` above the
        // caller's file; keep that window inside the `u16` register index space.
        let base = self.current_def.len();
        if base + usize::from(method.register_count) > usize::from(u16::MAX) {
            return Ok(None);
        }
        let base = base as u16;

        let ccfg = match Cfg::discover(&method.instructions) {
            Ok(c) => c,
            Err(_) => return Ok(None),
        };
        // Forward DAG only. Blocks are in start-PC order, so a back edge is a
        // predecessor whose block index is not strictly smaller.
        for (b, preds) in ccfg.preds.iter().enumerate() {
            if preds.iter().any(|&p| p as usize >= b) {
                return Ok(None);
            }
        }

        let nodes0 = self.graph.nodes.len();
        let blocks0 = self.graph.blocks.len();
        let cdef0 = self.current_def.len();
        let cont = blocks0 + ccfg.ranges.len();
        macro_rules! bail_cfg {
            () => {{
                self.graph.nodes.truncate(nodes0);
                self.graph.blocks.truncate(blocks0);
                self.current_def.truncate(cdef0);
                self.graph.phi_reg.retain(|&k, _| (k as usize) < nodes0);
                self.incomplete_phis.retain(|&b, _| (b as usize) < blocks0);
                return Ok(None);
            }};
        }

        // One caller-graph block per callee block, then the continuation merge.
        for &(start, _) in &ccfg.ranges {
            let pc = method.instructions[start].byte_pc;
            self.graph.blocks.push(super::ir::Block::new(pc));
        }
        self.graph.blocks.push(super::ir::Block::new(call_pc));
        self.current_def
            .resize(base as usize + usize::from(method.register_count), Default::default());
        let off = |r: u16| -> u16 { base + r };
        let gblock = |cb: BlockId| -> BlockId { blocks0 as BlockId + cb };

        // Callee-block predecessors (offset), the entry fed by the caller block.
        for (cb, preds) in ccfg.preds.iter().enumerate() {
            let mapped: Vec<BlockId> = preds.iter().map(|&p| gblock(p)).collect();
            self.graph.blocks[gblock(cb as BlockId) as usize].preds = mapped;
        }
        self.graph.blocks[gblock(0) as usize].preds = vec![block];

        // Seed the callee entry: parameter registers take the call arguments,
        // every other register starts `undefined` (matching an uninitialized
        // interpreter slot) so no cross-block read ever falls through to the
        // caller's register file.
        let entry_g = gblock(0);
        let ident = self.graph.add_node(
            NodeKind::CheckMethodIdentity {
                recv,
                recv_shape: method.recv_shape,
                proto_shape: method.proto_shape,
                method_value_byte: method.method_value_byte,
                method_on_receiver: method.method_on_receiver,
                method_fid: method.method_fid,
            },
            entry_g,
            call_pc,
        );
        self.push_body(entry_g, ident);
        for r in 0..method.register_count {
            let node = if (r as usize) < argc {
                args[r as usize]
            } else {
                let u = self.graph.add_node(NodeKind::ConstUndefined, entry_g, call_pc);
                self.push_body(entry_g, u);
                u
            };
            self.write_variable(off(r), entry_g, node);
        }

        // A captured binding of the inlined method resolves through the method's
        // own closure, which lives in the (identity-guarded) method slot: the
        // receiver's prototype slab for a prototype method, or the receiver's own
        // slab for an own-property method. Materialize it once in the entry block
        // so every `LoadUpvalue` in the body reads the method closure's spine.
        let method_closure = if method.instructions.iter().any(|i| i.op == Op::LoadUpvalue) {
            let node = if method.method_on_receiver {
                let checked = self.graph.add_node(
                    NodeKind::CheckShape(recv, method.recv_shape),
                    entry_g,
                    call_pc,
                );
                self.push_body(entry_g, checked);
                self.graph.add_node(
                    NodeKind::LoadSlot(checked, method.method_value_byte),
                    entry_g,
                    call_pc,
                )
            } else {
                self.graph.add_node(
                    NodeKind::LoadProtoSlot {
                        recv,
                        recv_shape: method.recv_shape,
                        proto_shape: method.proto_shape,
                        slot_byte: method.method_value_byte,
                    },
                    entry_g,
                    call_pc,
                )
            };
            self.push_body(entry_g, node);
            Some(node)
        } else {
            None
        };

        // Fill each callee block in topological (index) order. A block's
        // predecessors always have a lower index (forward DAG), so they are
        // already filled and sealed — the on-demand SSA reader resolves cross-
        // block values into real phis with no incomplete-phi backlog.
        let mut returns: Vec<NodeId> = Vec::new();
        for cb in 0..ccfg.ranges.len() {
            let g = gblock(cb as BlockId);
            self.graph.blocks[g as usize].sealed = true;
            let (start, end) = ccfg.ranges[cb];
            for instr in &method.instructions[start..end] {
                let op = instr.op;
                let operands = instr.operands.as_slice();
                match op {
                    Op::LoadThis => {
                        let dst = reg(operands, 0)?;
                        self.write_variable(off(dst), g, recv);
                    }
                    Op::LoadInt32 => {
                        let dst = reg(operands, 0)?;
                        let value = imm32(operands, 1)?;
                        let node = self.graph.add_node(NodeKind::ConstInt32(value), g, call_pc);
                        self.push_body(g, node);
                        self.write_variable(off(dst), g, node);
                    }
                    Op::LoadNumber => {
                        let dst = reg(operands, 0)?;
                        let Some(value) = instr.load_number else {
                            bail_cfg!();
                        };
                        let node = self.graph.add_node(NodeKind::ConstF64(value), g, call_pc);
                        self.push_body(g, node);
                        self.write_variable(off(dst), g, node);
                    }
                    Op::LoadTrue | Op::LoadFalse => {
                        let dst = reg(operands, 0)?;
                        let node = self.graph.add_node(
                            NodeKind::ConstBool(matches!(op, Op::LoadTrue)),
                            g,
                            call_pc,
                        );
                        self.push_body(g, node);
                        self.write_variable(off(dst), g, node);
                    }
                    Op::LoadUndefined | Op::LoadHole => {
                        let dst = reg(operands, 0)?;
                        let kind = if matches!(op, Op::LoadUndefined) {
                            NodeKind::ConstUndefined
                        } else {
                            NodeKind::LoadHole
                        };
                        let node = self.graph.add_node(kind, g, call_pc);
                        self.push_body(g, node);
                        self.write_variable(off(dst), g, node);
                    }
                    Op::LoadLocal | Op::StoreLocal => {
                        // `LoadLocal dst, srcIdx` and `StoreLocal src, dstIdx`
                        // both copy one register to another; the index operand is
                        // an inline immediate, the other operand a register.
                        let reg_operand = reg(operands, 0)?;
                        let imm_operand = u16::try_from(imm32(operands, 1)?)
                            .map_err(|_| Unsupported::OperandShape("inline local index"))?;
                        let (src, dst) = if matches!(op, Op::LoadLocal) {
                            (imm_operand, reg_operand)
                        } else {
                            (reg_operand, imm_operand)
                        };
                        let node = self.read_variable(off(src), g);
                        self.write_variable(off(dst), g, node);
                    }
                    Op::LoadUpvalue => {
                        let dst = reg(operands, 0)?;
                        let idx = imm32(operands, 1)?;
                        let Some(closure) = method_closure else {
                            bail_cfg!();
                        };
                        if idx < 0 {
                            bail_cfg!();
                        }
                        let node = self.graph.add_node(
                            NodeKind::InlineUpvalue {
                                closure,
                                index: idx as u32,
                            },
                            g,
                            call_pc,
                        );
                        self.push_body(g, node);
                        self.write_variable(off(dst), g, node);
                    }
                    Op::ToPrimitive | Op::ToNumeric => {
                        let dst = reg(operands, 0)?;
                        let src = reg(operands, 1)?;
                        let node = self.read_variable(off(src), g);
                        self.write_variable(off(dst), g, node);
                    }
                    Op::LoadProperty => {
                        let dst = reg(operands, 0)?;
                        let obj_reg = reg(operands, 1)?;
                        let obj = self.read_variable(off(obj_reg), g);
                        let Some(&slot_byte) = method.prop_offsets.get(&instr.byte_pc) else {
                            bail_cfg!();
                        };
                        // The receiver's shape is already proven by the entry
                        // `CheckMethodIdentity` (which deopts unless `recv.shape ==
                        // recv_shape`), and the entry dominates every spliced block,
                        // so a load off the receiver needs no second shape guard.
                        // A non-receiver object keeps its guard.
                        let checked = if obj == recv {
                            recv
                        } else {
                            let c = self.graph.add_node(
                                NodeKind::CheckShape(obj, method.recv_shape),
                                g,
                                call_pc,
                            );
                            self.push_body(g, c);
                            c
                        };
                        let load =
                            self.graph
                                .add_node(NodeKind::LoadSlot(checked, slot_byte), g, call_pc);
                        self.push_body(g, load);
                        self.write_variable(off(dst), g, load);
                    }
                    Op::Add | Op::Sub | Op::Mul | Op::Div => {
                        let dst = reg(operands, 0)?;
                        let lhs = self.read_variable(off(reg(operands, 1)?), g);
                        let rhs = self.read_variable(off(reg(operands, 2)?), g);
                        let node = match self
                            .arith_node_binop(g, op, lhs, rhs, instr.arith_feedback, call_pc)
                        {
                            Ok(node) => node,
                            Err(_) => bail_cfg!(),
                        };
                        self.push_body(g, node);
                        self.write_variable(off(dst), g, node);
                    }
                    Op::BitwiseOr | Op::BitwiseAnd | Op::BitwiseXor | Op::Shl | Op::Shr
                    | Op::Ushr => {
                        let dst = reg(operands, 0)?;
                        let lhs = self.read_variable(off(reg(operands, 1)?), g);
                        let rhs = self.read_variable(off(reg(operands, 2)?), g);
                        let node = match self
                            .bitwise_node_binop(g, op, lhs, rhs, instr.arith_feedback, call_pc)
                        {
                            Ok(node) => node,
                            Err(_) => bail_cfg!(),
                        };
                        self.push_body(g, node);
                        self.write_variable(off(dst), g, node);
                    }
                    Op::Increment => {
                        let dst = reg(operands, 0)?;
                        let src = self.read_variable(off(reg(operands, 1)?), g);
                        let delta = match operands.get(2) {
                            Some(Operand::Imm32(v)) => *v,
                            None => 1,
                            _ => return Err(Unsupported::OperandShape("increment delta")),
                        };
                        let step = self.graph.add_node(NodeKind::ConstInt32(delta), g, call_pc);
                        self.push_body(g, step);
                        let node =
                            match self.arith_node_binop(g, Op::Add, src, step, instr.arith_feedback, call_pc) {
                                Ok(node) => node,
                                Err(_) => bail_cfg!(),
                            };
                        self.push_body(g, node);
                        self.write_variable(off(dst), g, node);
                    }
                    Op::LessThan
                    | Op::LessEq
                    | Op::GreaterThan
                    | Op::GreaterEq
                    | Op::Equal
                    | Op::NotEqual
                    | Op::LooseEqual
                    | Op::LooseNotEqual => {
                        let dst = reg(operands, 0)?;
                        let (cmp, loose) = match op {
                            Op::LooseEqual => (CmpOp::Eq, true),
                            Op::LooseNotEqual => (CmpOp::Ne, true),
                            other => (CmpOp::from_op(other).expect("comparison opcode"), false),
                        };
                        let node = match self.compare(
                            g,
                            cmp,
                            off(reg(operands, 1)?),
                            off(reg(operands, 2)?),
                            instr.arith_feedback,
                            loose,
                            call_pc,
                        ) {
                            Ok(node) => node,
                            Err(_) => bail_cfg!(),
                        };
                        self.push_body(g, node);
                        self.write_variable(off(dst), g, node);
                    }
                    Op::Jump => {
                        let tgt = gblock(ccfg.succs[cb][0]);
                        self.set_term(g, Terminator::Jump(tgt));
                    }
                    Op::JumpIfTrue | Op::JumpIfFalse => {
                        let cond = reg(operands, 1)?;
                        let succs = &ccfg.succs[cb];
                        if succs.len() < 2 {
                            bail_cfg!();
                        }
                        let tgt_block = gblock(succs[0]);
                        let fallthrough = gblock(succs[1]);
                        let mut cond_node = self.read_variable(off(cond), g);
                        if self.graph.node(cond_node).kind.repr() != Repr::Bool
                            && !self.is_boxed_bool(cond_node, &mut FxHashSet::default())
                        {
                            let check =
                                self.graph.add_node(NodeKind::CheckBool(cond_node), g, call_pc);
                            self.push_body(g, check);
                            cond_node = check;
                        }
                        let (on_true, on_false) = if matches!(op, Op::JumpIfTrue) {
                            (tgt_block, fallthrough)
                        } else {
                            (fallthrough, tgt_block)
                        };
                        self.set_term(
                            g,
                            Terminator::Branch {
                                cond: cond_node,
                                on_true,
                                on_false,
                            },
                        );
                    }
                    Op::Return | Op::ReturnValue => {
                        let src = reg(operands, 0)?;
                        let value = self.read_variable(off(src), g);
                        returns.push(value);
                        self.graph.blocks[cont].preds.push(g);
                        self.set_term(g, Terminator::Jump(cont as BlockId));
                    }
                    Op::ReturnUndefined => {
                        let node = self.graph.add_node(NodeKind::ConstUndefined, g, call_pc);
                        self.push_body(g, node);
                        returns.push(node);
                        self.graph.blocks[cont].preds.push(g);
                        self.set_term(g, Terminator::Jump(cont as BlockId));
                    }
                    _ => bail_cfg!(),
                }
            }
            // A block whose last instruction was not a terminator falls through.
            if self.graph.blocks[g as usize].term.is_none() {
                let Some(&next) = ccfg.succs[cb].first() else {
                    bail_cfg!();
                };
                self.set_term(g, Terminator::Jump(gblock(next)));
            }
            self.graph.blocks[g as usize].filled = true;
        }

        if returns.is_empty() {
            bail_cfg!();
        }

        // The caller now enters the callee, and its CFG successors take control
        // from the continuation instead (so their phi operands read the post-call
        // environment).
        self.set_term(block, Terminator::Jump(entry_g));
        for succ in self.cfg.succs[cfg_block as usize].clone() {
            for p in &mut self.graph.blocks[succ as usize].preds {
                if *p == block {
                    *p = cont as BlockId;
                }
            }
        }

        // Merge the returns into the destination phi; a single return collapses
        // to its value via trivial-phi removal.
        let phi = self.new_phi(0, cont as BlockId);
        self.graph.nodes[phi as usize].kind = NodeKind::Phi(returns);
        self.graph.phi_reg.remove(&phi);
        let result = self.try_remove_trivial_phi(phi);
        self.seal_block(cont as BlockId);

        // A fresh continuation does not inherit the pre-call block's proven
        // receiver-shape guards (control reaches it from several return paths).
        self.checked_shapes.clear();
        Ok(Some((result, cont as BlockId)))
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
            Op::Rem => NodeKind::Float64Rem(l, r),
            _ => unreachable!("arith_binop on non-arithmetic op"),
        };
        Ok(self.graph.add_node(kind, block, byte_pc))
    }

    /// Lower a relational / equality site, mirroring [`Self::arith_binop`]'s
    /// representation choice: an int32-only site compares unboxed int32s, an
    /// otherwise-numeric site compares `f64`s, anything else bails.
    #[allow(clippy::too_many_arguments)]
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

    fn loop_header_live_registers(
        &self,
        bytecode_live: &FxHashMap<u32, FxHashSet<u16>>,
    ) -> FxHashSet<u16> {
        let mut regs = FxHashSet::default();
        for block in &self.graph.blocks {
            if self.back_edge_pc(block).is_none() {
                continue;
            }
            if let Some(live) = bytecode_live.get(&block.start_pc) {
                regs.extend(live.iter().copied());
            }
        }
        regs
    }

    fn active_loop_local_registers(&self) -> FxHashSet<u16> {
        if !self.view.inline_callees.is_empty()
            || !self.view.inline_methods.is_empty()
            || !self.view.inline_poly_methods.is_empty()
        {
            return FxHashSet::default();
        }

        let mut loop_ranges = Vec::new();
        for block in &self.graph.blocks {
            for &pred in &block.preds {
                if pred as usize >= self.cfg.ranges.len() {
                    continue;
                }
                let pred_block = &self.graph.blocks[pred as usize];
                if pred_block.start_pc <= block.start_pc {
                    continue;
                }
                let (_, pred_end) = self.cfg.ranges[pred as usize];
                let Some(last) = pred_end
                    .checked_sub(1)
                    .and_then(|idx| self.view.instructions.get(idx))
                else {
                    continue;
                };
                loop_ranges.push((block.start_pc, last.byte_pc));
            }
        }

        let mut regs = FxHashSet::default();
        for instr in &self.view.instructions {
            if !loop_ranges
                .iter()
                .any(|&(start, end)| start <= instr.byte_pc && instr.byte_pc <= end)
            {
                continue;
            }
            match instr.op {
                Op::LoadLocal => {
                    if let Some(src) = local_slot(&instr.operands, 1) {
                        regs.insert(src);
                    }
                }
                Op::StoreLocal => {
                    if let Some(dst) = local_slot(&instr.operands, 1) {
                        regs.insert(dst);
                    }
                }
                _ => {}
            }
        }
        regs
    }

    fn materialize_deopt_env(&mut self, block: BlockId, byte_pc: u32) {
        let mut regs = FxHashSet::default();
        let live = deopt::bytecode_liveness(self.view);
        if let Some(live) = live.get(&byte_pc) {
            regs.extend(live.iter().copied());
        }
        regs.extend(self.loop_header_live_registers(&live));
        regs.extend(self.active_loop_local_registers());
        for reg in regs {
            let _ = self.read_variable(reg, block);
        }
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
        if std::env::var_os("OTTER_JIT_TRACE").is_some() {
            let op = self
                .view
                .instructions
                .iter()
                .find(|instr| instr.byte_pc == byte_pc)
                .map(|instr| instr.op);
            eprintln!(
                "[otter-jit] optimizing site fid {} pc {byte_pc} op {op:?}: {reason:?}",
                self.view.function_id
            );
        }
        if self.block_is_in_loop(block) {
            return Err(reason);
        }
        if !self.can_deopt_at(byte_pc) {
            return Err(reason);
        }
        self.materialize_deopt_env(block, byte_pc);
        self.set_term(block, Terminator::Deopt(byte_pc));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::deopt;
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
                method_hint: otter_vm::jit::JitMethodHint::None,
                load_number: None,
                property_feedback: None,
                property_feedback_poly: Vec::new(),
                property_proto_feedback: None,
                object_literal: None,
                element_load_kind: otter_vm::jit::JitElementLoadKind::Any,
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
            string_layout: otter_vm::JitStringLayout::default(),
            object_shape_byte: 8,
            object_values_ptr_byte: 16,
            object_inline_values_byte: 80,
            object_slab_len_byte: 88,
            object_inline_slot_cap: 2,
            gc_barrier: Default::default(),
            jit_proto_byte: 12,
            heap_number_type_tag: 0x30,
            heap_number_bits_byte: 8,
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
            primitive_method_guards: Default::default(),
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
                        method_hint: otter_vm::jit::JitMethodHint::None,
                        load_number: None,
                        property_feedback: None,
                        property_feedback_poly: Vec::new(),
                        property_proto_feedback: None,
                        object_literal: None,
                        element_load_kind: otter_vm::jit::JitElementLoadKind::Any,
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
                        method_hint: otter_vm::jit::JitMethodHint::None,
                        load_number: None,
                        property_feedback: None,
                        property_feedback_poly: Vec::new(),
                        property_proto_feedback: None,
                        object_literal: None,
                        element_load_kind: otter_vm::jit::JitElementLoadKind::Any,
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
                    method_hint: otter_vm::jit::JitMethodHint::None,
                    load_number: None,
                    property_feedback: None,
                    property_feedback_poly: Vec::new(),
                    property_proto_feedback: None,
                    object_literal: None,
                    element_load_kind: otter_vm::jit::JitElementLoadKind::Any,
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
                    method_hint: otter_vm::jit::JitMethodHint::None,
                    load_number: None,
                    property_feedback: None,
                    property_feedback_poly: Vec::new(),
                    property_proto_feedback: None,
                    object_literal: None,
                    element_load_kind: otter_vm::jit::JitElementLoadKind::Any,
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
                    method_hint: otter_vm::jit::JitMethodHint::None,
                    load_number: None,
                    property_feedback: None,
                    property_feedback_poly: Vec::new(),
                    property_proto_feedback: None,
                    object_literal: None,
                    element_load_kind: otter_vm::jit::JitElementLoadKind::Any,
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

    #[test]
    fn no_site_method_call_lowers_to_runtime_method_call() {
        let v = view(
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

        let g = build_full(&v).expect("no-site generic method call builds");
        assert_eq!(
            count_kind(&g, |k| matches!(k, NodeKind::CallMethod { site: None, .. })),
            1
        );
    }

    #[test]
    fn tagged_method_branch_inserts_bool_guard() {
        let mut v = view(
            1,
            4,
            &[
                (
                    Op::CallMethodValue,
                    vec![r(1), r(0), Operand::ConstIndex(0), Operand::ConstIndex(0)],
                    0,
                ),
                (Op::JumpIfFalse, vec![imm(rel(1, 4)), r(1)], 0),
                (Op::LoadInt32, vec![r(2), imm(1)], 0),
                (Op::ReturnValue, vec![r(2)], 0),
                (Op::LoadInt32, vec![r(3), imm(0)], 0),
                (Op::ReturnValue, vec![r(3)], 0),
            ],
        );
        v.instructions[0].property_ic_site = Some(7);

        let g = build_full(&v).expect("method-result branch builds");
        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::CheckBool(_))), 1);
        assert_eq!(
            count_kind(&g, |k| matches!(k, NodeKind::CallMethod { .. })),
            1
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
            method_hint: otter_vm::jit::JitMethodHint::None,
            load_number: None,
            property_feedback: None,
            property_feedback_poly: Vec::new(),
            property_proto_feedback: None,
            object_literal: None,
            element_load_kind: otter_vm::jit::JitElementLoadKind::Any,
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
    fn missing_inline_method_inside_loop_lowers_to_runtime_method_call() {
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

        let g = build_full(&v).expect("loop method call builds");
        assert_eq!(
            count_kind(&g, |k| matches!(k, NodeKind::CallMethod { .. })),
            1
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
    fn string_add_lowers_to_concat_stub_node() {
        let g = build_full(&view(
            2,
            3,
            &[
                (Op::Add, vec![r(2), r(0), r(1)], ARITH_STRING),
                (Op::ReturnValue, vec![r(2)], 0),
            ],
        ))
        .expect("string add builds");

        assert_eq!(
            count_kind(&g, |k| matches!(k, NodeKind::StringConcat { .. })),
            1
        );
        assert_eq!(count_kind(&g, |k| matches!(k, NodeKind::Int32Add(..))), 0);
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
        // Unary numeric negation is outside the optimizing subset and bails the
        // function.
        let err = build_full(&view(
            2,
            4,
            &[
                (Op::Neg, vec![r(2), r(0)], ARITH_INT32),
                (Op::ReturnValue, vec![r(2)], 0),
            ],
        ))
        .unwrap_err();
        assert_eq!(err, Unsupported::Opcode(Op::Neg));
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
    fn epilogue_deopt_materializes_loop_carried_diamond_phi() {
        let v = view(
            0,
            6,
            &[
                (Op::LoadInt32, vec![r(0), imm(0)], 0),
                (Op::LoadInt32, vec![r(1), imm(0)], 0),
                (Op::LoadInt32, vec![r(2), imm(10)], 0),
                (Op::LessThan, vec![r(3), r(1), r(2)], ARITH_INT32),
                (Op::JumpIfFalse, vec![imm(rel(4, 10)), r(3)], 0),
                (Op::LoadTrue, vec![r(4)], 0),
                (Op::JumpIfFalse, vec![imm(rel(6, 8)), r(4)], 0),
                (Op::Increment, vec![r(0), r(0), imm(1)], ARITH_INT32),
                (Op::Increment, vec![r(1), r(1), imm(1)], ARITH_INT32),
                (Op::Jump, vec![imm(rel(9, 3))], 0),
                (Op::LoadGlobalThis, vec![r(5)], 0),
                (Op::ReturnValue, vec![r(0)], 0),
            ],
        );
        let g = build_full(&v).expect("epilogue deopt graph builds");
        let bcl = deopt::bytecode_liveness(&v);
        let deopts = deopt::capture_deopt_terminators(&g, &bcl);
        let epilogue = g
            .blocks
            .iter()
            .position(|b| b.start_pc == 10 * STRIDE)
            .expect("epilogue block") as BlockId;
        let point = deopts.get(&epilogue).expect("epilogue deopt state");
        let (_, acc) = point
            .top()
            .registers
            .iter()
            .find(|&&(reg, _)| reg == 0)
            .copied()
            .expect("acc is live at epilogue deopt");
        assert!(
            matches!(g.node(acc).kind, NodeKind::Phi(ref inputs) if inputs.len() == 2),
            "epilogue deopt must restore the loop-carried acc phi, got {:?}",
            g.node(acc).kind
        );
    }

    #[test]
    fn osr_target_skips_unsupported_earlier_loop() {
        // A setup loop contains an unsupported unary negation, then execution
        // reaches a later simple hot loop. Whole-function compilation must
        // decline the setup loop, but an OSR-target compile rooted at the later
        // header should build only the region reachable from that header.
        let v = view(
            1,
            8,
            &[
                (Op::LoadInt32, vec![r(1), imm(0)], 0),
                (Op::LessThan, vec![r(2), r(1), r(0)], ARITH_INT32),
                (Op::JumpIfFalse, vec![imm(rel(2, 6)), r(2)], 0),
                (Op::Neg, vec![r(3), r(1)], ARITH_INT32),
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
            Unsupported::Opcode(Op::Neg),
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
                (Op::Neg, vec![r(3), r(1)], ARITH_INT32),
                (Op::Increment, vec![r(1), r(1), imm(1)], ARITH_INT32),
                (Op::Jump, vec![imm(rel(6, 2))], 0),
                (Op::ReturnValue, vec![r(1)], 0),
            ],
        );

        assert_eq!(
            build(&v, Some(2 * STRIDE)).unwrap_err(),
            Unsupported::Opcode(Op::Neg)
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
