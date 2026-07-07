//! Linear-scan register allocation on SSA form for the optimizing tier.
//!
//! Assigns each SSA value of a typed [`Graph`] a machine home — an abstract GP
//! register index or a spill slot — using the Wimmer / Mössenböck algorithm
//! ("Linear Scan Register Allocation on SSA Form", CGO 2010). Operating directly
//! on SSA means there is no separate live-range coalescing pass: a value's live
//! range is exactly its def-to-last-use span, and control-flow merges are
//! resolved by inserting moves on the edges (phi lowering happens here too). The
//! emitter consumes [`Allocation::location`] to place values and
//! [`Allocation::edge_moves`] to wire up control-flow edges; the deoptimizer
//! reads the same locations to reconstruct the interpreter frame at a guard.
//!
//! # Algorithm
//! 1. **Position numbering.** Linearize blocks in liveness reverse-postorder and,
//!    within a block, phis then body. Each operation gets a strictly increasing
//!    *even* position. We follow Wimmer's convention that a use reads at the
//!    operation's (even) position and a def becomes available at position `+1`
//!    (the odd "def slot"). A value used and defined at the same instruction
//!    therefore does not falsely interfere: the old value's range ends at the
//!    even use position, the new value's range starts at the odd def position.
//!    Block boundaries are recorded as `[first_use_pos, last_pos+2)` so a value
//!    live across the whole block spans every position in it.
//! 2. **Loop detection.** A DFS over the CFG marks back-edges `b → h` (`h`'s rpo
//!    index `≤` `b`'s and `h` is an ancestor on the DFS stack). The natural loop
//!    body is the set of blocks that can reach the back-edge source without
//!    passing through the header (reverse walk over `Block.preds`, stopping at the
//!    header). Each loop records its maximum end position; reducible control flow
//!    is assumed (bytecode from structured source), so every back-edge has a
//!    single header that dominates its body.
//! 3. **Live intervals with holes.** Built in a single reverse pass over the
//!    linearized blocks (Wimmer `buildIntervals`): seed each value live-out of a
//!    block with a range covering the whole block, then walk the block's
//!    operations in reverse — `set_from` at a def shortens the leading range to
//!    the def position, and each input use `add_range`s `[block_start, use_pos]`.
//!    Phi heads define at the block's start position; phi *inputs* are uses on the
//!    predecessor edge (handled during resolution), never block-local uses here.
//!    Ranges coalesce so an interval is a sorted, disjoint list of `[from, to)`
//!    half-open ranges — the holes are the gaps between them. **Loop extension:**
//!    a value live at a loop header and live through the loop has its interval
//!    extended to the loop's end position so its register is not handed to another
//!    value mid-loop.
//! 4. **Linear scan.** Intervals are processed in increasing start order against
//!    `active` (covering the current position) and `inactive` (allocated but
//!    currently in a hole) lists. Because the current allocator assigns one home
//!    per value and does not insert reloads at hole exits, inactive intervals
//!    still reserve their registers; the scan only reuses a register when no
//!    live interval owns it. If none covers the whole interval we **spill**:
//!    whichever of the current interval or a blocking active/inactive interval
//!    has the furthest next-use is sent to a fresh spill slot.
//!    Registers are partitioned into classes by [`Repr`] via [`reg_class_of`]:
//!    `Int32` / `Bool` / `Tagged` allocate from the GP pool and `Float64` from a
//!    disjoint FP pool, each sized independently. The scan logic is class-generic
//!    (free-until / next-use are computed per class), so the two pools never
//!    alias.
//! 5. **Resolution.** For every control-flow edge `pred → succ`, values live
//!    across the edge whose home differs between the two ends, plus each `succ`
//!    phi mapping its `pred`-edge input to the phi's home, become a *parallel*
//!    move set. Moves are ordered so no move clobbers a source still needed
//!    (topological by free destinations); cycles are broken with a scratch
//!    location.
//!
//! # Invariants
//! - **Interference freedom.** Two values whose lifetime spans overlap never
//!   receive the same `Reg`. Lifetime holes are tracked for future splitting, but
//!   they are not used for register reuse until reload insertion exists.
//! - **One home per value (no splitting yet).** A value is spilled *whole* — it
//!   keeps a single [`Location`] for its entire life. The `active` / `inactive`
//!   machinery and the per-position "free until" computation are nonetheless
//!   structured exactly as splitting requires, so interval splitting (spill only
//!   the cold sub-range, keep the hot sub-range in a register) can be added later
//!   by splitting an interval at a position and re-queueing the tail, without
//!   reworking the scan. That is the one deliberately deferred production feature.
//! - **SSA phi semantics.** A phi input is live-out of its predecessor, not
//!   live-in of the phi's block; phi homes are reconciled with their inputs only
//!   by edge moves, never by forcing the input and the phi to share a register.
//! - **Reducible CFG.** Loop detection assumes a single header per back-edge.
//!
//! # See also
//! - [`super::ir`] — the typed SSA graph allocated over.
//! - [`super::liveness`] — live-in / live-out sets and the RPO this consumes.
//! - [`crate::baseline`] — the deopt target the saved locations restore into.

use rustc_hash::{FxHashMap, FxHashSet};

use super::ir::{BlockId, Graph, NodeId, Repr};
use super::liveness::{Liveness, successors};

/// A value's final machine home.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Location {
    /// Abstract GP register index (`0..gp_regs`); the emitter maps it to a
    /// physical register.
    Reg(u32),
    /// Spill-slot index (`0..spill_slots`); the emitter maps it to a frame offset.
    Spill(u32),
}

/// Register class an interval allocates from. Distinct classes draw from
/// disjoint physical register pools and never alias. A [`Location::Reg`] index
/// is interpreted *within* its value's class (the emitter recovers the class
/// from the value's [`Repr`]), so `Reg(3)` of a `Gp` value and `Reg(3)` of an
/// `Fp` value are different physical registers and never interfere.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum RegClass {
    /// General-purpose integer / pointer registers (`Int32`, `Bool`, `Tagged`).
    Gp,
    /// Floating-point registers (`Float64`).
    Fp,
}

/// Pick the register class a value of representation `repr` must live in. `Gp`
/// and `Fp` draw from separate physical register pools sized independently by
/// the scan, so adding the float subset needed only this arm plus a per-class
/// register count — the rest of the allocator is class-generic.
#[must_use]
fn reg_class_of(repr: Repr) -> RegClass {
    match repr {
        Repr::Int32 | Repr::Bool | Repr::Tagged => RegClass::Gp,
        Repr::Float64 => RegClass::Fp,
    }
}

/// One control-flow edge's reconciliation: the parallel move set that makes the
/// successor's expected locations match what the predecessor produced.
#[derive(Clone, Debug)]
pub struct EdgeMoves {
    /// Predecessor block of the edge.
    pub pred: BlockId,
    /// Successor block of the edge.
    pub succ: BlockId,
    /// Ordered `(from, to)` moves to execute on this edge. Ordering already
    /// breaks any parallel-move cycle with a scratch slot, so the emitter runs
    /// them sequentially.
    pub moves: Vec<(Location, Location)>,
}

/// A half-open `[from, to)` live range. `to` is exclusive: the value is *not*
/// live at position `to`.
type Range = (u32, u32);

/// One value's live interval: a sorted, disjoint list of ranges (the gaps
/// between them are its lifetime holes), its use positions, and — after the scan
/// — its assigned home.
#[derive(Clone, Debug)]
struct Interval {
    /// The SSA value this interval describes.
    value: NodeId,
    /// Register class drawn from (selected from the value's repr).
    class: RegClass,
    /// Sorted, disjoint, coalesced `[from, to)` ranges. The leading range's
    /// `from` is the interval's start (its definition); the final range's `to` is
    /// the interval's end (one past its last use).
    ranges: Vec<Range>,
    /// Positions at which this value is read, ascending. Drives spill heuristics
    /// (furthest next-use) and, later, optimal split points.
    use_positions: Vec<u32>,
    /// `true` when this value is live across a call position: it must not be
    /// assigned a caller-saved register, since the call clobbers those. Set by
    /// [`build_intervals`] from the call-site positions.
    crosses_call: bool,
    /// Home assigned by the scan (`None` until allocated).
    location: Option<Location>,
}

impl Interval {
    fn new(value: NodeId, class: RegClass) -> Self {
        Self {
            value,
            class,
            ranges: Vec::new(),
            use_positions: Vec::new(),
            crosses_call: false,
            location: None,
        }
    }

    /// Start position (definition) of the interval. Only valid once non-empty.
    fn start(&self) -> u32 {
        self.ranges[0].0
    }

    /// End position (one past last use) of the interval.
    fn end(&self) -> u32 {
        self.ranges.last().unwrap().1
    }

    /// Add `[from, to)` to the interval, coalescing with overlapping or adjacent
    /// ranges so the list stays sorted and disjoint. A no-op for an empty range.
    fn add_range(&mut self, from: u32, to: u32) {
        if from >= to {
            return;
        }
        // Insert then merge: find the first range that ends at or after `from`.
        let mut lo = from;
        let mut hi = to;
        let mut merged: Vec<Range> = Vec::with_capacity(self.ranges.len() + 1);
        let mut inserted = false;
        for &(rf, rt) in &self.ranges {
            if rt < lo {
                // Entirely before the new range.
                merged.push((rf, rt));
            } else if rf > hi {
                // Entirely after: flush the (possibly merged) new range first.
                if !inserted {
                    merged.push((lo, hi));
                    inserted = true;
                }
                merged.push((rf, rt));
            } else {
                // Overlapping or adjacent: absorb into the new range.
                lo = lo.min(rf);
                hi = hi.max(rt);
            }
        }
        if !inserted {
            merged.push((lo, hi));
        }
        self.ranges = merged;
    }

    /// Shorten the leading range's `from` to `pos` (Wimmer `setFrom`): a
    /// definition replaces the "live for the whole block" seed with "live from
    /// the def". If there is no range yet (a value defined but never used), seed a
    /// unit range so the def still occupies a position.
    fn set_from(&mut self, pos: u32) {
        if let Some(first) = self.ranges.first_mut() {
            first.0 = pos;
        } else {
            self.ranges.push((pos, pos + 1));
        }
    }

    /// Record a use at `pos` (kept sorted, de-duplicated).
    fn add_use(&mut self, pos: u32) {
        match self.use_positions.binary_search(&pos) {
            Ok(_) => {}
            Err(i) => self.use_positions.insert(i, pos),
        }
    }

    /// Whether this interval is live at `pos` (inside one of its ranges).
    fn covers(&self, pos: u32) -> bool {
        self.ranges.iter().any(|&(f, t)| f <= pos && pos < t)
    }

    /// First explicit use at or after `pos`. A value live past `pos` with no
    /// explicit use ahead (e.g. it is only consumed by a successor phi on an
    /// out-edge, which is an edge use rather than a recorded read) still must not
    /// look "free": fall back to the interval end so it remains a live blocker,
    /// only weaker than an interval with a real upcoming read. Returns `u32::MAX`
    /// only when the interval is genuinely dead from `pos` on.
    fn next_use(&self, pos: u32) -> u32 {
        if let Some(u) = self.use_positions.iter().copied().find(|&u| u >= pos) {
            return u;
        }
        if self.end() > pos {
            self.end()
        } else {
            u32::MAX
        }
    }
}

/// Register-allocation result for one function. `location` is the final home of
/// every value; `edge_moves` carries the per-edge parallel-move sets (phi
/// lowering + cross-edge reconciliation). The emitter and deoptimizer consume
/// both.
#[derive(Clone, Debug)]
pub struct Allocation {
    /// Final home of each SSA value (exactly one entry per value: no splitting).
    pub location: FxHashMap<NodeId, Location>,
    /// Number of distinct value spill slots used (`Spill(0..spill_slots)`). The
    /// emitter must additionally reserve **one more** frame slot,
    /// `Spill(spill_slots)`, as the parallel-move cycle-break scratch referenced
    /// by [`EdgeMoves::moves`]; it is never a live value's home.
    pub spill_slots: u32,
    /// Per control-flow edge reconciliation moves, in emission order.
    pub edge_moves: Vec<EdgeMoves>,
}

/// Allocate registers for `graph` given its `liveness` and the per-class
/// register counts `gp_regs` / `fp_regs` (`Reg(0..count)` within each class).
/// The result is interference-free: no two values live at the same position
/// share a `Reg` *of the same class*.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn allocate(
    graph: &Graph,
    liveness: &Liveness,
    gp_regs: u32,
    fp_regs: u32,
    caller_saved_gp: u32,
    caller_saved_fp: u32,
    deopt_uses: &FxHashMap<NodeId, Vec<NodeId>>,
) -> Allocation {
    let numbering = Numbering::build(graph, liveness);
    let loops = detect_loops(graph, liveness, &numbering);
    // Positions of call sites: a value live across one cannot stay in a
    // caller-saved register (the call clobbers those), so its interval is
    // pinned to the callee-saved sub-pool (or spilled).
    let mut call_positions: Vec<u32> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| {
            matches!(
                n.kind,
                super::ir::NodeKind::Call { .. }
                    | super::ir::NodeKind::CallMethod { .. }
                    | super::ir::NodeKind::StringConcat { .. }
                    | super::ir::NodeKind::ArrayPush { .. }
                    | super::ir::NodeKind::StorePropertyGeneric
                    // `Float64Rem` lowers to a leaf `fmod` libcall that clobbers
                    // the caller-saved register pool, so values live across it must
                    // hold callee-saved homes just like a real call.
                    | super::ir::NodeKind::Float64Rem(_, _)
                    // `Float64UnaryCall` (`Math.sin` / `Math.log` / …) is a leaf
                    // libm call with the same caller-saved clobber contract.
                    | super::ir::NodeKind::Float64UnaryCall(_, _)
            )
        })
        .filter_map(|(id, _)| numbering.pos_of.get(&(id as NodeId)).copied())
        .collect();
    call_positions.sort_unstable();
    let intervals = build_intervals(
        graph,
        liveness,
        &numbering,
        &loops,
        deopt_uses,
        &call_positions,
    );
    let scan = LinearScan::run(
        intervals,
        gp_regs,
        fp_regs,
        caller_saved_gp,
        caller_saved_fp,
    );
    let location = scan.locations();
    let edge_moves = resolve(graph, liveness, &numbering, &scan, &location);
    Allocation {
        location,
        spill_slots: scan.spill_slots,
        edge_moves,
    }
}

// --- Position numbering -----------------------------------------------------

/// Linear-position assignment over the SSA graph. Maps each value to its even
/// def/use position and records each block's `[first, last]` position span.
struct Numbering {
    /// Even position of the *operation* that defines each value (use position).
    /// The value is available from `pos + 1` (the odd def slot).
    pos_of: FxHashMap<NodeId, u32>,
    /// `[first_pos, last_pos]` (inclusive even positions) of each block, indexed
    /// by `BlockId`. A block with no operations still reserves one position.
    block_span: Vec<(u32, u32)>,
    /// Linearized block order (== liveness rpo), kept for the reverse interval
    /// build and edge enumeration.
    order: Vec<BlockId>,
}

impl Numbering {
    fn build(graph: &Graph, liveness: &Liveness) -> Self {
        let mut pos_of = FxHashMap::default();
        let mut block_span = vec![(0u32, 0u32); graph.blocks.len()];
        let mut pos: u32 = 0;
        for &b in &liveness.rpo {
            let block = graph.block(b);
            let first = pos;
            // Phis define at block entry, then body nodes in order.
            for &phi in &block.phis {
                pos_of.insert(phi, pos);
                pos += 2;
            }
            for &nid in &block.body {
                pos_of.insert(nid, pos);
                pos += 2;
            }
            // Reserve a trailing slot for the terminator's use(s) and to give an
            // empty block a non-degenerate span.
            let last = pos;
            pos += 2;
            block_span[b as usize] = (first, last);
        }
        Self {
            pos_of,
            block_span,
            order: liveness.rpo.clone(),
        }
    }

    /// Even use position of a value's defining operation.
    fn pos(&self, value: NodeId) -> u32 {
        self.pos_of[&value]
    }

    /// Odd position at which a value becomes available (its def slot).
    fn def_pos(&self, value: NodeId) -> u32 {
        self.pos_of[&value] + 1
    }

    /// `[first, last]` inclusive position span of a block.
    fn span(&self, block: BlockId) -> (u32, u32) {
        self.block_span[block as usize]
    }

    /// The position one past a block's last operation — the exclusive end used to
    /// make a whole-block range cover the terminator's uses.
    fn block_end(&self, block: BlockId) -> u32 {
        self.block_span[block as usize].1 + 2
    }
}

// --- Loop detection ---------------------------------------------------------

/// A natural loop: its header block, member blocks, and maximum end position.
struct NaturalLoop {
    header: BlockId,
    blocks: FxHashSet<BlockId>,
    /// Exclusive end position of the loop (max `block_end` over its members),
    /// filled by the interval builder once numbering is known.
    end_pos: u32,
}

/// Find natural loops via back-edge detection over the RPO. A back-edge is
/// `b → h` where `h` has an rpo index `≤` `b`'s and `h` is an ancestor of `b` on
/// the DFS tree (reducible CFG: that is exactly an edge to a dominator-header).
/// The loop body is the reverse-reachable set from `b` to `h` over `preds`.
fn detect_loops(graph: &Graph, liveness: &Liveness, numbering: &Numbering) -> Vec<NaturalLoop> {
    let mut rpo_index = vec![u32::MAX; graph.blocks.len()];
    for (i, &b) in liveness.rpo.iter().enumerate() {
        rpo_index[b as usize] = i as u32;
    }

    let mut loops: Vec<NaturalLoop> = Vec::new();
    // For each edge b → s, an s with rpo_index <= b's is a loop header reached by
    // a back-edge. (Under a reducible CFG this coincides with `s` dominating `b`.)
    for &b in &liveness.rpo {
        for s in successors(graph, b) {
            if rpo_index[s as usize] != u32::MAX && rpo_index[s as usize] <= rpo_index[b as usize] {
                let body = natural_loop_body(graph, s, b);
                if let Some(existing) = loops.iter_mut().find(|l| l.header == s) {
                    existing.blocks.extend(body);
                } else {
                    loops.push(NaturalLoop {
                        header: s,
                        blocks: body,
                        end_pos: 0,
                    });
                }
            }
        }
    }
    // The loop's exclusive end position is the max block-end over its members;
    // interval extension stretches loop-through values to this position.
    for lp in &mut loops {
        lp.end_pos = lp
            .blocks
            .iter()
            .map(|&b| numbering.block_end(b))
            .max()
            .unwrap_or(0);
    }
    loops
}

/// Member blocks of the natural loop with the given `header` whose back-edge
/// source is `tail`: every block that reaches `tail` without passing through the
/// header, plus the header itself. Reverse walk over `Block.preds`.
fn natural_loop_body(graph: &Graph, header: BlockId, tail: BlockId) -> FxHashSet<BlockId> {
    let mut body = FxHashSet::default();
    body.insert(header);
    if tail == header {
        // Self-loop: the header is the entire body.
        return body;
    }
    let mut stack = vec![tail];
    body.insert(tail);
    while let Some(b) = stack.pop() {
        for &p in &graph.block(b).preds {
            if body.insert(p) {
                stack.push(p);
            }
        }
    }
    body
}

// --- Interval construction --------------------------------------------------

/// Borrow (creating on first touch) the interval for `value`, with its register
/// class derived from the value's representation.
fn interval_for<'m>(
    intervals: &'m mut FxHashMap<NodeId, Interval>,
    graph: &Graph,
    value: NodeId,
) -> &'m mut Interval {
    intervals
        .entry(value)
        .or_insert_with(|| Interval::new(value, reg_class_of(graph.node(value).repr)))
}

/// Build live intervals (with holes and loop extension) for every value.
fn build_intervals(
    graph: &Graph,
    liveness: &Liveness,
    numbering: &Numbering,
    loops: &[NaturalLoop],
    deopt_uses: &FxHashMap<NodeId, Vec<NodeId>>,
    call_positions: &[u32],
) -> Vec<Interval> {
    let mut intervals: FxHashMap<NodeId, Interval> = FxHashMap::default();

    // Reverse linear scan of blocks (Wimmer buildIntervals): later blocks first.
    for &b in numbering.order.iter().rev() {
        let block = graph.block(b);
        let (first, _last) = numbering.span(b);
        let block_start = first;
        let block_end = numbering.block_end(b);

        // Seed: every value live-out of this block is live for the whole block.
        for &v in &liveness.live_out[b as usize] {
            interval_for(&mut intervals, graph, v).add_range(block_start, block_end);
        }

        // Walk the block's operations in reverse: body (reversed) then phis.
        // A def `set_from`s the leading range; each input use `add_range`s
        // [block_start, use_pos] and records the use.
        for &nid in block.body.iter().rev() {
            let def_pos = numbering.def_pos(nid);
            let use_pos = numbering.pos(nid);
            interval_for(&mut intervals, graph, nid).set_from(def_pos);
            // Phi inputs are edge uses, resolved separately; a body node's inputs
            // are real block-local uses.
            for input in graph.node(nid).kind.inputs() {
                let iv = interval_for(&mut intervals, graph, input);
                iv.add_range(block_start, use_pos);
                iv.add_use(use_pos);
            }
            // Deopt frame-state values a guard reads are uses at the guard, so
            // the value stays in a home the deopt exit can restore from.
            if let Some(dvals) = deopt_uses.get(&nid) {
                for &v in dvals {
                    let iv = interval_for(&mut intervals, graph, v);
                    iv.add_range(block_start, use_pos);
                    iv.add_use(use_pos);
                }
            }
        }
        // Terminator use (Branch.cond / Return value): live to the block end.
        if let Some(cond) = terminator_use(graph, b) {
            let iv = interval_for(&mut intervals, graph, cond);
            iv.add_range(block_start, block_end - 1);
            iv.add_use(block_end - 1);
        }
        // Phi heads define at block entry; shorten their leading range. Only a
        // phi that is actually live — already given an interval by the live-out
        // seeding or a real use above — needs this. A phi with no interval here is
        // dead (no use, not live-out of its block); minting one via `set_from`
        // would hand it a spurious degenerate range and a register home, and the
        // edge resolver would then emit a move into that home. When the dead phi's
        // home aliases a live value (linear scan is free to reuse it — the dead
        // phi never interferes), that move clobbers the live value on the edge.
        for &phi in &block.phis {
            if let Some(iv) = intervals.get_mut(&phi) {
                iv.set_from(numbering.def_pos(phi));
            }
        }
    }

    // Loop extension: a value live through a loop (live-in at the header *and*
    // live-out of some body block) must hold its register across the whole loop,
    // so extend its interval to the loop end (precomputed in `detect_loops`).
    for lp in loops {
        let header_start = numbering.span(lp.header).0;
        let end = lp.end_pos;
        // Values live into the loop header and still live somewhere in the loop
        // body span the whole loop.
        let live_through: FxHashSet<NodeId> = liveness.live_in[lp.header as usize]
            .iter()
            .copied()
            .filter(|v| {
                lp.blocks
                    .iter()
                    .any(|&b| liveness.live_out[b as usize].contains(v))
            })
            .collect();
        for v in live_through {
            interval_for(&mut intervals, graph, v).add_range(header_start, end);
        }
    }

    let mut result: Vec<Interval> = intervals
        .into_values()
        .filter(|iv| !iv.ranges.is_empty())
        .collect();
    // Flag intervals that are live across a call site (a call position strictly
    // inside the live range): those values must avoid caller-saved registers.
    for iv in &mut result {
        let (start, end) = (iv.start(), iv.end());
        iv.crosses_call = call_positions.iter().any(|&c| start < c && c < end);
    }
    // Deterministic order: by start position, then value id.
    result.sort_by_key(|iv| (iv.start(), iv.value));
    result
}

/// The single value a block's terminator reads (Branch predicate / Return
/// value), if any. Phi inputs are *not* block-local uses and are excluded.
fn terminator_use(graph: &Graph, block: BlockId) -> Option<NodeId> {
    use super::ir::Terminator;
    match &graph.block(block).term {
        Some(Terminator::Return(v)) => Some(*v),
        Some(Terminator::Branch { cond, .. }) => Some(*cond),
        Some(Terminator::Jump(_)) | Some(Terminator::Deopt(_)) | None => None,
    }
}

// --- Linear scan ------------------------------------------------------------

/// The linear-scan core: processes start-sorted intervals against active /
/// inactive lists, producing one home per interval.
struct LinearScan {
    /// Allocated intervals, indexed by a dense local id.
    intervals: Vec<Interval>,
    /// General-purpose registers available (`RegClass::Gp`).
    gp_regs: u32,
    /// Floating-point registers available (`RegClass::Fp`).
    fp_regs: u32,
    /// Count of caller-saved GP registers — abstract `Reg(0..caller_saved_gp)`.
    /// A call-crossing interval is forbidden from these.
    caller_saved_gp: u32,
    /// Count of caller-saved FP registers — abstract `Reg(0..caller_saved_fp)`.
    caller_saved_fp: u32,
    /// Spill slots handed out so far.
    spill_slots: u32,
}

impl LinearScan {
    fn run(
        intervals: Vec<Interval>,
        gp_regs: u32,
        fp_regs: u32,
        caller_saved_gp: u32,
        caller_saved_fp: u32,
    ) -> Self {
        let mut scan = LinearScan {
            intervals,
            gp_regs,
            fp_regs,
            caller_saved_gp,
            caller_saved_fp,
            spill_slots: 0,
        };
        scan.allocate_all();
        scan
    }

    /// Physical register count for `class`.
    fn regs_of(&self, class: RegClass) -> u32 {
        match class {
            RegClass::Gp => self.gp_regs,
            RegClass::Fp => self.fp_regs,
        }
    }

    /// Count of caller-saved registers for `class`.
    fn caller_saved_of(&self, class: RegClass) -> u32 {
        match class {
            RegClass::Gp => self.caller_saved_gp,
            RegClass::Fp => self.caller_saved_fp,
        }
    }

    fn allocate_all(&mut self) {
        // Unhandled intervals as local ids in ascending start order (already
        // sorted by the builder).
        let order: Vec<usize> = (0..self.intervals.len()).collect();

        // Active: allocated and live at the current position. Inactive:
        // allocated but currently in a hole (will be live again later).
        let mut active: Vec<usize> = Vec::new();
        let mut inactive: Vec<usize> = Vec::new();

        for &cur in &order {
            let position = self.intervals[cur].start();

            // Expire/transition: move active intervals that ended to handled, and
            // those now in a hole to inactive; revive inactive intervals.
            let mut still_active = Vec::with_capacity(active.len());
            for &a in &active {
                if self.intervals[a].end() <= position {
                    // Ended — drop (handled).
                } else if self.intervals[a].covers(position) {
                    still_active.push(a);
                } else {
                    inactive.push(a);
                }
            }
            active = still_active;

            let mut still_inactive = Vec::with_capacity(inactive.len());
            for &i in &inactive {
                if self.intervals[i].end() <= position {
                    // Ended — drop.
                } else if self.intervals[i].covers(position) {
                    active.push(i);
                } else {
                    still_inactive.push(i);
                }
            }
            inactive = still_inactive;

            // Try to give `cur` a register; else spill.
            if !self.try_allocate_free(cur, position, &active, &inactive) {
                self.allocate_blocked(cur, position, &mut active, &mut inactive);
            }

            if matches!(self.intervals[cur].location, Some(Location::Reg(_))) {
                active.push(cur);
            }
        }
    }

    /// Try to assign `cur` the register free for the longest span from
    /// `position`. Returns whether a register covering the whole interval was
    /// found.
    fn try_allocate_free(
        &mut self,
        cur: usize,
        _position: u32,
        active: &[usize],
        inactive: &[usize],
    ) -> bool {
        let class = self.intervals[cur].class;
        // free_until[r] = first position at which register r is next needed.
        let mut free_until = vec![u32::MAX; self.regs_of(class) as usize];

        // Active intervals of the same class occupy their register from now on.
        for &a in active {
            if self.intervals[a].class != class {
                continue;
            }
            if let Some(Location::Reg(r)) = self.intervals[a].location {
                free_until[r as usize] = 0;
            }
        }
        // Without interval splitting/reload insertion, an inactive interval
        // still owns its register across holes: reusing that register would
        // clobber the value before its later use.
        for &i in inactive {
            if self.intervals[i].class != class {
                continue;
            }
            if let Some(Location::Reg(r)) = self.intervals[i].location {
                free_until[r as usize] = 0;
            }
        }
        // A value live across a call must not occupy a caller-saved register —
        // the call clobbers those. Mark them unavailable for this interval so it
        // takes a callee-saved register (or spills).
        if self.intervals[cur].crosses_call {
            for slot in free_until
                .iter_mut()
                .take(self.caller_saved_of(class) as usize)
            {
                *slot = 0;
            }
        }

        // Pick the register free longest.
        let (best_reg, best_free) = free_until
            .iter()
            .copied()
            .enumerate()
            .max_by_key(|&(_, f)| f)
            .map(|(r, f)| (r as u32, f))
            .unwrap_or((0, 0));

        if best_free == 0 {
            // No register free at `position`.
            return false;
        }
        if best_free >= self.intervals[cur].end() {
            // Free for the interval's whole life.
            self.intervals[cur].location = Some(Location::Reg(best_reg));
            true
        } else {
            // A register is free for part of the life. Without splitting we
            // cannot use a partially-free register, so report failure and let the
            // spill path decide between spilling `cur` and evicting a blocker.
            false
        }
    }

    /// No register covers `cur`'s whole life: spill `cur` or evict the
    /// active/inactive interval whose next use is furthest, sending the loser to
    /// a fresh spill slot.
    fn allocate_blocked(
        &mut self,
        cur: usize,
        position: u32,
        active: &mut Vec<usize>,
        inactive: &mut Vec<usize>,
    ) {
        let class = self.intervals[cur].class;

        // next_use[r] = soonest next use of any interval currently owning
        // register r. Inactive intervals still reserve their register because
        // this allocator has one home per value and no reloads at hole exits.
        // A register whose occupant is needed furthest out is the cheapest to
        // free. `occupied[r]` records that *some* interval holds `r`, so a
        // still-live occupant whose next use is `MAX` is never mistaken for free.
        let mut next_use = vec![u32::MAX; self.regs_of(class) as usize];
        let mut occupied = vec![false; self.regs_of(class) as usize];

        for &a in active.iter() {
            if self.intervals[a].class != class {
                continue;
            }
            if let Some(Location::Reg(r)) = self.intervals[a].location {
                let u = self.intervals[a].next_use(position);
                if !occupied[r as usize] || u < next_use[r as usize] {
                    next_use[r as usize] = u;
                    occupied[r as usize] = true;
                }
            }
        }
        for &i in inactive.iter() {
            if self.intervals[i].class != class {
                continue;
            }
            if let Some(Location::Reg(r)) = self.intervals[i].location {
                let u = self.intervals[i].next_use(position);
                if !occupied[r as usize] || u < next_use[r as usize] {
                    next_use[r as usize] = u;
                    occupied[r as usize] = true;
                }
            }
        }

        // A call-crossing interval cannot land in a caller-saved register; mark
        // them occupied-now so the furthest-use pick never selects one (ties
        // resolve to the higher, callee-saved indices).
        if self.intervals[cur].crosses_call {
            for r in 0..self.caller_saved_of(class) as usize {
                next_use[r] = 0;
                occupied[r] = true;
            }
        }

        // Best register = the one whose current occupant is needed furthest out.
        let (best_reg, best_next_use) = next_use
            .iter()
            .copied()
            .enumerate()
            .max_by_key(|&(_, u)| u)
            .map(|(r, u)| (r as u32, u))
            .unwrap_or((0, 0));

        let cur_first_use = self.intervals[cur].next_use(position);

        if cur_first_use <= best_next_use && best_next_use != u32::MAX {
            // Every register's current occupant is used before `cur` is (or `cur`
            // has no use at all) — spilling a holder would be a net loss, so spill
            // `cur` whole. (With splitting we would instead split `cur` after its
            // first use; spilling whole is the conservative no-split choice.)
            let slot = self.fresh_spill();
            self.intervals[cur].location = Some(Location::Spill(slot));
        } else {
            // `cur` is needed sooner than `best_reg`'s occupant: take the
            // register and spill *every* interval that occupies it and overlaps
            // `cur` — not just the soonest-used blocker — so no overlap survives.
            // (Splitting would instead split only the conflicting sub-range.)
            self.evict_overlapping(cur, best_reg, active, inactive);
            self.intervals[cur].location = Some(Location::Reg(best_reg));
        }
    }

    /// Spill every active/inactive interval assigned `reg` whose live range
    /// overlaps `cur`'s, removing them from the worklists. Guarantees that giving
    /// `cur` `reg` leaves no two overlapping intervals sharing it.
    fn evict_overlapping(
        &mut self,
        cur: usize,
        reg: u32,
        active: &mut Vec<usize>,
        inactive: &mut Vec<usize>,
    ) {
        let victims: Vec<usize> = active
            .iter()
            .chain(inactive.iter())
            .copied()
            .filter(|&x| {
                self.intervals[x].location == Some(Location::Reg(reg))
                    && self.interval_lifetimes_overlap(x, cur)
            })
            .collect();
        for victim in victims {
            let slot = self.fresh_spill();
            self.intervals[victim].location = Some(Location::Spill(slot));
            active.retain(|&x| x != victim);
            inactive.retain(|&x| x != victim);
        }
    }

    /// Whether two intervals are live at any common position.
    fn interval_lifetimes_overlap(&self, a: usize, b: usize) -> bool {
        self.intervals[a].start() < self.intervals[b].end()
            && self.intervals[b].start() < self.intervals[a].end()
    }

    fn fresh_spill(&mut self) -> u32 {
        let slot = self.spill_slots;
        self.spill_slots += 1;
        slot
    }

    /// Final value → home map.
    fn locations(&self) -> FxHashMap<NodeId, Location> {
        self.intervals
            .iter()
            .filter_map(|iv| iv.location.map(|loc| (iv.value, loc)))
            .collect()
    }

    /// The home of `value` at any point in its (unsplit) life.
    fn location_of(&self, value: NodeId) -> Option<Location> {
        self.intervals
            .iter()
            .find(|iv| iv.value == value)
            .and_then(|iv| iv.location)
    }
}

// --- Resolution -------------------------------------------------------------

/// Build the per-edge parallel-move sets: phi lowering plus cross-edge
/// reconciliation. Without splitting a value's home is constant, so the only
/// moves needed are phi-input → phi-home; the structure nonetheless enumerates
/// every edge and would carry value-relocation moves once splitting introduces
/// per-position homes.
fn resolve(
    graph: &Graph,
    liveness: &Liveness,
    numbering: &Numbering,
    scan: &LinearScan,
    location: &FxHashMap<NodeId, Location>,
) -> Vec<EdgeMoves> {
    let mut edge_moves = Vec::new();

    for &pred in &numbering.order {
        let succs = successors(graph, pred);
        for succ in succs {
            let mut moves: Vec<(Location, Location)> = Vec::new();

            // Phi resolution: for each phi in `succ`, move this edge's input from
            // its pred-end home to the phi's home.
            let pred_idx = graph.block(succ).preds.iter().position(|&p| p == pred);
            if let Some(idx) = pred_idx {
                for &phi in &graph.block(succ).phis {
                    let inputs = graph.node(phi).kind.inputs();
                    if let Some(&input) = inputs.get(idx)
                        // A `Float64` input lives in an FP register; its phi
                        // reconciliation is a cross-class `fmov` the emitter does
                        // directly (see `emit_edge`), not a GP parallel move, so
                        // it must stay out of this same-class move set.
                        && graph.node(input).repr != Repr::Float64
                        && let (Some(&dst), Some(src)) =
                            (location.get(&phi), scan.location_of(input))
                        && src != dst
                    {
                        moves.push((src, dst));
                    }
                }
            }

            // Cross-edge value reconciliation. With one home per value the source
            // and target homes coincide, so nothing is emitted here today; the
            // loop is the seam where split-induced relocations attach.
            for &v in &liveness.live_in[succ as usize] {
                if graph.block(succ).phis.contains(&v) {
                    continue; // handled above
                }
                if let (Some(&dst), Some(src)) = (location.get(&v), scan.location_of(v))
                    && src != dst
                {
                    moves.push((src, dst));
                }
            }

            if !moves.is_empty() {
                let ordered = order_parallel_moves(moves, scan.spill_slots);
                edge_moves.push(EdgeMoves {
                    pred,
                    succ,
                    moves: ordered,
                });
            }
        }
    }

    edge_moves
}

/// Order a parallel move set into a sequential schedule: emit a move only once
/// its destination is not also a pending source, and break any remaining cycle
/// by routing one member through a scratch slot. The scratch is a fresh spill
/// index (`spill_slots`) the emitter reserves; it is never a live value's home.
fn order_parallel_moves(
    moves: Vec<(Location, Location)>,
    scratch_slot: u32,
) -> Vec<(Location, Location)> {
    // Drop trivial and de-duplicate by destination (a destination is written
    // once; SSA + per-edge construction guarantees at most one writer already).
    let mut pending: Vec<(Location, Location)> =
        moves.into_iter().filter(|(s, d)| s != d).collect();
    let mut ordered = Vec::with_capacity(pending.len());

    while !pending.is_empty() {
        // Emit every move whose destination is not the source of any remaining
        // move (safe — its target register/slot is dead).
        let mut progressed = false;
        let mut i = 0;
        while i < pending.len() {
            let (src, dst) = pending[i];
            let dst_is_needed = pending
                .iter()
                .enumerate()
                .any(|(j, &(s, _))| j != i && s == dst);
            if !dst_is_needed {
                ordered.push((src, dst));
                pending.remove(i);
                progressed = true;
            } else {
                i += 1;
            }
        }
        if progressed {
            continue;
        }
        // Only cycles remain (every destination is some other move's source).
        // Break one: park a member's source in the scratch and rewrite every
        // move reading it to read the scratch instead. The parked move itself
        // stays pending — its destination is still another pending move's
        // source, so writing it now would clobber a value not yet consumed
        // (e.g. a two-register swap). With its dependency on the live source
        // gone, the cycle is open and the readiness rule above schedules the
        // remaining moves, the parked one last.
        let (src, _) = pending[0];
        let scratch = Location::Spill(scratch_slot);
        ordered.push((src, scratch));
        for m in pending.iter_mut() {
            if m.0 == src {
                m.0 = scratch;
            }
        }
    }

    ordered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optimizing::{build_graph, liveness};
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
                method_hint: otter_vm::jit::JitMethodHint::None,
                load_number: None,
                property_feedback: None,
                property_feedback_poly: Vec::new(),
                property_proto_feedback: None,
                object_literal: None,
                element_load_kind: otter_vm::jit::JitElementLoadKind::Any,
                global_lex_cell: None,
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

    /// Recompute intervals for assertion: maps value → its interval ranges.
    fn intervals_of(graph: &Graph, live: &Liveness) -> Vec<Interval> {
        let numbering = Numbering::build(graph, live);
        let loops = detect_loops(graph, live, &numbering);
        build_intervals(graph, live, &numbering, &loops, &FxHashMap::default(), &[])
    }

    /// Assert no two values whose intervals overlap share the same `Reg`.
    fn assert_interference_free(graph: &Graph, live: &Liveness, alloc: &Allocation) {
        let intervals = intervals_of(graph, live);
        for (i, a) in intervals.iter().enumerate() {
            for b in intervals.iter().skip(i + 1) {
                if a.value == b.value {
                    continue;
                }
                let la = alloc.location.get(&a.value);
                let lb = alloc.location.get(&b.value);
                let (Some(&Location::Reg(ra)), Some(&Location::Reg(rb))) = (la, lb) else {
                    continue;
                };
                if ra != rb {
                    continue;
                }
                // Same register: their ranges must not overlap at any position.
                let overlap = a
                    .ranges
                    .iter()
                    .any(|&(af, at)| b.ranges.iter().any(|&(bf, bt)| af < bt && bf < at));
                assert!(
                    !overlap,
                    "values {} and {} share Reg({ra}) but their intervals overlap",
                    a.value, b.value
                );
            }
        }
    }

    #[test]
    fn diamond_allocation_is_interference_free() {
        // if (r0 < r1) r2 = 1 else r2 = 2; return r2  (a phi merge)
        let g = build_graph(&view(
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
        let live = liveness::analyze(&g, &Default::default(), &Default::default());

        let alloc = allocate(&g, &live, 7, 6, 7, 6, &Default::default());
        assert_interference_free(&g, &live, &alloc);

        // The merge phi must be correctly reconciled from *both* predecessors:
        // on each edge, the phi's input either already sits in the phi's home or a
        // resolution move places it there.
        let merge = g
            .blocks
            .iter()
            .position(|b| b.start_pc == 5 * STRIDE)
            .unwrap() as BlockId;
        let phi = g.block(merge).phis[0];
        assert_phi_resolved(&g, &alloc, merge, phi);
    }

    /// Assert every predecessor edge into `block` lands `phi`'s corresponding
    /// input in `phi`'s home (already-equal or via a resolution move on that
    /// edge).
    fn assert_phi_resolved(graph: &Graph, alloc: &Allocation, block: BlockId, phi: NodeId) {
        let phi_home = alloc.location[&phi];
        let inputs = graph.node(phi).kind.inputs();
        for (i, &pred) in graph.block(block).preds.iter().enumerate() {
            let input = inputs[i];
            let input_home = alloc.location[&input];
            if input_home == phi_home {
                continue; // already in place; no move needed
            }
            let edge = alloc
                .edge_moves
                .iter()
                .find(|e| e.pred == pred && e.succ == block)
                .unwrap_or_else(|| {
                    panic!("edge {pred}→{block} needs a move for phi input but has none")
                });
            assert!(
                edge.moves.iter().any(|&(_, d)| d == phi_home),
                "edge {pred}→{block} must move phi input into the phi home {phi_home:?}"
            );
        }
    }

    #[test]
    fn counting_loop_allocation_is_interference_free() {
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
        let live = liveness::analyze(&g, &Default::default(), &Default::default());

        let alloc = allocate(&g, &live, 7, 6, 7, 6, &Default::default());
        assert_interference_free(&g, &live, &alloc);

        // The loop has header phis (i and acc); the back edge must carry phi
        // resolution moves into the header.
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
        assert!(
            !g.block(header).phis.is_empty(),
            "loop header has phis to resolve"
        );
        // Every header phi is correctly reconciled on *both* the entry edge and
        // the back edge (input already at the phi home, or a move places it).
        for &phi in &g.block(header).phis {
            assert_phi_resolved(&g, &alloc, header, phi);
        }
        // The back edge `body → header` carries next-iteration loop values whose
        // homes differ from the header-phi homes, so it must produce real moves.
        let back_edge = alloc
            .edge_moves
            .iter()
            .find(|e| e.pred == body && e.succ == header)
            .expect("back edge body→header must carry phi-resolution moves");
        let phi_homes: FxHashSet<Location> = g
            .block(header)
            .phis
            .iter()
            .filter_map(|p| alloc.location.get(p).copied())
            .collect();
        assert!(
            back_edge.moves.iter().any(|&(_, d)| phi_homes.contains(&d)),
            "back-edge moves must feed the header phis"
        );
    }

    #[test]
    fn tight_register_budget_forces_spill_without_interference() {
        // Same counting loop, but only 1 GP register: the allocator must spill
        // and stay interference-free.
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
        let live = liveness::analyze(&g, &Default::default(), &Default::default());

        let alloc = allocate(&g, &live, 1, 6, 1, 6, &Default::default());
        assert_interference_free(&g, &live, &alloc);
        assert!(
            alloc.spill_slots >= 1,
            "a 1-register budget on a multi-value loop must spill"
        );
    }
    /// Simulate a parallel-move schedule: apply the ordered moves to an
    /// environment keyed by location and return the final register state.
    fn run_moves(
        init: &[(Location, i64)],
        ordered: &[(Location, Location)],
    ) -> std::collections::HashMap<Location, i64> {
        let mut env: std::collections::HashMap<Location, i64> = init.iter().copied().collect();
        for &(src, dst) in ordered {
            let v = *env.get(&src).expect("move reads an unwritten location");
            env.insert(dst, v);
        }
        env
    }

    #[test]
    fn parallel_moves_two_register_swap() {
        let a = Location::Reg(0);
        let b = Location::Reg(1);
        let ordered = order_parallel_moves(vec![(a, b), (b, a)], 7);
        let env = run_moves(&[(a, 10), (b, 20)], &ordered);
        assert_eq!(env[&a], 20, "swap must move b's old value into a");
        assert_eq!(env[&b], 10, "swap must move a's old value into b");
    }

    #[test]
    fn parallel_moves_three_cycle() {
        let a = Location::Reg(0);
        let b = Location::Reg(1);
        let c = Location::Reg(2);
        // a->b, b->c, c->a (each destination is another move's source).
        let ordered = order_parallel_moves(vec![(a, b), (b, c), (c, a)], 7);
        let env = run_moves(&[(a, 1), (b, 2), (c, 3)], &ordered);
        assert_eq!((env[&b], env[&c], env[&a]), (1, 2, 3));
    }

    #[test]
    fn parallel_moves_cycle_with_chain() {
        // The shape that miscompiled crypto's am3 loop entry: a two-register
        // swap {j<->n} plus an independent chain {c->x, i->c}.
        let n_ = Location::Reg(0);
        let j = Location::Reg(1);
        let i = Location::Reg(2);
        let c = Location::Reg(3);
        let x = Location::Reg(4);
        let ordered = order_parallel_moves(vec![(n_, j), (j, n_), (c, x), (i, c)], 7);
        let env = run_moves(
            &[(n_, 100), (j, 200), (i, 300), (c, 400), (x, 500)],
            &ordered,
        );
        assert_eq!(env[&j], 100);
        assert_eq!(env[&n_], 200);
        assert_eq!(env[&x], 400);
        assert_eq!(env[&c], 300);
    }
}
