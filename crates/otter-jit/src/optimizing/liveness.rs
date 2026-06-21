//! SSA liveness analysis for the optimizing tier's backend.
//!
//! Computes per-block live-in / live-out sets of SSA values over the typed
//! [`Graph`], plus a reverse-postorder block ordering. Both the register
//! allocator (live-interval construction) and the deoptimizer (which values are
//! live at each guard / safepoint) consume this.
//!
//! # Algorithm
//! Standard SSA dataflow to a fixpoint — correct for arbitrary reducible *and*
//! irreducible control flow, loops included, without a separate loop-extension
//! pass:
//!
//! ```text
//! live_out[b] = ⋃_{s ∈ succ(b)} ( (live_in[s] \ defs_φ(s)) ∪ φ_inputs(s, b) )
//! live_in[b]  = uses(b) ∪ ( live_out[b] \ defs(b) )
//! ```
//!
//! where a φ's per-predecessor input is treated as a use on that predecessor
//! edge (it is live-out of the predecessor, not live-in of the φ's block) — the
//! defining property of SSA liveness. Iterated over blocks in reverse RPO until
//! the sets stop changing.
//!
//! # See also
//! - [`super::ir`] — the graph analyzed here.

use rustc_hash::{FxHashMap, FxHashSet};

use super::ir::{BlockId, Graph, NodeId, Terminator};

/// Liveness result: per-block live-in / live-out value sets and a
/// reverse-postorder block ordering.
#[derive(Debug, Clone)]
pub struct Liveness {
    /// Values live on entry to each block, indexed by [`BlockId`].
    pub live_in: Vec<FxHashSet<NodeId>>,
    /// Values live on exit from each block, indexed by [`BlockId`].
    pub live_out: Vec<FxHashSet<NodeId>>,
    /// Blocks in reverse postorder (entry first); a near-optimal visiting order
    /// for the allocator and a deterministic linearization for emission.
    pub rpo: Vec<BlockId>,
}

/// Successor blocks of `block`, derived from its terminator.
pub fn successors(graph: &Graph, block: BlockId) -> Vec<BlockId> {
    match graph.block(block).term {
        Some(Terminator::Jump(t)) => vec![t],
        Some(Terminator::Branch {
            on_true, on_false, ..
        }) => vec![on_true, on_false],
        Some(Terminator::Return(_)) | Some(Terminator::Deopt(_)) | None => Vec::new(),
    }
}

/// Reverse-postorder block ordering from the entry block (iterative DFS).
fn reverse_postorder(graph: &Graph) -> Vec<BlockId> {
    let n = graph.blocks.len();
    let mut visited = vec![false; n];
    let mut postorder = Vec::with_capacity(n);
    // (block, next successor index) DFS stack.
    let mut stack: Vec<(BlockId, usize)> = Vec::new();
    visited[graph.entry as usize] = true;
    stack.push((graph.entry, 0));
    while let Some(&(block, idx)) = stack.last() {
        let succs = successors(graph, block);
        if idx < succs.len() {
            stack.last_mut().unwrap().1 += 1;
            let s = succs[idx];
            if !visited[s as usize] {
                visited[s as usize] = true;
                stack.push((s, 0));
            }
        } else {
            postorder.push(block);
            stack.pop();
        }
    }
    postorder.reverse();
    postorder
}

/// Compute SSA liveness for `graph`.
///
/// `deopt_uses` maps each guard / overflowing-arithmetic node to the SSA values
/// its deopt frame state restores. These are *additional uses* at the guard:
/// a value read only to reconstruct the interpreter frame must stay live to the
/// guard so the register allocator keeps it in a home the deopt exit can read.
/// Omitting them would let the allocator reuse the register and corrupt the
/// restored frame.
pub fn analyze(
    graph: &Graph,
    deopt_uses: &FxHashMap<NodeId, Vec<NodeId>>,
    block_deopts: &FxHashMap<BlockId, super::deopt::DeoptPoint>,
) -> Liveness {
    let n = graph.blocks.len();
    let rpo = reverse_postorder(graph);

    // Precompute per-block defs and upward-exposed uses (non-φ).
    // defs(b)  = every value defined in b (φ heads + body nodes).
    // uses(b)  = body operands whose definition is not in b (φ operands are
    //            handled at predecessor edges, never here), plus deopt
    //            frame-state values a guard in b reads that are defined outside b.
    let mut defs: Vec<FxHashSet<NodeId>> = vec![FxHashSet::default(); n];
    let mut uses: Vec<FxHashSet<NodeId>> = vec![FxHashSet::default(); n];
    for (b, block) in graph.blocks.iter().enumerate() {
        let d = &mut defs[b];
        for &p in &block.phis {
            d.insert(p);
        }
        for &nid in &block.body {
            d.insert(nid);
        }
        let d = defs[b].clone();
        let u = &mut uses[b];
        for &nid in &block.body {
            for input in graph.node(nid).kind.inputs() {
                if !d.contains(&input) {
                    u.insert(input);
                }
            }
            if let Some(dvals) = deopt_uses.get(&nid) {
                for &v in dvals {
                    if !d.contains(&v) {
                        u.insert(v);
                    }
                }
            }
        }
        if let Some(dp) = block_deopts.get(&(b as BlockId)) {
            for &(_, v) in &dp.registers {
                if !d.contains(&v) {
                    u.insert(v);
                }
            }
        }
    }

    // Map each predecessor block to its operand index within a successor's φ
    // (preds order defines φ operand alignment).
    let pred_index = |succ: BlockId, pred: BlockId| -> Option<usize> {
        graph.block(succ).preds.iter().position(|&p| p == pred)
    };

    let mut live_in: Vec<FxHashSet<NodeId>> = vec![FxHashSet::default(); n];
    let mut live_out: Vec<FxHashSet<NodeId>> = vec![FxHashSet::default(); n];

    // Backward dataflow to a fixpoint. Iterate over RPO reversed (postorder),
    // which converges quickly for backward problems.
    let order: Vec<BlockId> = rpo.iter().rev().copied().collect();
    let mut changed = true;
    while changed {
        changed = false;
        for &b in &order {
            let bi = b as usize;
            let mut out: FxHashSet<NodeId> = FxHashSet::default();
            for s in successors(graph, b) {
                let si = s as usize;
                // live_in[s] minus s's own φ heads.
                for &v in &live_in[si] {
                    out.insert(v);
                }
                let sblock = graph.block(s);
                for &phi in &sblock.phis {
                    out.remove(&phi);
                }
                // Plus this edge's φ inputs.
                if let Some(idx) = pred_index(s, b) {
                    for &phi in &sblock.phis {
                        if let Some(&input) = graph.node(phi).kind.inputs().get(idx) {
                            out.insert(input);
                        }
                    }
                }
            }
            // live_in[b] = uses(b) ∪ (live_out[b] \ defs(b)).
            let mut ins = uses[bi].clone();
            for &v in &out {
                if !defs[bi].contains(&v) {
                    ins.insert(v);
                }
            }
            if out != live_out[bi] {
                live_out[bi] = out;
                changed = true;
            }
            if ins != live_in[bi] {
                live_in[bi] = ins;
                changed = true;
            }
        }
    }

    Liveness {
        live_in,
        live_out,
        rpo,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optimizing::build_graph;
    use otter_bytecode::{Op, Operand};
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
    fn loop_carried_value_is_live_across_back_edge() {
        // i=0; acc=0; while (i < n) { acc += i; i += 1 } return acc
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

        let live = analyze(&g, &Default::default());
        assert_eq!(live.rpo[0], g.entry, "entry first in RPO");
        assert_eq!(live.rpo.len(), g.blocks.len(), "every block ordered once");

        // The header's loop-carried phis (i and acc) must be live across the
        // back edge: live-out of the loop body block.
        let header = g
            .blocks
            .iter()
            .position(|b| b.start_pc == 2 * STRIDE)
            .unwrap() as BlockId;
        let body = g
            .blocks
            .iter()
            .position(|b| b.start_pc == 4 * STRIDE)
            .unwrap() as BlockId;
        // The back edge carries the next-iteration loop values (i+1, acc+i) plus
        // the loop-invariant n, all live OUT of the loop body. At least two
        // values cross the back edge.
        assert!(
            live.live_out[body as usize].len() >= 2,
            "loop body keeps loop-carried values live across the back edge"
        );
        // The header's phi inputs on the back edge are exactly values that are
        // live-out of the body (an SSA phi input is live-out of its predecessor,
        // never live-in of the phi's block). With trivial-phi elimination, the
        // invariant n flows directly (no phi), so the phi inputs are a subset of
        // the back-edge live set rather than equal to it.
        let header_phi_inputs: FxHashSet<NodeId> = g
            .block(header)
            .phis
            .iter()
            .flat_map(|&phi| g.node(phi).kind.inputs())
            .collect();
        assert!(
            !header_phi_inputs.is_empty(),
            "header has loop-carried phis"
        );
        for v in &header_phi_inputs {
            assert!(
                live.live_out[body as usize].contains(v)
                    || live.live_out[g.entry as usize].contains(v),
                "each header phi input is live-out of one of its predecessors"
            );
        }
    }

    #[test]
    fn straight_line_has_no_loop_liveness() {
        let g = build_graph(&view(
            1,
            4,
            &[
                (Op::LoadInt32, vec![r(1), imm(1)], 0),
                (Op::Add, vec![r(2), r(0), r(1)], ARITH_INT32),
                (Op::ReturnValue, vec![r(2)], 0),
            ],
        ))
        .expect("builds");
        let live = analyze(&g, &Default::default());
        assert_eq!(live.rpo.len(), 1);
        // Single block: nothing is live-in (params are defs in the entry block)
        // and nothing live-out (the return consumes the last value).
        assert!(live.live_out[0].is_empty());
    }
}
