//! Loop-invariant code motion: a node every iteration computes from the
//! same inputs over memory the loop never writes runs once, in a pre-header
//! every entry into the loop passes.
//!
//! # Contents
//! - [`hoist_invariants`] — per loop, innermost first: the pre-header and
//!   the nodes moved into it.
//!
//! # Invariants
//! - The loop's writes are the union of its nodes' [`effects::clobberize`]
//!   writes. A loop that writes the world (a call, or a slow path that may
//!   run JavaScript) moves nothing. A node moves only when it writes
//!   nothing, reads no heap the loop writes, cannot call, throw or collect,
//!   and every input is defined before the loop, moved before it, or a header
//!   phi the loop never changes, which it reads as the value entering the
//!   loop. A raw element base is read past nothing that collects
//!   ([`effects::AbstractHeap::Movement`]); the back edge's poll is a
//!   no-allocation leaf (`STUB_JIT_BACKEDGE_POLL`).
//! - Reads of the activation record (`this`, window registers, actual
//!   arguments) stay where the builder put them: an entry that resumes an
//!   existing record, the OSR entry among them, reaches the pre-header before
//!   the record holds what the loop reads.
//! - A node that dereferences an input on the strength of an earlier proof
//!   moves only when no guard of that input stays in the loop ahead of it,
//!   and either its block runs on every iteration and before every exit
//!   from the loop, or each input is proved by a guard moved ahead of it or
//!   was itself moved: in the pre-header the same proofs precede it.
//! - The pre-header merges every entering edge, the OSR entry included,
//!   with a phi wherever the edges bring different values; the header's
//!   back edges stay its last predecessors. Nothing moves out of the blocks
//!   of a loop that an entry, such as the OSR entry into an inner loop,
//!   reaches without passing its header.
//! - A moved check leaves to the loop header with the values entering the
//!   loop, so the interpreter runs the iteration the check guarded. A
//!   header that already left optimized code for a reason a check raises
//!   keeps that check in the loop: its next compile checks where the value
//!   is used.
//!
//! # See also
//! - [`super::effects`] — the abstract heaps of reads and writes.
//! - [`super::dominators`] — which loop blocks run every iteration.
//! - [`super::phi_repr`] — checks entry values of untagged phis on their
//!   entering edges.
//! - [`super::builder::LoopHeader`] — the loop facts this pass reads.

use rustc_hash::FxHashSet;
use smallvec::SmallVec;

use super::builder::LoopHeader;
use super::dominators::{self, Dominators};
use super::effects::{self, AbstractHeap, HeapSet};
use super::ir::{BlockId, FrameState, FrameStateId, Graph, Kind, NodeId, Repr};

/// Move the invariant nodes of every loop in `loop_headers` into a
/// pre-header placed before the loop in `layout`.
pub(crate) fn hoist_invariants(
    graph: &mut Graph,
    layout: &mut Vec<BlockId>,
    loop_headers: &[LoopHeader],
) {
    let mut loops: Vec<(&LoopHeader, usize)> = loop_headers
        .iter()
        .filter(|header| header.hoist)
        .map(|header| (header, loop_body(graph, header.block).len()))
        .collect();
    // Inner loops first: what leaves an inner loop, its pre-header
    // included, may leave its outer one too.
    loops.sort_by_key(|&(_, size)| size);
    for (header, _) in loops {
        let body = loop_body(graph, header.block);
        hoist_loop(graph, layout, header, &body);
    }
}

/// The blocks of the loop headed by `header`: the header and every block
/// that reaches one of its back edges without passing the header.
pub(crate) fn loop_body(graph: &Graph, header: BlockId) -> FxHashSet<BlockId> {
    let mut body = FxHashSet::default();
    body.insert(header);
    let mut work: Vec<BlockId> = graph
        .block(header)
        .predecessors
        .iter()
        .copied()
        .filter(|&predecessor| {
            graph.block(predecessor).control.is_some_and(|control| {
                matches!(graph.node(control).kind, Kind::JumpLoop(target) if target == header)
            })
        })
        .collect();
    while let Some(block) = work.pop() {
        if body.insert(block) {
            work.extend(graph.block(block).predecessors.iter().copied());
        }
    }
    body
}

/// The blocks of `body` an entry reaches without passing `header`: every
/// block forward of a body block with no predecessor, such as the OSR entry.
fn bypassed_blocks(
    graph: &Graph,
    header: BlockId,
    body: &FxHashSet<BlockId>,
) -> FxHashSet<BlockId> {
    let mut reached = FxHashSet::default();
    let mut work: Vec<BlockId> = body
        .iter()
        .copied()
        .filter(|&block| block != header && graph.block(block).predecessors.is_empty())
        .collect();
    while let Some(block) = work.pop() {
        if block == header || !body.contains(&block) || !reached.insert(block) {
            continue;
        }
        let Some(control) = graph.block(block).control else {
            continue;
        };
        match graph.node(control).kind {
            Kind::Jump(target) | Kind::JumpLoop(target) => work.push(target),
            Kind::Branch {
                if_true, if_false, ..
            } => work.extend([if_true, if_false]),
            _ => {}
        }
    }
    reached
}

/// Whether a check moved to the pre-header may leave at the loop header: no
/// earlier optimized code left there for a reason this check raises.
fn may_leave_at_header(kind: &Kind, header: &LoopHeader) -> bool {
    use otter_vm::native_abi::ExitReason as R;
    let reasons: &[R] = match kind {
        Kind::CheckShapes { .. }
        | Kind::CheckElements { .. }
        | Kind::LoadArrayLength { .. }
        | Kind::LoadGuardedMethod { .. }
        | Kind::LoadNamedProperty { .. }
        | Kind::LoadArrayExpando(_)
        | Kind::LoadFunctionBag => return header.hoist,
        Kind::CheckedTaggedToInt32
        | Kind::CheckedTaggedToFloat64
        | Kind::CheckedFloat64ToInt32
        | Kind::CheckNumber => return header.speculate,
        Kind::CheckedTaggedToIndex | Kind::CheckedFloat64ToIndex => {
            if !header.speculate {
                return false;
            }
            &[R::InvalidElementIndex]
        }
        Kind::CheckBounds => &[R::BoundsGuard],
        Kind::StringCharCodeAt => &[R::TypeMismatch, R::BoundsGuard],
        Kind::Int32Add | Kind::Int32Sub | Kind::Int32Negate => &[R::Int32Overflow, R::NegativeZero],
        Kind::Int32Mul
        | Kind::Int32MulIdentifyZeros
        | Kind::Int32Div
        | Kind::Int32Mod
        | Kind::Int32ShiftRightLogical => &[R::Int32Overflow, R::NegativeZero, R::TypeMismatch],
        Kind::CheckFunction { .. }
        | Kind::CheckCellValue(_)
        | Kind::CheckNative(_)
        | Kind::CheckFunctionPrototypeCall(_)
        | Kind::CheckClosureContext(_) => &[R::IdentityGuard],
        Kind::CheckFieldTypes(_)
        | Kind::CheckContextSlot { .. }
        | Kind::CheckStableShapes(_)
        | Kind::CheckDependencies => &[R::DependencyChanged],
        // A check whose reasons are not listed moves only out of a header
        // that never left.
        _ => return header.exit_reasons.is_empty(),
    };
    !reasons
        .iter()
        .any(|reason| header.exit_reasons.contains(reason))
}

/// Every heap a node of the loop body may write.
fn loop_writes(graph: &Graph, body: &FxHashSet<BlockId>) -> HeapSet {
    let mut writes = HeapSet::default();
    for &block in body {
        for &node in &graph.block(block).body {
            for heap in effects::clobberize(&graph.node(node).kind).writes {
                writes.add(heap);
            }
        }
    }
    writes
}

/// Whether `block` runs on every iteration that reaches a back edge and on
/// no path that leaves the loop before it: it dominates every back edge, and
/// no edge out of the body is reachable from the header around it.
fn control_equivalent(
    graph: &Graph,
    dominators: &Dominators,
    header: BlockId,
    body: &FxHashSet<BlockId>,
    latches: &[BlockId],
    block: BlockId,
) -> bool {
    if !latches
        .iter()
        .all(|&latch| dominators.dominates(block, latch))
    {
        return false;
    }
    if block == header {
        return true;
    }
    let mut seen: FxHashSet<BlockId> = FxHashSet::default();
    let mut work = vec![header];
    while let Some(current) = work.pop() {
        if current == block || !seen.insert(current) {
            continue;
        }
        let next = dominators::successors(graph, current);
        let leaves = graph.block(current).control.is_some_and(|control| {
            matches!(graph.node(control).kind, Kind::Return | Kind::Deopt(_))
        });
        if leaves || next.iter().any(|successor| !body.contains(successor)) {
            return false;
        }
        work.extend(next.into_iter().filter(|&successor| successor != header));
    }
    true
}

fn hoist_loop(
    graph: &mut Graph,
    layout: &mut Vec<BlockId>,
    header: &LoopHeader,
    body: &FxHashSet<BlockId>,
) {
    // An entry into the body that bypasses the header, the OSR entry into an
    // inner loop among them, reaches the blocks after it without the
    // pre-header: nothing there moves.
    let bypassed = bypassed_blocks(graph, header.block, body);
    let writes = loop_writes(graph, body);
    if writes.is_world() {
        return;
    }
    let predecessors = graph.block(header.block).predecessors.clone();
    let entering: SmallVec<[usize; 2]> = (0..predecessors.len())
        .filter(|&index| !body.contains(&predecessors[index]))
        .collect();
    if entering.is_empty() {
        return;
    }
    let latches: SmallVec<[BlockId; 2]> = predecessors
        .iter()
        .copied()
        .filter(|block| body.contains(block))
        .collect();
    let dominators = Dominators::compute(graph, layout);
    // A header phi the loop never changes holds its entering value.
    let unchanged_phi = |graph: &Graph, value: NodeId| -> bool {
        let data = graph.node(value);
        data.kind == Kind::Phi
            && data.block == Some(header.block)
            && data
                .inputs
                .iter()
                .enumerate()
                .all(|(index, &input)| entering.contains(&index) || input == value)
    };
    let ordered: Vec<BlockId> = layout
        .iter()
        .copied()
        .filter(|block| body.contains(block))
        .collect();
    let mut moved: Vec<NodeId> = Vec::new();
    let mut moved_set: FxHashSet<NodeId> = FxHashSet::default();
    let mut checks: Vec<(Kind, SmallVec<[NodeId; 3]>)> = Vec::new();
    // Values a guard that stays in the loop proves: what reads through them
    // stays behind that guard.
    let mut guarded_in_loop: FxHashSet<NodeId> = FxHashSet::default();
    // Values a guard moved to the pre-header proves there.
    let mut proved: FxHashSet<NodeId> = FxHashSet::default();
    for &block in &ordered {
        if bypassed.contains(&block) {
            continue;
        }
        let equivalent =
            control_equivalent(graph, &dominators, header.block, body, &latches, block);
        let nodes = graph.block(block).body.clone();
        let mut kept = Vec::with_capacity(nodes.len());
        for node in nodes {
            let data = graph.node(node);
            let effects = effects::clobberize(&data.kind);
            let properties = data.kind.properties();
            // A raw base input names movable storage only while nothing
            // collects; the move keeps tagged inputs alone.
            let raw_base = matches!(data.kind, Kind::LoadTaggedField(_))
                && data
                    .inputs
                    .first()
                    .is_some_and(|&input| graph.node(input).repr != Repr::Tagged);
            let movable = effects.writes.is_empty()
                && !properties.call
                && !properties.lazy_deopt
                && !properties.can_throw
                && !properties.may_collect
                && !raw_base
                && !matches!(data.kind, Kind::Generic { .. })
                && !effects.reads_overlap(&writes)
                && !effects.reads.contains(&AbstractHeap::Frame)
                && (!properties.eager_deopt || may_leave_at_header(&data.kind, header))
                && (!effects.derefs
                    || (data
                        .inputs
                        .iter()
                        .all(|input| !guarded_in_loop.contains(input))
                        && (equivalent
                            || data.inputs.iter().all(|input| {
                                proved.contains(input)
                                    || moved_set.contains(input)
                                    || graph.node(*input).block.is_none()
                            }))));
            let invariant = movable
                && data.inputs.iter().all(|&input| {
                    let input_data = graph.node(input);
                    input_data.block.is_none_or(|block| !body.contains(&block))
                        || moved_set.contains(&input)
                        || unchanged_phi(graph, input)
                });
            if !invariant {
                if effects.guard {
                    guarded_in_loop.extend(data.inputs.iter().copied());
                }
                kept.push(node);
                continue;
            }
            if data.repr == Repr::None {
                let key = (data.kind.clone(), data.inputs.clone());
                if checks.contains(&key) {
                    continue;
                }
                checks.push(key);
            }
            if effects.guard {
                proved.extend(data.inputs.iter().copied());
            }
            moved_set.insert(node);
            moved.push(node);
        }
        graph.block_mut(block).body = kept;
    }
    if moved.is_empty() {
        return;
    }
    let preheader = insert_preheader(graph, layout, header, &entering);
    let state = entry_state(graph, header);
    for node in moved {
        // A header phi the loop never changes is, here, the value the
        // pre-header brings it: its first input.
        let inputs = graph.node(node).inputs.clone();
        let rewired: SmallVec<[NodeId; 3]> = inputs
            .iter()
            .map(|&input| {
                let data = graph.node(input);
                let unchanged = data.kind == Kind::Phi
                    && data.block == Some(header.block)
                    && data.inputs[1..].iter().all(|&back| back == input);
                if unchanged { data.inputs[0] } else { input }
            })
            .collect();
        let data = graph.node_mut(node);
        data.inputs = rewired;
        data.block = Some(preheader);
        if data.eager.is_some() {
            data.eager = Some(state);
        }
        graph.block_mut(preheader).body.push(node);
    }
}

/// A block every entering edge of the loop goes through, placed just
/// before the header: the header's first predecessor, ahead of its back
/// edges, with a phi for each header phi whose entering values differ.
fn insert_preheader(
    graph: &mut Graph,
    layout: &mut Vec<BlockId>,
    header: &LoopHeader,
    entering: &[usize],
) -> BlockId {
    let preheader = graph.new_block();
    let predecessors = graph.block(header.block).predecessors.clone();
    let back_edges: SmallVec<[BlockId; 2]> = (0..predecessors.len())
        .filter(|index| !entering.contains(index))
        .map(|index| predecessors[index])
        .collect();
    graph.position = graph.frame_state(header.state).pc;
    graph.origin = 0;
    for phi in graph.block(header.block).phis.clone() {
        let inputs = graph.node(phi).inputs.clone();
        let entry: SmallVec<[NodeId; 2]> = entering.iter().map(|&index| inputs[index]).collect();
        let from_preheader = if entry.iter().all(|&value| value == entry[0]) {
            entry[0]
        } else {
            let repr = graph.node(phi).repr;
            let merged = graph.add_node(Kind::Phi, &entry, repr);
            graph.node_mut(merged).block = Some(preheader);
            graph.block_mut(preheader).phis.push(merged);
            merged
        };
        let mut rewired: SmallVec<[NodeId; 3]> = SmallVec::new();
        rewired.push(from_preheader);
        rewired.extend(
            (0..inputs.len())
                .filter(|index| !entering.contains(index))
                .map(|index| inputs[index]),
        );
        graph.node_mut(phi).inputs = rewired;
    }
    graph.block_mut(preheader).predecessors =
        entering.iter().map(|&index| predecessors[index]).collect();
    let mut header_predecessors = vec![preheader];
    header_predecessors.extend(back_edges);
    graph.block_mut(header.block).predecessors = header_predecessors;
    for &index in entering {
        let block = predecessors[index];
        let control = graph
            .block(block)
            .control
            .expect("an entering edge ends its block");
        match &mut graph.node_mut(control).kind {
            Kind::Jump(target) if *target == header.block => *target = preheader,
            Kind::Branch {
                if_true, if_false, ..
            } => {
                if *if_true == header.block {
                    *if_true = preheader;
                }
                if *if_false == header.block {
                    *if_false = preheader;
                }
            }
            other => unreachable!("an entering edge ends in {other:?}"),
        }
    }
    let jump = graph.add_node(Kind::Jump(header.block), &[], Repr::None);
    graph.node_mut(jump).block = Some(preheader);
    graph.block_mut(preheader).control = Some(jump);
    let position = layout
        .iter()
        .position(|&block| block == header.block)
        .expect("the header is laid out");
    layout.insert(position, preheader);
    preheader
}

/// The interpreter state at the loop header as the pre-header enters it:
/// the back edge's state with each phi bound to the pre-header's input.
fn entry_state(graph: &mut Graph, header: &LoopHeader) -> FrameStateId {
    let back_edge = graph.frame_state(header.state).clone();
    let registers = back_edge
        .registers
        .iter()
        .map(|&(register, value)| {
            let entry_value = header
                .phis
                .iter()
                .find(|&&(phi_register, _)| phi_register == register)
                .map_or(value, |&(_, phi)| graph.node(phi).inputs[0]);
            (register, entry_value)
        })
        .collect();
    graph.add_frame_state(FrameState {
        registers,
        ..back_edge
    })
}
