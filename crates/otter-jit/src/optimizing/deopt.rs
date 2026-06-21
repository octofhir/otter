//! Deoptimization support for the optimizing tier.
//!
//! Exact-PC deopt needs to know, at every speculation guard, which interpreter
//! registers must be reconstructed before resuming the bytecode — i.e. the
//! registers the interpreter will *read* at (or after) the guard's byte-PC. This
//! module computes that **bytecode-register liveness**: a backward dataflow over
//! the function's instruction stream giving, per byte-PC, the set of registers
//! live just before that instruction executes.
//!
//! Pruning the deopt frame state to exactly these live registers is essential:
//! capturing every register at every guard would keep every value artificially
//! live for the register allocator (each guard would use the whole frame),
//! exploding register pressure. Restoring only the live registers keeps frame
//! states minimal — the same choice V8 Maglev makes with its per-bytecode
//! liveness.
//!
//! # Contents
//! - [`bytecode_liveness`] — per-byte-PC live-before register sets.
//!
//! # Invariants
//! - Operates only over the optimizing subset's opcodes (the tier compiles a
//!   function only when every opcode is supported), so the def/use model here is
//!   exhaustive for any function that reaches it.
//! - Conservative is safe: a register wrongly kept live merely restores an extra
//!   value at deopt; a register wrongly dropped is a correctness bug. The
//!   dataflow therefore over-approximates on any opcode it does not model
//!   (none today) by treating it as using nothing and defining nothing — which
//!   would surface as a builder `Unsupported` long before here.
//!
//! # See also
//! - [`super::liveness`] — SSA-value liveness (a different problem: values, not
//!   bytecode registers).

use otter_bytecode::{Op, Operand};
use otter_vm::JitFunctionView;
use rustc_hash::{FxHashMap, FxHashSet};

use super::ir::{BlockId, Graph, NodeId, NodeKind};
use super::liveness::successors as block_successors;

/// Registers an instruction defines (writes) and uses (reads).
struct RegEffects {
    defs: Vec<u16>,
    uses: Vec<u16>,
}

fn reg(operands: &[Operand], i: usize) -> Option<u16> {
    match operands.get(i) {
        Some(Operand::Register(r)) => Some(*r),
        _ => None,
    }
}

/// A register index encoded as an inline immediate (`LoadLocal` / `StoreLocal`
/// address the register window by an `Imm32` index).
fn reg_imm(operands: &[Operand], i: usize) -> Option<u16> {
    match operands.get(i) {
        Some(Operand::Imm32(v)) => u16::try_from(*v).ok(),
        _ => None,
    }
}

/// Register reads / writes of one instruction, for the optimizing subset.
fn reg_effects(op: Op, operands: &[Operand]) -> RegEffects {
    let mut defs = Vec::new();
    let mut uses = Vec::new();
    match op {
        Op::LoadInt32
        | Op::LoadNumber
        | Op::LoadTrue
        | Op::LoadFalse
        | Op::LoadUndefined
        | Op::LoadThis
        | Op::LoadHole
        | Op::LoadUpvalue
        | Op::MakeFunction => {
            if let Some(d) = reg(operands, 0) {
                defs.push(d);
            }
        }
        // `LoadLocal dst, srcIdx` reads register `srcIdx`, writes `dst`.
        Op::LoadLocal => {
            if let Some(d) = reg(operands, 0) {
                defs.push(d);
            }
            if let Some(s) = reg_imm(operands, 1) {
                uses.push(s);
            }
        }
        // `StoreLocal src, dstIdx` reads `src`, writes register `dstIdx`.
        Op::StoreLocal => {
            if let Some(s) = reg(operands, 0) {
                uses.push(s);
            }
            if let Some(d) = reg_imm(operands, 1) {
                defs.push(d);
            }
        }
        // `ToPrimitive dst, src` / `ToNumeric dst, src` / `Increment dst, src,
        // delta` all write `dst` and read `src` (the inline delta is an
        // immediate, not a register).
        // `ToPrimitive dst, src` / `ToNumeric dst, src` / `Increment dst, src,
        // delta` / `LoadProperty dst, obj, name` all write `dst` and read the
        // second register operand.
        Op::ToPrimitive | Op::ToNumeric | Op::Increment | Op::LoadProperty => {
            if let Some(d) = reg(operands, 0) {
                defs.push(d);
            }
            if let Some(s) = reg(operands, 1) {
                uses.push(s);
            }
        }
        // `LoadElement dst, recv, idx` reads receiver and computed key, writes
        // the destination. The optimizing fast path may deopt at the load, so
        // the live-before state must contain the pre-instruction operands.
        Op::LoadElement => {
            if let Some(d) = reg(operands, 0) {
                defs.push(d);
            }
            if let Some(o) = reg(operands, 1) {
                uses.push(o);
            }
            if let Some(i) = reg(operands, 2) {
                uses.push(i);
            }
        }
        // `StoreElement recv, idx, src, scratch` reads the receiver, computed
        // key, and stored value; on a deopt miss the interpreter re-runs the
        // store at the same byte-PC.
        Op::StoreElement => {
            if let Some(o) = reg(operands, 0) {
                uses.push(o);
            }
            if let Some(i) = reg(operands, 1) {
                uses.push(i);
            }
            if let Some(s) = reg(operands, 2) {
                uses.push(s);
            }
            if let Some(d) = reg(operands, 3) {
                defs.push(d);
            }
        }
        // `StoreProperty obj, name, src, scratch` reads the receiver and the
        // stored value and clobbers the scratch register.
        Op::StoreProperty => {
            if let Some(o) = reg(operands, 0) {
                uses.push(o);
            }
            if let Some(s) = reg(operands, 2) {
                uses.push(s);
            }
            if let Some(d) = reg(operands, 3) {
                defs.push(d);
            }
        }
        Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Div
        | Op::BitwiseOr
        | Op::BitwiseAnd
        | Op::BitwiseXor
        | Op::Shl
        | Op::Shr
        | Op::LessThan
        | Op::LessEq
        | Op::GreaterThan
        | Op::GreaterEq
        | Op::Equal
        | Op::NotEqual => {
            if let Some(d) = reg(operands, 0) {
                defs.push(d);
            }
            if let Some(l) = reg(operands, 1) {
                uses.push(l);
            }
            if let Some(r) = reg(operands, 2) {
                uses.push(r);
            }
        }
        // `JumpIf* rel, cond` reads the condition register.
        Op::JumpIfTrue | Op::JumpIfFalse => {
            if let Some(c) = reg(operands, 1) {
                uses.push(c);
            }
        }
        Op::Return | Op::ReturnValue => {
            if let Some(s) = reg(operands, 0) {
                uses.push(s);
            }
        }
        Op::Call => {
            if let Some(d) = reg(operands, 0) {
                defs.push(d);
            }
            if let Some(c) = reg(operands, 1) {
                uses.push(c);
            }
            let argc = match operands.get(2) {
                Some(Operand::ConstIndex(n)) => *n as usize,
                _ => 0,
            };
            for slot in 0..argc {
                if let Some(a) = reg(operands, 3 + slot) {
                    uses.push(a);
                }
            }
        }
        Op::CallMethodValue => {
            if let Some(d) = reg(operands, 0) {
                defs.push(d);
            }
            if let Some(r) = reg(operands, 1) {
                uses.push(r);
            }
            let argc = match operands.get(3) {
                Some(Operand::ConstIndex(n)) => *n as usize,
                _ => 0,
            };
            for slot in 0..argc {
                if let Some(a) = reg(operands, 4 + slot) {
                    uses.push(a);
                }
            }
        }
        // No register effects.
        Op::Jump | Op::ReturnUndefined => {}
        // Any opcode outside the subset would have failed the builder; model it
        // as no-effect (conservative — never drops a live register).
        _ => {}
    }
    RegEffects { defs, uses }
}

/// Byte-PC successors of the instruction at index `i` (fallthrough + branch
/// target), as instruction byte-PCs.
fn successors(view: &JitFunctionView, i: usize, index_of_pc: &FxHashMap<u32, usize>) -> Vec<u32> {
    let instr = &view.instructions[i];
    let next_pc = view.instructions.get(i + 1).map(|n| n.byte_pc);
    let branch = |slot: usize| -> Option<u32> {
        match instr.operands.get(slot) {
            Some(Operand::Imm32(rel)) => {
                let target = i64::from(instr.byte_pc) + 1 + i64::from(*rel);
                u32::try_from(target)
                    .ok()
                    .filter(|pc| index_of_pc.contains_key(pc))
            }
            _ => None,
        }
    };
    match instr.op {
        Op::Jump => branch(0).into_iter().collect(),
        Op::JumpIfTrue | Op::JumpIfFalse => branch(0).into_iter().chain(next_pc).collect(),
        Op::Return | Op::ReturnValue | Op::ReturnUndefined => Vec::new(),
        _ => next_pc.into_iter().collect(),
    }
}

/// Backward dataflow giving, per instruction byte-PC, the set of registers live
/// just before the instruction executes (`live_before`). The deopt frame state
/// at a guard serving byte-PC `P` restores exactly `live_before[P]`.
#[must_use]
pub fn bytecode_liveness(view: &JitFunctionView) -> FxHashMap<u32, FxHashSet<u16>> {
    let index_of_pc: FxHashMap<u32, usize> = view
        .instructions
        .iter()
        .enumerate()
        .map(|(i, instr)| (instr.byte_pc, i))
        .collect();

    let n = view.instructions.len();
    let effects: Vec<RegEffects> = view
        .instructions
        .iter()
        .map(|instr| reg_effects(instr.op, &instr.operands))
        .collect();

    let mut live_before: Vec<FxHashSet<u16>> = vec![FxHashSet::default(); n];

    // Iterate to a fixpoint, processing instructions back-to-front for fast
    // convergence on forward control flow (loops still settle via re-iteration).
    let mut changed = true;
    while changed {
        changed = false;
        for i in (0..n).rev() {
            // live_after = union of successors' live_before.
            let mut after: FxHashSet<u16> = FxHashSet::default();
            for s in successors(view, i, &index_of_pc) {
                if let Some(&si) = index_of_pc.get(&s) {
                    after.extend(live_before[si].iter().copied());
                }
            }
            // live_before = uses ∪ (live_after \ defs).
            let mut before = after;
            for d in &effects[i].defs {
                before.remove(d);
            }
            for u in &effects[i].uses {
                before.insert(*u);
            }
            if before != live_before[i] {
                live_before[i] = before;
                changed = true;
            }
        }
    }

    view.instructions
        .iter()
        .enumerate()
        .map(|(i, instr)| (instr.byte_pc, std::mem::take(&mut live_before[i])))
        .collect()
}

/// One guard's deopt frame state: the bytecode PC to resume the interpreter at,
/// and the SSA value to box and store into each live interpreter register before
/// resuming. Built by [`capture_frame_states`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeoptPoint {
    /// Bytecode byte-PC the interpreter resumes at when this guard fails.
    pub byte_pc: u32,
    /// `(register, value)` pairs for every register live at `byte_pc`, ascending
    /// by register. The deopt exit materializes each value (boxed to its tagged
    /// representation) into the frame slot for that register.
    pub registers: Vec<(u16, NodeId)>,
}

/// Reverse-postorder block ordering of `graph` (iterative DFS from the entry).
fn reverse_postorder(graph: &Graph) -> Vec<BlockId> {
    let n = graph.blocks.len();
    let mut visited = vec![false; n];
    let mut post = Vec::with_capacity(n);
    let mut stack: Vec<(BlockId, usize)> = vec![(graph.entry, 0)];
    visited[graph.entry as usize] = true;
    while let Some(&(block, idx)) = stack.last() {
        let succs = block_successors(graph, block);
        if idx < succs.len() {
            stack.last_mut().unwrap().1 += 1;
            let s = succs[idx];
            if !visited[s as usize] {
                visited[s as usize] = true;
                stack.push((s, 0));
            }
        } else {
            post.push(block);
            stack.pop();
        }
    }
    post.reverse();
    post
}

/// Whether a node can deoptimize and therefore needs a captured frame state: a
/// speculation guard (`CheckInt32` / `CheckNumber`) or an arithmetic node that
/// deopts on int32 overflow (`Int32Add` / `Int32Sub` / `Int32Mul`). An overflow
/// resumes the interpreter at the arithmetic instruction's byte-PC, so it needs
/// the same live-register frame state as a guard does. Float arithmetic is total
/// (IEEE) and never deopts, so the `Float64*` nodes are absent here.
fn can_deopt(kind: &NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::CheckInt32(_)
            | NodeKind::CheckNumber(_)
            | NodeKind::CheckShape(_, _)
            | NodeKind::LoadUpvalue(_)
            | NodeKind::Call { .. }
            | NodeKind::LoadElement(_, _)
            | NodeKind::StoreElement(_, _, _)
            | NodeKind::LoadArrayLength(_)
            | NodeKind::LoadThis
            | NodeKind::Int32Add(_, _)
            | NodeKind::Int32Sub(_, _)
            | NodeKind::Int32Mul(_, _)
    )
}

/// Capture the deopt frame state at every node that can deoptimize: a speculation
/// guard (`CheckInt32` / `CheckNumber`) or an overflowing int32 arithmetic node.
///
/// Reconstructs, per block in RPO, the interpreter environment (register → SSA
/// value) by inheriting from a predecessor and overlaying header phis, then
/// walks the block recording, at each deopt-capable node, the value of every
/// register live at its byte-PC (per [`bytecode_liveness`]). A register live at a
/// merge necessarily flows through a phi (or is unchanged across all
/// predecessors), so the captured value is always correct for the live set.
///
/// The environment overlay is applied *before* recording at a node so an
/// arithmetic node's own freshly-written destination register restores to a
/// pre-instruction value (the operands), exactly matching what the interpreter
/// re-reads when it re-executes that instruction after the bail.
#[must_use]
pub fn capture_frame_states(
    graph: &Graph,
    bytecode_live: &FxHashMap<u32, FxHashSet<u16>>,
) -> FxHashMap<NodeId, DeoptPoint> {
    let rc = graph.register_count as usize;
    let rpo = reverse_postorder(graph);
    // The interpreter environment at each block's exit, filled as RPO advances.
    let mut exit_env: Vec<Option<Vec<Option<NodeId>>>> = vec![None; graph.blocks.len()];
    let mut points: FxHashMap<NodeId, DeoptPoint> = FxHashMap::default();

    for &b in &rpo {
        let block = graph.block(b);
        // Block-entry environment.
        let mut env: Vec<Option<NodeId>> = if b == graph.entry {
            // The builder seeds the entry block's first `register_count` body
            // nodes as the per-register Param / undefined definitions, in order.
            (0..rc)
                .map(|r| graph.block(graph.entry).body.get(r).copied())
                .collect()
        } else {
            // Inherit a computed predecessor's exit environment (a forward edge
            // is always available in RPO), then overlay this block's phis: a
            // register that genuinely varies across the merge has a phi here;
            // one that does not is identical in every predecessor.
            let mut e = block
                .preds
                .iter()
                .find_map(|&p| exit_env[p as usize].clone())
                .unwrap_or_else(|| vec![None; rc]);
            for &phi in &block.phis {
                if let Some(&r) = graph.phi_reg.get(&phi)
                    && (r as usize) < rc
                {
                    e[r as usize] = Some(phi);
                }
            }
            e
        };

        // The block's register rebinds, in execution order (byte_pc ascending).
        // Replaying these — rather than only `frame_dst` — captures `LoadLocal` /
        // `StoreLocal` aliasing, so the env reflects exactly which SSA value each
        // interpreter register holds, matching what the resumed bytecode reads.
        let empty = Vec::new();
        let writes = graph.reg_writes.get(&b).unwrap_or(&empty);
        let mut wi = 0usize;

        for &nid in &block.body {
            let node = graph.node(nid);
            if can_deopt(&node.kind) {
                let pc = node.byte_pc;
                // Apply every register rebind from an instruction strictly before
                // this guard's instruction. A rebind at the guard's own `pc` is
                // that instruction's def, which `live_before[pc]` excludes — the
                // interpreter re-executes the instruction, so its def is restored
                // by re-running, not by the frame state.
                while wi < writes.len() && writes[wi].0 < pc {
                    let (_, r, v) = writes[wi];
                    if (r as usize) < rc {
                        env[r as usize] = Some(v);
                    }
                    wi += 1;
                }
                let mut registers: Vec<(u16, NodeId)> = Vec::new();
                if let Some(live) = bytecode_live.get(&pc) {
                    let mut regs: Vec<u16> = live.iter().copied().collect();
                    regs.sort_unstable();
                    for r in regs {
                        if let Some(Some(v)) = env.get(r as usize) {
                            registers.push((r, *v));
                        }
                    }
                }
                points.entry(nid).or_insert(DeoptPoint {
                    byte_pc: pc,
                    registers,
                });
            }
        }
        // Apply the remaining rebinds to form the block-exit environment that
        // successors inherit.
        while wi < writes.len() {
            let (_, r, v) = writes[wi];
            if (r as usize) < rc {
                env[r as usize] = Some(v);
            }
            wi += 1;
        }
        exit_env[b as usize] = Some(env);
    }
    points
}

/// An on-stack-replacement entry point: a loop-header block the interpreter can
/// jump into mid-loop, with the SSA value each live interpreter register must be
/// reloaded into.
#[derive(Clone, Debug)]
pub struct OsrEntry {
    /// Loop-header byte-PC the interpreter resumes into (its `frame.pc` at the
    /// backward branch). The OSR trampoline is keyed by this.
    pub byte_pc: u32,
    /// The header block the trampoline branches to after reloading registers.
    pub block: BlockId,
    /// `(register, value)` for every register live at the header entry, ascending
    /// by register. The trampoline loads the interpreter frame slot `[x19, r*8]`
    /// into the home the allocator gave `value` (unboxing for a typed home).
    pub registers: Vec<(u16, NodeId)>,
}

/// Capture an [`OsrEntry`] for every loop header (a block with a back-edge
/// predecessor). Reconstructs the interpreter environment (register → SSA value)
/// at each block entry exactly as [`capture_frame_states`] does — inherit a
/// forward predecessor's exit env, overlay the block's header phis — and, for a
/// loop header, records the value of every register live at the header. This is
/// the inverse of a deopt frame state: deopt writes live homes out to the
/// interpreter frame; OSR reads the interpreter frame back into live homes.
#[must_use]
pub fn capture_osr_entries(
    graph: &Graph,
    bytecode_live: &FxHashMap<u32, FxHashSet<u16>>,
) -> Vec<OsrEntry> {
    let rc = graph.register_count as usize;
    let rpo = reverse_postorder(graph);
    let mut exit_env: Vec<Option<Vec<Option<NodeId>>>> = vec![None; graph.blocks.len()];
    let mut entries: Vec<OsrEntry> = Vec::new();

    for &b in &rpo {
        let block = graph.block(b);
        let mut env: Vec<Option<NodeId>> = if b == graph.entry {
            (0..rc)
                .map(|r| graph.block(graph.entry).body.get(r).copied())
                .collect()
        } else {
            let mut e = block
                .preds
                .iter()
                .find_map(|&p| exit_env[p as usize].clone())
                .unwrap_or_else(|| vec![None; rc]);
            for &phi in &block.phis {
                if let Some(&r) = graph.phi_reg.get(&phi)
                    && (r as usize) < rc
                {
                    e[r as usize] = Some(phi);
                }
            }
            e
        };

        // A loop header is the target of a back edge: a predecessor whose block
        // begins later in the bytecode (the block that ends in the backward
        // branch). The interpreter's backedge counter fires at exactly this pc.
        // Every loop header (inner loops of a nest included) gets a trampoline —
        // the reload set is the header's own phis plus its live-in invariants, so
        // an inner header correctly re-seeds the enclosing loops' induction state.
        let is_loop_header = block
            .preds
            .iter()
            .any(|&p| graph.block(p).start_pc > block.start_pc);
        if is_loop_header {
            let pc = block.start_pc;
            let mut registers: Vec<(u16, NodeId)> = Vec::new();
            if let Some(live) = bytecode_live.get(&pc) {
                let mut regs: Vec<u16> = live.iter().copied().collect();
                regs.sort_unstable();
                for r in regs {
                    if let Some(Some(v)) = env.get(r as usize) {
                        registers.push((r, *v));
                    }
                }
            }
            entries.push(OsrEntry {
                byte_pc: pc,
                block: b,
                registers,
            });
        }

        let empty = Vec::new();
        let writes = graph.reg_writes.get(&b).unwrap_or(&empty);
        for &(_, r, v) in writes {
            if (r as usize) < rc {
                env[r as usize] = Some(v);
            }
        }
        exit_env[b as usize] = Some(env);
    }
    entries
}

/// Capture the deopt frame state at every block ending in a
/// [`Terminator::Deopt`] (an instruction outside the optimizing subset). Like
/// [`capture_osr_entries`], reconstructs the register → SSA value environment per
/// block and records the value of every register live at the deopt byte-PC.
/// Keyed by the deopting block.
#[must_use]
pub fn capture_deopt_terminators(
    graph: &Graph,
    bytecode_live: &FxHashMap<u32, FxHashSet<u16>>,
) -> FxHashMap<BlockId, DeoptPoint> {
    use super::ir::Terminator;
    let rc = graph.register_count as usize;
    let rpo = reverse_postorder(graph);
    let mut exit_env: Vec<Option<Vec<Option<NodeId>>>> = vec![None; graph.blocks.len()];
    let mut out: FxHashMap<BlockId, DeoptPoint> = FxHashMap::default();

    for &b in &rpo {
        let block = graph.block(b);
        let mut env: Vec<Option<NodeId>> = if b == graph.entry {
            (0..rc)
                .map(|r| graph.block(graph.entry).body.get(r).copied())
                .collect()
        } else {
            let mut e = block
                .preds
                .iter()
                .find_map(|&p| exit_env[p as usize].clone())
                .unwrap_or_else(|| vec![None; rc]);
            for &phi in &block.phis {
                if let Some(&r) = graph.phi_reg.get(&phi)
                    && (r as usize) < rc
                {
                    e[r as usize] = Some(phi);
                }
            }
            e
        };
        let empty = Vec::new();
        let writes = graph.reg_writes.get(&b).unwrap_or(&empty);
        for &(_, r, v) in writes {
            if (r as usize) < rc {
                env[r as usize] = Some(v);
            }
        }
        if let Some(Terminator::Deopt(pc)) = &block.term {
            let mut registers: Vec<(u16, NodeId)> = Vec::new();
            if let Some(live) = bytecode_live.get(pc) {
                let mut regs: Vec<u16> = live.iter().copied().collect();
                regs.sort_unstable();
                for r in regs {
                    if let Some(Some(v)) = env.get(r as usize) {
                        registers.push((r, *v));
                    }
                }
            }
            out.insert(
                b,
                DeoptPoint {
                    byte_pc: *pc,
                    registers,
                },
            );
        }
        exit_env[b as usize] = Some(env);
    }
    out
}

/// Per-guard SSA values a deopt frame restores, keyed by guard node. The
/// register allocator consumes this to keep each such value live to its guard so
/// the deopt exit can read it from a stable home (see [`super::liveness::analyze`]
/// and [`super::regalloc::allocate`]).
#[must_use]
pub fn deopt_value_uses(frames: &FxHashMap<NodeId, DeoptPoint>) -> FxHashMap<NodeId, Vec<NodeId>> {
    frames
        .iter()
        .map(|(&nid, point)| (nid, point.registers.iter().map(|&(_, v)| v).collect()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optimizing::build_graph;
    use otter_vm::{JitFunctionView, jit_feedback::ARITH_INT32};

    const STRIDE: u32 = 4;

    fn rel(from: usize, to: usize) -> i32 {
        (to as i32 - from as i32) * STRIDE as i32 - 1
    }

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

    #[test]
    fn straight_line_liveness() {
        // r2 = r0 + r1 ; return r2
        let v = view(
            2,
            3,
            &[
                (Op::Add, vec![r(2), r(0), r(1)], ARITH_INT32),
                (Op::ReturnValue, vec![r(2)], 0),
            ],
        );
        let live = bytecode_liveness(&v);
        // Before the Add, the operands r0 and r1 are live; r2 is not yet.
        assert_eq!(live[&0], [0u16, 1].into_iter().collect());
        // Before the return, only r2 is live.
        assert_eq!(live[&STRIDE], [2u16].into_iter().collect());
    }

    #[test]
    fn loop_keeps_counter_and_bound_live() {
        // i=0; acc=0; while (i < n) { acc += i; i += 1 } return acc
        let v = view(
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
        );
        let live = bytecode_liveness(&v);
        // At the header comparison (byte-pc 2*STRIDE): the counter i (r1), the
        // bound n (r0), and the accumulator acc (r2) are all live across the loop.
        let header = live[&(2 * STRIDE)].clone();
        assert!(header.contains(&0), "bound n live at header");
        assert!(header.contains(&1), "counter i live at header");
        assert!(header.contains(&2), "accumulator acc live at header");
        // After the loop, at the return (byte-pc 8*STRIDE), only acc (r2) is live.
        assert_eq!(live[&(8 * STRIDE)], [2u16].into_iter().collect());
    }

    #[test]
    fn frame_state_at_header_guard_restores_live_registers() {
        // Same counting loop. At the header comparison's guard the deopt frame
        // state must restore n (r0 → the parameter), i (r1 → the header phi) and
        // acc (r2 → the header phi).
        let g = build_graph(&view(
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
        let blive = bytecode_liveness(&view(
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
        ));
        let points = capture_frame_states(&g, &blive);

        // Find the header-comparison guard: a CheckInt32 at byte-pc 2*STRIDE.
        let header_guard = g
            .nodes
            .iter()
            .enumerate()
            .find(|(_, n)| matches!(n.kind, NodeKind::CheckInt32(_)) && n.byte_pc == 2 * STRIDE)
            .map(|(id, _)| id as NodeId)
            .expect("header comparison has an int32 guard");
        let point = &points[&header_guard];
        assert_eq!(point.byte_pc, 2 * STRIDE);

        let regs: std::collections::HashMap<u16, NodeId> =
            point.registers.iter().copied().collect();
        // r0 = n restores the parameter; r1 = i and r2 = acc restore header phis.
        assert!(
            matches!(g.node(regs[&0]).kind, NodeKind::Param(0)),
            "n is the param"
        );
        assert!(
            matches!(g.node(regs[&1]).kind, NodeKind::Phi(_)),
            "i is a header phi"
        );
        assert!(
            matches!(g.node(regs[&2]).kind, NodeKind::Phi(_)),
            "acc is a header phi"
        );
        // r3 (the comparison result) is not yet defined at the guard, so it is
        // not restored.
        assert!(
            !regs.contains_key(&3),
            "comparison result not live before itself"
        );
    }
}
