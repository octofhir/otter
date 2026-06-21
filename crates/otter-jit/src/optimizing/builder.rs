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
use otter_vm::JitFunctionView;
use otter_vm::jit_feedback::ArithFeedback;
use rustc_hash::FxHashMap;

use super::Unsupported;
use super::ir::{BlockId, CmpOp, Graph, NodeId, NodeKind, Repr, Terminator};

/// Build a typed SSA graph for `view`, or report why the function is outside the
/// optimizing subset.
pub(super) fn build(view: &JitFunctionView) -> Result<Graph, Unsupported> {
    if view.instructions.is_empty() {
        return Err(Unsupported::Empty);
    }
    let cfg = Cfg::discover(view)?;
    let mut builder = Builder::new(view, cfg);
    builder.run()?;
    Ok(builder.graph)
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
}

/// SSA construction state.
struct Builder<'a> {
    view: &'a JitFunctionView,
    cfg: Cfg,
    graph: Graph,
    /// `current_def[reg][block]` — the SSA node defining `reg` at the end of (or
    /// within) `block`.
    current_def: Vec<FxHashMap<BlockId, NodeId>>,
    /// Phis created in still-unsealed blocks, pending operand fill-in at seal
    /// time: `block → [(register, phi node)]`.
    incomplete_phis: FxHashMap<BlockId, Vec<(u16, NodeId)>>,
}

impl<'a> Builder<'a> {
    fn new(view: &'a JitFunctionView, cfg: Cfg) -> Self {
        // Materialize one block per CFG range (the graph starts with just the
        // entry block).
        let mut graph = Graph::new(view.param_count, view.register_count);
        graph.blocks.clear();
        for &(start, _) in &cfg.ranges {
            let pc = view.instructions[start].byte_pc;
            graph.blocks.push(super::ir::Block::new(pc));
        }
        for (b, p) in cfg.preds.iter().enumerate() {
            graph.blocks[b].preds = p.clone();
        }
        let reg_count = view.register_count as usize;
        Self {
            view,
            cfg,
            graph,
            current_def: vec![FxHashMap::default(); reg_count],
            incomplete_phis: FxHashMap::default(),
        }
    }

    fn run(&mut self) -> Result<(), Unsupported> {
        // Entry block: every register starts as a parameter (first
        // `param_count`) or `undefined` (locals / scratch), so reading any
        // register always terminates even on a path that never assigned it.
        for r in 0..self.view.register_count {
            let kind = if r < self.view.param_count {
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
            self.seal_ready();
            self.fill_block(b as BlockId)?;
            self.graph.blocks[b].filled = true;
            self.seal_ready();
        }
        self.seal_ready();
        debug_assert!(self.graph.blocks.iter().all(|blk| blk.sealed));
        Ok(())
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
            let property_feedback = instr.property_feedback;
            let operands = instr.operands.clone();
            match op {
                // The leading named-function self-binding. The closure value is
                // never a numeric operand in this subset; only `make_self`
                // (no allocation) is accepted — any other `MakeFunction`
                // allocates and bails to the baseline.
                Op::MakeFunction if make_self => {
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
                    // Inline slot access decompresses object pointers against the
                    // baked GC cage base; without it the layout offsets are absent.
                    let Some((shape, slot_byte)) =
                        property_feedback.filter(|_| self.view.cage_base != 0)
                    else {
                        return Err(Unsupported::Opcode(Op::LoadProperty));
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
                // `StoreSlot`, but only for a primitive (int32 / f64 / bool)
                // value: a primitive `Value` is never a `Gc` pointer, so no
                // generational write barrier is needed (the optimizing tier has no
                // safepoint to run one). A pointer-valued store, or a non-own-data
                // site, bails to the baseline.
                Op::StoreProperty => {
                    let obj_reg = reg(&operands, 0)?;
                    let src_reg = reg(&operands, 2)?;
                    let Some((shape, slot_byte)) =
                        property_feedback.filter(|_| self.view.cage_base != 0)
                    else {
                        return Err(Unsupported::Opcode(Op::StoreProperty));
                    };
                    let value = self.read_variable(src_reg, block);
                    if !matches!(
                        self.graph.node(value).kind.repr(),
                        Repr::Int32 | Repr::Float64
                    ) {
                        return Err(Unsupported::Opcode(Op::StoreProperty));
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
                Op::LoadUndefined => {
                    let dst = reg(&operands, 0)?;
                    let node = self
                        .graph
                        .add_node(NodeKind::ConstUndefined, block, byte_pc);
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
                    let node = self.arith_binop(block, op, lhs, rhs, feedback, byte_pc)?;
                    self.graph.set_frame_dst(node, dst);
                    self.push_body(block, node);
                    self.def_register(dst, block, node, byte_pc);
                }
                // Bitwise / shift sites are integer ops: JS coerces both operands
                // to int32 and the result is int32 (`|`, `&`, `^`, `<<`, `>>`
                // all stay in signed-int32 range). Speculate int32 operands from
                // feedback (`CheckInt32`); a non-int32 site bails. `>>>` is absent
                // — its result can exceed the int32 range (a double).
                Op::BitwiseOr | Op::BitwiseAnd | Op::BitwiseXor | Op::Shl | Op::Shr => {
                    let (dst, lhs, rhs) =
                        (reg(&operands, 0)?, reg(&operands, 1)?, reg(&operands, 2)?);
                    let node = self.bitwise_binop(block, op, lhs, rhs, feedback, byte_pc)?;
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
                    let node = self.increment(block, src, delta, feedback, byte_pc)?;
                    self.graph.set_frame_dst(node, dst);
                    self.push_body(block, node);
                    self.def_register(dst, block, node, byte_pc);
                }
                Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Equal
                | Op::NotEqual => {
                    let (dst, lhs, rhs) =
                        (reg(&operands, 0)?, reg(&operands, 1)?, reg(&operands, 2)?);
                    let cmp = CmpOp::from_op(op).expect("comparison opcode");
                    let node = self.compare(block, cmp, lhs, rhs, feedback, byte_pc)?;
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
                    if self.graph.node(cond_node).kind.repr() != Repr::Bool {
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
                other => return Err(Unsupported::Opcode(other)),
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
    fn bitwise_binop(
        &mut self,
        block: BlockId,
        op: Op,
        lhs: u16,
        rhs: u16,
        feedback: u8,
        byte_pc: u32,
    ) -> Result<NodeId, Unsupported> {
        if !ArithFeedback::from_bits(feedback).is_int32_only() {
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
            _ => unreachable!("bitwise_binop on non-bitwise op"),
        };
        Ok(self.graph.add_node(kind, block, byte_pc))
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
        if fb.is_int32_only() && !matches!(op, Op::Div) {
            return self.int32_binop(block, op, lhs, rhs, feedback, byte_pc);
        }
        if !fb.is_numeric_only() {
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
        byte_pc: u32,
    ) -> Result<NodeId, Unsupported> {
        let fb = ArithFeedback::from_bits(feedback);
        if fb.is_int32_only() {
            let l = self.int32_operand(block, lhs, feedback, byte_pc)?;
            let r = self.int32_operand(block, rhs, feedback, byte_pc)?;
            return Ok(self
                .graph
                .add_node(NodeKind::Int32Compare(cmp, l, r), block, byte_pc));
        }
        if !fb.is_numeric_only() {
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
        if fb.is_int32_only() {
            let s = self.int32_operand(block, src, feedback, byte_pc)?;
            let step = self
                .graph
                .add_node(NodeKind::ConstInt32(delta), block, byte_pc);
            self.push_body(block, step);
            return Ok(self
                .graph
                .add_node(NodeKind::Int32Add(s, step), block, byte_pc));
        }
        if !fb.is_numeric_only() {
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

    /// Resolve a register operand to an [`Repr::Int32`] node, inserting a
    /// `CheckInt32` guard when the value is still tagged. A site whose feedback
    /// proves it is not int32-only bails the function.
    fn int32_operand(
        &mut self,
        block: BlockId,
        operand_reg: u16,
        raw_feedback: u8,
        byte_pc: u32,
    ) -> Result<NodeId, Unsupported> {
        let feedback = ArithFeedback::from_bits(raw_feedback);
        if !feedback.is_int32_only() {
            return Err(Unsupported::TypeFeedback(raw_feedback));
        }
        let node = self.read_variable(operand_reg, block);
        if self.graph.node(node).repr == Repr::Int32 {
            return Ok(node);
        }
        let check = self
            .graph
            .add_node(NodeKind::CheckInt32(node), block, byte_pc);
        self.push_body(block, check);
        Ok(check)
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

    fn push_body(&mut self, block: BlockId, node: NodeId) {
        self.graph.blocks[block as usize].body.push(node);
    }

    fn set_term(&mut self, block: BlockId, term: Terminator) {
        self.graph.blocks[block as usize].term = Some(term);
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
            jit_proto_byte: 12,
            closure_fid_byte: 8,
            instructions,
            inline_callees: Default::default(),
            inline_methods: Default::default(),
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

    #[test]
    fn straight_line_int_add_guards_param_only() {
        // r2 = r0 + 1; return r2   (r0 is a parameter, 1 is an int32 const)
        let g = build(&view(
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
    fn diamond_merge_inserts_phi() {
        // if (r0 < r1) r2 = 1 else r2 = 2; return r2
        //  0 LessThan r2, r0, r1
        //  1 JumpIfFalse ->else(4), r2
        //  2 LoadInt32 r2, 1
        //  3 Jump ->merge(5)
        //  4 LoadInt32 r2, 2          (else)
        //  5 ReturnValue r2           (merge)
        let g = build(&view(
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
        let g = build(&view(
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
        let err = build(&view(
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
    fn div_lowers_to_float() {
        // `Div` always takes the float path (int32 operands still yield a
        // non-integer result), so its operands are widened/checked to f64 and
        // the node is a `Float64Div`.
        let g = build(&view(
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
        let g = build(&view(
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
        let err = build(&view(
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
