//! Graph construction: an abstract interpretation of the bytecode that keeps
//! one SSA value per interpreter register and turns feedback into
//! speculation.
//!
//! # Contents
//! - [`build`] — the entry point: bytecode analysis plus snapshot feedback to
//!   a finished [`Graph`] whose blocks are in emission order.
//! - The per-opcode lowering in [`Builder::visit`]: every instruction becomes
//!   specialized nodes, a [`Kind::Generic`] node, or an unconditional deopt.
//! - [`Known`] — facts about values that hold along the current path
//!   (alternative representations, proved shapes), merged at joins.
//!
//! # Invariants
//! - No instruction makes the whole function fail: anything without a
//!   specialized form becomes a `Generic` node, and an operation that cannot
//!   run in optimized code at all (suspension, an irreducible entry) becomes
//!   an unconditional deopt that resumes the interpreter at its PC.
//! - Blocks are visited in reverse post-order, so every forward predecessor
//!   of a join has been visited before the join starts; a join's phis are
//!   created from the complete set of forward states, and back edges append
//!   their inputs when they arrive.
//! - Frame states carry exactly the registers live before (eager) or after
//!   (lazy) the instruction; dead registers resume as `undefined`.
//! - A fact in [`Known`] about a node never outlives the path it was proved
//!   on: joins keep only facts every predecessor agrees on, loop headers drop
//!   heap facts, and calls and stores drop shape facts.
//!
//! # See also
//! - [`super::bytecode`] — blocks, loops and liveness.
//! - [`super::ir`] — the node vocabulary.

use otter_bytecode::Op;
use otter_bytecode::opcode_schema::ContextCoord;
use otter_vm::{JitCompileSnapshot, value::tag};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use super::bytecode::{Analysis, Flow, Instruction, RegisterSet};
use super::ir::{
    BlockId, BranchKind, Condition, DeoptReason, FrameState, FrameStateId, Graph, Kind, NodeId,
    Repr,
};

const UNDEFINED: u64 = tag::VALUE_UNDEFINED;

/// What is known about one value along the current path.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct NodeInfo {
    /// The same value as an unboxed int32.
    pub(crate) int32: Option<NodeId>,
    /// The same value as an unboxed double.
    pub(crate) float64: Option<NodeId>,
    /// The same value as a tagged word.
    pub(crate) tagged: Option<NodeId>,
    /// Shapes the value is proved to have (an ordinary object with one of
    /// them).
    pub(crate) shapes: Option<SmallVec<[u32; 4]>>,
    /// The value is proved to be a heap cell.
    pub(crate) heap_object: bool,
    /// The value is proved to be a Number.
    pub(crate) number: bool,
    /// The shape proof also excludes prototype objects.
    pub(crate) writable: bool,
}

/// Facts along the current path.
#[derive(Debug, Clone, Default)]
pub(crate) struct Known {
    info: FxHashMap<NodeId, NodeInfo>,
}

impl Known {
    fn get(&self, node: NodeId) -> Option<&NodeInfo> {
        self.info.get(&node)
    }

    fn entry(&mut self, node: NodeId) -> &mut NodeInfo {
        self.info.entry(node).or_default()
    }

    /// Keep only facts `other` agrees on.
    fn intersect(&mut self, other: &Known) {
        self.info.retain(|node, info| {
            let Some(theirs) = other.info.get(node) else {
                return false;
            };
            if info.int32 != theirs.int32 {
                info.int32 = None;
            }
            if info.float64 != theirs.float64 {
                info.float64 = None;
            }
            if info.tagged != theirs.tagged {
                info.tagged = None;
            }
            if info.shapes != theirs.shapes {
                info.shapes = None;
            }
            info.writable &= theirs.writable;
            info.heap_object &= theirs.heap_object;
            info.number &= theirs.number;
            *info != NodeInfo::default()
        });
    }

    /// Forget every fact a heap write may invalidate.
    fn clobber_heap(&mut self) {
        for info in self.info.values_mut() {
            info.shapes = None;
            info.writable = false;
        }
    }
}

/// One incoming edge's state at a join.
#[derive(Debug, Clone)]
struct Incoming {
    block: BlockId,
    frame: Vec<NodeId>,
    known: Known,
}

/// Why graph construction stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuildError {
    /// The bytecode could not be analysed.
    Analysis(super::bytecode::AnalysisError),
    /// The OSR PC does not start a reachable block.
    OsrTarget,
}

/// A finished graph plus the facts later phases need.
#[derive(Debug)]
pub(crate) struct Built {
    pub(crate) graph: Graph,
    /// Blocks in emission order.
    pub(crate) layout: Vec<BlockId>,
    /// The block an OSR entry jumps to; it reads every register live at the
    /// loop header from the interpreter's window.
    pub(crate) osr_entry: Option<BlockId>,
}

struct Builder<'a> {
    view: &'a JitCompileSnapshot,
    analysis: &'a Analysis,
    baseline: &'a super::BaselineSupport,
    graph: Graph,
    function_id: u32,
    register_count: u16,
    /// Graph block per bytecode block.
    block_map: Vec<BlockId>,
    /// Split blocks appended after a bytecode block in emission order.
    trailing: Vec<Vec<BlockId>>,
    incoming: Vec<Vec<Incoming>>,
    /// Loop-header phis per bytecode header: `(register, phi)`.
    loop_phis: FxHashMap<usize, Vec<(u16, NodeId)>>,
    current: Option<BlockId>,
    current_bytecode_block: usize,
    frame: Vec<NodeId>,
    known: Known,
    /// The graph has an OSR entry block.
    osr_entry: bool,
    /// Eager frame states already built, by PC.
    eager_states: FxHashMap<u32, FrameStateId>,
    pc: u32,
    undefined: NodeId,
}

/// Build the graph of `view`.
pub(crate) fn build(
    view: &JitCompileSnapshot,
    analysis: &Analysis,
    baseline: &'_ super::BaselineSupport,
    osr_pc: Option<u32>,
) -> Result<Built, BuildError> {
    let code = view.code_block.as_ref();
    let register_count = code.register_count;
    let mut graph = Graph::default();
    let undefined = graph.constant(Kind::ConstTagged(UNDEFINED));
    let block_count = analysis.blocks.len();
    let mut block_map = vec![BlockId(u32::MAX); block_count];
    for &block in &analysis.order {
        block_map[block] = graph.new_block();
    }
    let mut builder = Builder {
        view,
        analysis,
        baseline,
        graph,
        function_id: code.id,
        register_count,
        block_map,
        trailing: vec![Vec::new(); block_count],
        incoming: vec![Vec::new(); block_count],
        loop_phis: FxHashMap::default(),
        current: None,
        current_bytecode_block: 0,
        frame: vec![undefined; usize::from(register_count)],
        known: Known::default(),
        osr_entry: osr_pc.is_some(),
        eager_states: FxHashMap::default(),
        pc: 0,
        undefined,
    };
    let entry = builder.graph.new_block();
    builder.start_entry(entry);
    let mut layout = vec![entry];
    let mut osr_entry = None;
    if let Some(pc) = osr_pc {
        let header = *analysis
            .block_of
            .get(pc as usize)
            .ok_or(BuildError::OsrTarget)?;
        if analysis.blocks[header].start != pc || !analysis.blocks[header].reachable {
            return Err(BuildError::OsrTarget);
        }
        let block = builder.graph.new_block();
        builder.start_osr_entry(block, header);
        layout.push(block);
        osr_entry = Some(block);
    }
    let order = analysis.order.clone();
    for block in order {
        if !builder.start_block(block) {
            continue;
        }
        layout.push(builder.block_map[block]);
        let range = analysis.blocks[block].start..analysis.blocks[block].end;
        for pc in range.clone() {
            builder.pc = pc;
            builder.visit(&analysis.instructions[pc as usize]);
            if builder.current.is_none() {
                break;
            }
        }
        if builder.current.is_some() {
            // Fell off the end of the block into its single successor.
            let last = &analysis.instructions[range.end as usize - 1];
            builder.pc = last.pc;
            match analysis.blocks[block].successors.first() {
                Some(&successor) => builder.goto(successor),
                None => builder.deopt(DeoptReason::Unsupported),
            }
        }
        layout.extend(builder.trailing[block].iter().copied());
    }
    builder.finish_loops();
    Ok(Built {
        graph: builder.graph,
        layout,
        osr_entry,
    })
}

impl<'a> Builder<'a> {
    // ------------------------------------------------------------------
    // Blocks, edges and merges
    // ------------------------------------------------------------------

    fn start_entry(&mut self, entry: BlockId) {
        self.current = Some(entry);
        let params = self.view.code_block.param_count.min(self.register_count);
        for register in 0..self.register_count {
            self.frame[usize::from(register)] = if register < params {
                self.add(Kind::InitialRegister(register), &[], Repr::Tagged)
            } else {
                self.undefined
            };
        }
        self.current_bytecode_block = usize::MAX;
        self.goto(0);
    }

    /// The OSR entry block: every register live at the loop header comes
    /// from the interpreter's window, and the block feeds the header as one
    /// more forward predecessor.
    fn start_osr_entry(&mut self, block: BlockId, header: usize) {
        self.current = Some(block);
        self.current_bytecode_block = usize::MAX;
        self.known = Known::default();
        let live = self
            .analysis
            .live_at(self.analysis.blocks[header].start)
            .clone();
        for register in 0..self.register_count {
            self.frame[usize::from(register)] = if live.contains(register) {
                self.add(Kind::InitialRegister(register), &[], Repr::Tagged)
            } else {
                self.undefined
            };
        }
        self.goto(header);
    }

    /// Begin bytecode block `block`; `false` when nothing reaches it.
    fn start_block(&mut self, block: usize) -> bool {
        let incoming = std::mem::take(&mut self.incoming[block]);
        if incoming.is_empty() {
            return false;
        }
        let graph_block = self.block_map[block];
        self.current = Some(graph_block);
        self.current_bytecode_block = block;
        let is_loop = self.analysis.loops.contains_key(&block);
        let start_pc = self.analysis.blocks[block].start;
        let live = self.analysis.live_at(start_pc).clone();
        self.graph.block_mut(graph_block).predecessors =
            incoming.iter().map(|edge| edge.block).collect();
        self.graph.block_mut(graph_block).is_loop = is_loop;

        let mut known = incoming[0].known.clone();
        for edge in &incoming[1..] {
            known.intersect(&edge.known);
        }
        if is_loop {
            known.clobber_heap();
        }
        let mut frame = incoming[0].frame.clone();
        let mut phis = Vec::new();
        for register in 0..self.register_count {
            let index = usize::from(register);
            if !live.contains(register) {
                frame[index] = self.undefined;
                continue;
            }
            let first = incoming[0].frame[index];
            // An OSR entry reaches outer loop headers along their back edges
            // only: there every live register is a merge, not just the ones
            // the body assigns.
            let assigned = is_loop
                && (self.osr_entry || self.analysis.loops[&block].assigned.contains(register));
            let differs = incoming.iter().any(|edge| edge.frame[index] != first);
            if !assigned && !differs {
                continue;
            }
            let inputs: SmallVec<[NodeId; 4]> = incoming
                .iter()
                .map(|edge| self.tagged_for_phi(edge, index))
                .collect();
            let phi = self.graph.add_node(Kind::Phi, &inputs, Repr::Tagged);
            self.graph.node_mut(phi).block = Some(graph_block);
            self.graph.block_mut(graph_block).phis.push(phi);
            frame[index] = phi;
            if is_loop {
                phis.push((register, phi));
            }
        }
        if is_loop {
            self.loop_phis.insert(block, phis);
        }
        self.frame = frame;
        self.known = known;
        true
    }

    /// The tagged form of an incoming edge's register value. A phi merges
    /// tagged values only; an unboxed value is boxed at the end of its
    /// predecessor, which is where the edge's state was captured.
    fn tagged_for_phi(&mut self, edge: &Incoming, index: usize) -> NodeId {
        let value = edge.frame[index];
        if self.graph.node(value).repr == Repr::Tagged || self.graph.node(value).kind.is_constant()
        {
            return self.tagged(value);
        }
        edge.known
            .get(value)
            .and_then(|info| info.tagged)
            .expect("an edge carries a tagged form of every unboxed live value")
    }

    /// Box every live unboxed register so the outgoing state can feed phis.
    fn box_live_registers(&mut self, live: &RegisterSet) {
        for register in live.iter() {
            let value = self.frame[usize::from(register)];
            if self.graph.node(value).repr != Repr::Tagged {
                self.tagged(value);
            }
        }
    }

    /// Leave the current block along a normal edge to bytecode block
    /// `target`.
    fn goto(&mut self, target: usize) {
        let Some(current) = self.current else {
            return;
        };
        let from = self.current_bytecode_block;
        let back_edge = from != usize::MAX
            && self.analysis.loops.contains_key(&target)
            && self.analysis.blocks[target].start <= self.analysis.blocks[from].start
            && self.loop_phis.contains_key(&target);
        let retreating = from != usize::MAX
            && self.analysis.blocks[target].start <= self.analysis.blocks[from].start;
        if retreating && !back_edge {
            // An edge into a block already built that is not a natural
            // loop's back edge: resume the interpreter at the jump.
            self.deopt(DeoptReason::Unsupported);
            return;
        }
        let target_start = self.analysis.blocks[target].start;
        let live = self.analysis.live_at(target_start).clone();
        self.box_live_registers(&live);
        let target_block = self.block_map[target];
        if back_edge {
            let phis = self.loop_phis[&target].clone();
            for (register, phi) in phis {
                let value = self.tagged(self.frame[usize::from(register)]);
                self.graph.node_mut(phi).inputs.push(value);
            }
            self.graph
                .block_mut(target_block)
                .predecessors
                .push(current);
            // An interrupt at the back edge resumes the interpreter at the
            // header with the values entering the next iteration.
            let header_pc = self.analysis.blocks[target].start;
            let state = self.frame_state(header_pc, &live, None);
            let jump = self.terminate(Kind::JumpLoop(target_block), &[]);
            self.graph.node_mut(jump).eager = Some(state);
            return;
        }
        self.incoming[target].push(Incoming {
            block: current,
            frame: self.frame.clone(),
            known: self.known.clone(),
        });
        self.terminate(Kind::Jump(target_block), &[]);
    }

    /// End the current block with a two-way branch whose targets are the
    /// bytecode blocks `if_true` and `if_false`.
    fn branch(&mut self, kind: BranchKind, inputs: &[NodeId], if_true: usize, if_false: usize) {
        let Some(current) = self.current else {
            return;
        };
        let true_block = self.edge_block(if_true);
        let false_block = self.edge_block(if_false);
        self.terminate(
            Kind::Branch {
                kind,
                if_true: true_block,
                if_false: false_block,
            },
            inputs,
        );
        // Each edge continues from its own split block with the branch's
        // facts: a box one edge materializes does not exist on the other.
        let known = self.known.clone();
        for (split, target) in [(true_block, if_true), (false_block, if_false)] {
            self.known = known.clone();
            self.current = Some(split);
            self.graph.block_mut(split).predecessors.push(current);
            self.goto(target);
        }
        self.current = None;
    }

    /// A fresh block for one branch edge; it jumps on to `target`.
    fn edge_block(&mut self, _target: usize) -> BlockId {
        let block = self.graph.new_block();
        let owner = self.current_bytecode_block;
        if owner == usize::MAX {
            unreachable!("the entry block never branches");
        }
        self.trailing[owner].push(block);
        block
    }

    fn terminate(&mut self, kind: Kind, inputs: &[NodeId]) -> NodeId {
        let block = self.current.take().expect("an open block");
        let node = self.graph.add_node(kind, inputs, Repr::None);
        self.graph.node_mut(node).block = Some(block);
        self.graph.block_mut(block).control = Some(node);
        node
    }

    /// End the current block with an unconditional eager deopt at `pc`.
    fn deopt(&mut self, reason: DeoptReason) {
        if self.current.is_none() {
            return;
        }
        let state = self.eager_state();
        let block = self.current.take().expect("an open block");
        let node = self.graph.add_node(Kind::Deopt(reason), &[], Repr::None);
        let node_ref = self.graph.node_mut(node);
        node_ref.block = Some(block);
        node_ref.eager = Some(state);
        self.graph.block_mut(block).control = Some(node);
    }

    /// Drop loop-header phis whose back edges never arrived, and phis whose
    /// inputs are all the same value.
    fn finish_loops(&mut self) {
        for (&header, phis) in &self.loop_phis {
            let block = self.block_map[header];
            let predecessors = self.graph.block(block).predecessors.len();
            for &(_, phi) in phis {
                debug_assert_eq!(self.graph.node(phi).inputs.len(), predecessors);
            }
            let forward = predecessors
                - self
                    .graph
                    .block(block)
                    .predecessors
                    .iter()
                    .filter(|&&predecessor| {
                        self.graph
                            .block(predecessor)
                            .control
                            .is_some_and(|control| {
                                matches!(self.graph.node(control).kind, Kind::JumpLoop(_))
                            })
                    })
                    .count();
            if forward == predecessors {
                self.graph.block_mut(block).is_loop = false;
            }
        }
    }

    // ------------------------------------------------------------------
    // Nodes, frame states and representations
    // ------------------------------------------------------------------

    fn add(&mut self, kind: Kind, inputs: &[NodeId], repr: Repr) -> NodeId {
        let block = self.current.expect("an open block");
        let properties = kind.properties();
        let node = self.graph.add_node(kind, inputs, repr);
        self.graph.node_mut(node).block = Some(block);
        self.graph.block_mut(block).body.push(node);
        if properties.eager_deopt {
            let state = self.eager_state();
            self.graph.node_mut(node).eager = Some(state);
        }
        // Only a call can change an object's shape or prototype state; a
        // slot store keeps every shape.
        if properties.call {
            self.known.clobber_heap();
        }
        node
    }

    fn constant_tagged(&mut self, bits: u64) -> NodeId {
        self.graph.constant(Kind::ConstTagged(bits))
    }

    fn constant_int32(&mut self, value: i32) -> NodeId {
        self.graph.constant(Kind::ConstInt32(value))
    }

    fn constant_float64(&mut self, value: f64) -> NodeId {
        self.graph.constant(Kind::ConstFloat64(value.to_bits()))
    }

    /// The canonical constant for a number: int32 when integral and in range
    /// (and not -0), double otherwise.
    fn constant_number(&mut self, value: f64) -> NodeId {
        let truncated = value as i32;
        if f64::from(truncated) == value && !(value == 0.0 && value.is_sign_negative()) {
            self.constant_int32(truncated)
        } else {
            self.constant_float64(value)
        }
    }

    /// The registers live before the current instruction, bound to their
    /// current values.
    fn eager_state(&mut self) -> FrameStateId {
        if let Some(&state) = self.eager_states.get(&self.pc) {
            return state;
        }
        let live = self.analysis.live_at(self.pc).clone();
        let state = self.frame_state(self.pc, &live, None);
        self.eager_states.insert(self.pc, state);
        state
    }

    /// The registers live after the current instruction, with `result`
    /// binding the instruction's written register to the node producing it.
    fn lazy_state(&mut self, result: Option<(u16, NodeId)>) -> FrameStateId {
        let next = self.pc + 1;
        let live = if (next as usize) < self.analysis.instructions.len() {
            self.analysis.live_at(next).clone()
        } else {
            RegisterSet::new(usize::from(self.register_count))
        };
        self.frame_state(next, &live, result)
    }

    fn frame_state(
        &mut self,
        pc: u32,
        live: &RegisterSet,
        result: Option<(u16, NodeId)>,
    ) -> FrameStateId {
        let registers = live
            .iter()
            .map(|register| match result {
                Some((written, node)) if written == register => (register, node),
                _ => (register, self.frame[usize::from(register)]),
            })
            .collect();
        let byte_pc = self
            .view
            .instructions
            .get(pc as usize)
            .map_or(self.view.code_block.bytecode_byte_len(), |metadata| {
                metadata.byte_pc
            });
        self.graph.add_frame_state(FrameState {
            pc,
            byte_pc,
            function_id: self.function_id,
            register_count: self.register_count,
            registers,
        })
    }

    /// Current value of register `register`.
    fn read(&mut self, register: u16) -> NodeId {
        self.frame[usize::from(register)]
    }

    /// Bind register `register` to `value`.
    fn write(&mut self, register: u16, value: NodeId) {
        self.frame[usize::from(register)] = value;
    }

    /// The tagged form of `value`.
    fn tagged(&mut self, value: NodeId) -> NodeId {
        let node = self.graph.node(value);
        match (&node.kind, node.repr) {
            (_, Repr::Tagged) => return value,
            (Kind::ConstInt32(int), _) => {
                let int = *int;
                return self.constant_tagged(tag::NUMBER_TAG | u64::from(int as u32));
            }
            (Kind::ConstFloat64(bits), _) => {
                let number = otter_vm::number::NumberValue::from_f64(f64::from_bits(*bits));
                return self.constant_tagged(otter_vm::Value::number(number).to_bits());
            }
            _ => {}
        }
        if let Some(tagged) = self.known.get(value).and_then(|info| info.tagged) {
            return tagged;
        }
        let repr = self.graph.node(value).repr;
        let boxed = match repr {
            Repr::Int32 => self.add(Kind::Int32ToTagged, &[value], Repr::Tagged),
            Repr::Float64 => self.add(Kind::Float64ToTagged, &[value], Repr::Tagged),
            Repr::Tagged | Repr::None | Repr::Word => unreachable!("not a boxable value"),
        };
        self.known.entry(value).tagged = Some(boxed);
        let info = self.known.entry(boxed);
        match repr {
            Repr::Int32 => info.int32 = Some(value),
            _ => info.float64 = Some(value),
        }
        boxed
    }

    /// The int32 form of `value`, guarded by an eager deopt when unproved.
    fn int32(&mut self, value: NodeId) -> NodeId {
        let node = self.graph.node(value);
        match (&node.kind, node.repr) {
            (_, Repr::Int32) => return value,
            (Kind::ConstTagged(bits), _) if bits & tag::NUMBER_TAG == tag::NUMBER_TAG => {
                let int = *bits as u32 as i32;
                return self.constant_int32(int);
            }
            _ => {}
        }
        if let Some(int) = self.known.get(value).and_then(|info| info.int32) {
            return int;
        }
        let repr = self.graph.node(value).repr;
        let unboxed = match repr {
            Repr::Tagged => self.add(Kind::CheckedTaggedToInt32, &[value], Repr::Int32),
            Repr::Float64 => self.add(Kind::CheckedFloat64ToInt32, &[value], Repr::Int32),
            Repr::Int32 | Repr::None | Repr::Word => unreachable!("not an int32 source"),
        };
        self.known.entry(value).int32 = Some(unboxed);
        if repr == Repr::Tagged {
            self.known.entry(unboxed).tagged = Some(value);
        }
        unboxed
    }

    /// The float64 form of `value`, guarded by an eager deopt when unproved.
    fn float64(&mut self, value: NodeId) -> NodeId {
        let node = self.graph.node(value);
        match (&node.kind, node.repr) {
            (_, Repr::Float64) => return value,
            (Kind::ConstInt32(int), _) => {
                let int = *int;
                return self.constant_float64(f64::from(int));
            }
            (Kind::ConstTagged(bits), _) if bits & tag::NUMBER_TAG != 0 => {
                let value = otter_vm::Value::from_bits(*bits);
                if let Some(number) = value.as_number() {
                    return self.constant_float64(number.as_f64());
                }
            }
            _ => {}
        }
        if let Some(double) = self.known.get(value).and_then(|info| info.float64) {
            return double;
        }
        let repr = self.graph.node(value).repr;
        let unboxed = match repr {
            Repr::Tagged => self.add(Kind::CheckedTaggedToFloat64, &[value], Repr::Float64),
            Repr::Int32 => self.add(Kind::Int32ToFloat64, &[value], Repr::Float64),
            Repr::Float64 | Repr::None | Repr::Word => unreachable!("not a number source"),
        };
        self.known.entry(value).float64 = Some(unboxed);
        unboxed
    }

    // ------------------------------------------------------------------
    // Instructions
    // ------------------------------------------------------------------

    fn visit(&mut self, instruction: &Instruction) {
        let op = instruction.op;
        match op {
            Op::Nop => {}
            Op::LoadUndefined => self.set_constant(instruction, UNDEFINED),
            Op::LoadNull => self.set_constant(instruction, tag::OTHER_TAG),
            Op::LoadTrue => self.set_constant(instruction, tag::OTHER_TAG | tag::BOOL_TAG | 1),
            Op::LoadFalse => self.set_constant(instruction, tag::OTHER_TAG | tag::BOOL_TAG),
            Op::LoadInt32 => {
                let value = self.constant_int32(instruction.imm32(1).unwrap_or(0));
                self.write(instruction.writes[0], value);
            }
            Op::LoadNumber => {
                let number = self.view.instructions[self.pc as usize]
                    .load_number
                    .unwrap_or(f64::NAN);
                let value = self.constant_number(number);
                self.write(instruction.writes[0], value);
            }
            Op::LoadLocal | Op::StoreLocal => {
                let value = self.read(instruction.reads[0]);
                self.write(instruction.writes[0], value);
            }
            Op::LoadSelf => {
                let value = self.add(Kind::LoadClosure, &[], Repr::Tagged);
                self.write(instruction.writes[0], value);
            }
            Op::LoadThis if !self.view.derived_constructor => {
                let value = self.add(Kind::LoadThis, &[], Repr::Tagged);
                self.write(instruction.writes[0], value);
            }
            Op::Jump => {
                let Flow::Jump { target } = instruction.flow else {
                    unreachable!("a jump has a target");
                };
                self.goto(self.analysis.block_of[target as usize]);
            }
            Op::JumpIfTrue | Op::JumpIfFalse | Op::JumpIfNullish => {
                self.visit_branch(instruction);
            }
            Op::Return | Op::ReturnValue => {
                let value = self.read(instruction.reads[0]);
                let value = self.tagged(value);
                self.terminate(Kind::Return, &[value]);
            }
            Op::ReturnUndefined => {
                let value = self.undefined;
                self.terminate(Kind::Return, &[value]);
            }
            Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Rem
            | Op::BitwiseAnd
            | Op::BitwiseOr
            | Op::BitwiseXor
            | Op::Shl
            | Op::Shr
            | Op::Ushr => self.visit_binary(instruction, None),
            Op::AddImm | Op::SubImm | Op::BitwiseAndImm => {
                let immediate = instruction.imm32(2).unwrap_or(0);
                self.visit_binary(instruction, Some(immediate));
            }
            Op::Increment => {
                let delta = instruction.imm32(2).unwrap_or(1);
                self.visit_increment(instruction, delta);
            }
            Op::Neg | Op::BitwiseNot => self.visit_unary(instruction),
            Op::ToNumeric | Op::ToNumber => self.visit_to_numeric(instruction),
            Op::LessThan
            | Op::LessEq
            | Op::GreaterThan
            | Op::GreaterEq
            | Op::Equal
            | Op::NotEqual => self.visit_compare(instruction, None),
            Op::LessThanImm | Op::EqualImm | Op::NotEqualImm => {
                let immediate = instruction.imm32(2).unwrap_or(0);
                self.visit_compare(instruction, Some(immediate));
            }
            Op::LoadProperty => self.visit_load_property(instruction),
            Op::StoreProperty | Op::StorePropertyStrict => self.visit_store_property(instruction),
            Op::LoadContextSlot => self.visit_load_context_slot(instruction),
            Op::StoreContextSlot => self.visit_store_context_slot(instruction),
            _ => self.generic(instruction),
        }
    }

    fn set_constant(&mut self, instruction: &Instruction, bits: u64) {
        let value = self.constant_tagged(bits);
        self.write(instruction.writes[0], value);
    }

    /// The instruction through its baseline operation on the frame window,
    /// or an unconditional deopt when it has none.
    fn generic(&mut self, instruction: &Instruction) {
        if !self.baseline.has_operation(self.pc) {
            self.deopt(DeoptReason::Unsupported);
            return;
        }
        let mut inputs: SmallVec<[NodeId; 4]> = SmallVec::new();
        let mut registers = Vec::new();
        for &register in &instruction.reads {
            if registers.contains(&register) {
                continue;
            }
            let value = self.frame[usize::from(register)];
            inputs.push(self.tagged(value));
            registers.push(register);
        }
        let node = self.add(
            Kind::Generic {
                pc: self.pc,
                registers: registers.into_boxed_slice(),
            },
            &inputs,
            Repr::None,
        );
        let lazy = self.lazy_state(None);
        self.graph.node_mut(node).lazy = Some(lazy);
        self.known.clobber_heap();
        if instruction.flow == Flow::Exit {
            // The operation completes the frame; nothing follows it.
            self.deopt(DeoptReason::Unsupported);
            return;
        }
        for &register in &instruction.writes {
            let value = self.add(Kind::LoadWindow(register), &[], Repr::Tagged);
            self.frame[usize::from(register)] = value;
        }
    }

    fn feedback(&self) -> otter_vm::feedback::ArithFeedback {
        self.view.instructions[self.pc as usize].arith_feedback()
    }

    fn exited_before(&self) -> bool {
        self.view.optimized_exit_reasons.contains_key(&self.pc)
    }

    fn visit_binary(&mut self, instruction: &Instruction, immediate: Option<i32>) {
        let feedback = self.feedback();
        let op = instruction.op;
        let lhs = self.read(instruction.reads[0]);
        let rhs = match immediate {
            Some(value) => self.constant_int32(value),
            None => self.read(instruction.reads[1]),
        };
        let bitwise = matches!(
            op,
            Op::BitwiseAnd
                | Op::BitwiseOr
                | Op::BitwiseXor
                | Op::Shl
                | Op::Shr
                | Op::Ushr
                | Op::BitwiseAndImm
        );
        let int32 = feedback.speculates_int32() && !self.exited_before();
        let numeric = feedback.is_numeric_only() || int32;
        if bitwise && numeric {
            let a = self.truncated_int32(lhs, int32);
            let b = self.truncated_int32(rhs, int32);
            // `>>>` is unsigned: once a result left the int32 range here, it
            // is computed as a double instead of checked again.
            if op == Op::Ushr && !int32 {
                let result = self.add(Kind::Uint32ShiftRightToFloat64, &[a, b], Repr::Float64);
                self.write(instruction.writes[0], result);
                return;
            }
            let kind = match op {
                Op::BitwiseAnd | Op::BitwiseAndImm => Kind::Int32BitAnd,
                Op::BitwiseOr => Kind::Int32BitOr,
                Op::BitwiseXor => Kind::Int32BitXor,
                Op::Shl => Kind::Int32ShiftLeft,
                Op::Shr => Kind::Int32ShiftRight,
                _ => Kind::Int32ShiftRightLogical,
            };
            let result = self.add(kind, &[a, b], Repr::Int32);
            self.write(instruction.writes[0], result);
            return;
        }
        if int32 {
            let a = self.int32(lhs);
            let b = self.int32(rhs);
            let kind = match op {
                Op::Add | Op::AddImm => Kind::Int32Add,
                Op::Sub | Op::SubImm => Kind::Int32Sub,
                Op::Mul => Kind::Int32Mul,
                Op::Div => Kind::Int32Div,
                _ => Kind::Int32Mod,
            };
            let result = self.add(kind, &[a, b], Repr::Int32);
            self.write(instruction.writes[0], result);
            return;
        }
        if numeric {
            let a = self.float64(lhs);
            let b = self.float64(rhs);
            let kind = match op {
                Op::Add | Op::AddImm => Kind::Float64Add,
                Op::Sub | Op::SubImm => Kind::Float64Sub,
                Op::Mul => Kind::Float64Mul,
                Op::Div => Kind::Float64Div,
                _ => Kind::Float64Mod,
            };
            let result = self.add(kind, &[a, b], Repr::Float64);
            self.write(instruction.writes[0], result);
            return;
        }
        self.generic(instruction);
    }

    /// An int32 operand of a bitwise operator: exact when the site only saw
    /// int32, `ToInt32` of any number otherwise.
    fn truncated_int32(&mut self, value: NodeId, int32: bool) -> NodeId {
        if int32 || self.graph.node(value).repr == Repr::Int32 {
            return self.int32(value);
        }
        let double = self.float64(value);
        self.add(Kind::TruncateFloat64ToInt32, &[double], Repr::Int32)
    }

    fn visit_increment(&mut self, instruction: &Instruction, delta: i32) {
        let feedback = self.feedback();
        let value = self.read(instruction.reads[0]);
        if feedback.speculates_int32() && !self.exited_before() {
            let a = self.int32(value);
            let b = self.constant_int32(delta);
            let result = self.add(Kind::Int32Add, &[a, b], Repr::Int32);
            self.write(instruction.writes[0], result);
        } else if feedback.is_numeric_only() {
            let a = self.float64(value);
            let b = self.constant_float64(f64::from(delta));
            let result = self.add(Kind::Float64Add, &[a, b], Repr::Float64);
            self.write(instruction.writes[0], result);
        } else {
            self.generic(instruction);
        }
    }

    /// Whether `value` is proved to be a Number on this path.
    fn is_number(&self, value: NodeId) -> bool {
        let node = self.graph.node(value);
        match (&node.kind, node.repr) {
            (_, Repr::Int32 | Repr::Float64) => return true,
            (Kind::ConstTagged(bits), _) => return tag::is_number_bits(*bits),
            _ => {}
        }
        self.known
            .get(value)
            .is_some_and(|info| info.number || info.int32.is_some() || info.float64.is_some())
    }

    /// `ToNumeric` / `ToNumber` of a Number is the value itself; anything
    /// else leaves through the check, or runs generically once it has.
    fn visit_to_numeric(&mut self, instruction: &Instruction) {
        let value = self.read(instruction.reads[0]);
        if self.is_number(value) {
            self.write(instruction.writes[0], value);
            return;
        }
        if self.exited_before() {
            self.generic(instruction);
            return;
        }
        self.add(Kind::CheckNumber, &[value], Repr::None);
        self.known.entry(value).number = true;
        self.write(instruction.writes[0], value);
    }

    fn visit_unary(&mut self, instruction: &Instruction) {
        let feedback = self.feedback();
        let value = self.read(instruction.reads[0]);
        let int32 = feedback.speculates_int32() && !self.exited_before();
        match instruction.op {
            Op::Neg if int32 => {
                let a = self.int32(value);
                let result = self.add(Kind::Int32Negate, &[a], Repr::Int32);
                self.write(instruction.writes[0], result);
            }
            Op::Neg if feedback.is_numeric_only() => {
                let a = self.float64(value);
                let result = self.add(Kind::Float64Negate, &[a], Repr::Float64);
                self.write(instruction.writes[0], result);
            }
            Op::BitwiseNot if int32 || feedback.is_numeric_only() => {
                let a = self.truncated_int32(value, int32);
                let result = self.add(Kind::Int32BitNot, &[a], Repr::Int32);
                self.write(instruction.writes[0], result);
            }
            _ => self.generic(instruction),
        }
    }

    fn condition_of(op: Op) -> Option<Condition> {
        Some(match op {
            Op::LessThan | Op::LessThanImm => Condition::Less,
            Op::LessEq => Condition::LessEqual,
            Op::GreaterThan => Condition::Greater,
            Op::GreaterEq => Condition::GreaterEqual,
            Op::Equal | Op::EqualImm => Condition::Equal,
            Op::NotEqual | Op::NotEqualImm => Condition::NotEqual,
            _ => return None,
        })
    }

    fn visit_compare(&mut self, instruction: &Instruction, immediate: Option<i32>) {
        let Some(condition) = Self::condition_of(instruction.op) else {
            self.generic(instruction);
            return;
        };
        let feedback = self.feedback();
        let int32 = feedback.speculates_int32() && !self.exited_before();
        let numeric = feedback.is_numeric_only() || int32;
        if !numeric {
            self.generic(instruction);
            return;
        }
        let lhs = self.read(instruction.reads[0]);
        let rhs = match immediate {
            Some(value) => self.constant_int32(value),
            None => self.read(instruction.reads[1]),
        };
        let (branch, inputs) = if int32 {
            let a = self.int32(lhs);
            let b = self.int32(rhs);
            (BranchKind::Int32(condition), [a, b])
        } else {
            let a = self.float64(lhs);
            let b = self.float64(rhs);
            (BranchKind::Float64(condition), [a, b])
        };
        let destination = instruction.writes[0];
        // Fuse with an immediately following conditional jump on the result
        // when nothing else reads it.
        let next_pc = self.pc + 1;
        if let Some(next) = self.analysis.instructions.get(next_pc as usize)
            && self.analysis.block_of[next_pc as usize] == self.current_bytecode_block
            && matches!(next.op, Op::JumpIfTrue | Op::JumpIfFalse)
            && next.reads.first() == Some(&destination)
            && let Flow::Branch { target } = next.flow
        {
            let after = next_pc + 1;
            let dead_after = (after as usize) >= self.analysis.instructions.len()
                || !self.analysis.live_at(after).contains(destination)
                    && !self.analysis.live_at(target).contains(destination);
            if dead_after {
                let value = match branch {
                    BranchKind::Int32(condition) => {
                        self.add(Kind::Int32Compare(condition), &inputs, Repr::Tagged)
                    }
                    _ => self.add(Kind::Float64Compare(condition), &inputs, Repr::Tagged),
                };
                // The comparison value stays defined for frame states of
                // the jump; the branch itself re-tests the operands.
                self.write(destination, value);
                self.pc = next_pc;
                let taken = self.analysis.block_of[target as usize];
                let fallthrough = self.analysis.block_of[after as usize];
                let (if_true, if_false) = if next.op == Op::JumpIfTrue {
                    (taken, fallthrough)
                } else {
                    (fallthrough, taken)
                };
                self.branch(branch, &inputs, if_true, if_false);
                return;
            }
        }
        let kind = match branch {
            BranchKind::Int32(condition) => Kind::Int32Compare(condition),
            _ => Kind::Float64Compare(condition),
        };
        let value = self.add(kind, &inputs, Repr::Tagged);
        self.write(destination, value);
    }

    fn visit_branch(&mut self, instruction: &Instruction) {
        let Flow::Branch { target } = instruction.flow else {
            unreachable!("a conditional jump has a target");
        };
        let value = self.read(instruction.reads[0]);
        let value = self.tagged(value);
        let taken = self.analysis.block_of[target as usize];
        let fallthrough = self.analysis.block_of[self.pc as usize + 1];
        match instruction.op {
            Op::JumpIfTrue => self.branch(BranchKind::Truthy, &[value], taken, fallthrough),
            Op::JumpIfFalse => self.branch(BranchKind::Truthy, &[value], fallthrough, taken),
            _ => self.branch(BranchKind::Nullish, &[value], taken, fallthrough),
        }
    }

    // ------------------------------------------------------------------
    // Property and context access
    // ------------------------------------------------------------------

    fn visit_load_property(&mut self, instruction: &Instruction) {
        let byte_pc = instruction.byte_pc;
        let Some(access) = super::feedback::own_data_load(self.view, byte_pc) else {
            self.generic(instruction);
            return;
        };
        let object = self.read(instruction.reads[0]);
        let object = self.tagged(object);
        self.check_shapes(object, &access.shapes, false);
        let base = self.add(Kind::LoadSlotBase, &[object], Repr::Word);
        let value = self.add(Kind::LoadTaggedField(access.offset), &[base], Repr::Tagged);
        self.write(instruction.writes[0], value);
    }

    fn visit_store_property(&mut self, instruction: &Instruction) {
        let Some(access) = super::feedback::own_data_store(self.view, instruction.byte_pc) else {
            self.generic(instruction);
            return;
        };
        let object = self.read(instruction.reads[0]);
        let object = self.tagged(object);
        let value = self.read(instruction.reads[1]);
        let value = self.tagged(value);
        self.check_shapes(object, &access.shapes, true);
        let base = self.add(Kind::LoadSlotBase, &[object], Repr::Word);
        self.add(
            Kind::StoreTaggedField(access.offset),
            &[base, value],
            Repr::None,
        );
        self.add(Kind::WriteBarrier, &[object, value], Repr::None);
        // A slot write keeps the object's shape.
        let info = self.known.entry(object);
        info.shapes = Some(access.shapes.clone());
        info.writable = true;
    }

    /// Prove `object` has one of `shapes`, unless already proved.
    fn check_shapes(&mut self, object: NodeId, shapes: &SmallVec<[u32; 4]>, writable: bool) {
        if let Some(info) = self.known.get(object)
            && let Some(known) = info.shapes.as_ref()
            && known.iter().all(|shape| shapes.contains(shape))
            && (info.writable || !writable)
        {
            return;
        }
        self.add(
            Kind::CheckShapes {
                shapes: shapes.clone(),
                writable,
            },
            &[object],
            Repr::None,
        );
        let info = self.known.entry(object);
        info.shapes = Some(shapes.clone());
        info.writable = writable;
        info.heap_object = true;
    }

    fn context_at_depth(&mut self, context: NodeId, depth: u16) -> NodeId {
        let mut context = context;
        for _ in 0..depth {
            context = self.add(Kind::LoadContextParent, &[context], Repr::Tagged);
        }
        context
    }

    fn visit_load_context_slot(&mut self, instruction: &Instruction) {
        let Some(coord) = instruction.imm32(2).and_then(ContextCoord::from_imm32) else {
            self.generic(instruction);
            return;
        };
        let context = self.read(instruction.reads[0]);
        let context = self.tagged(context);
        let context = self.context_at_depth(context, coord.depth);
        let offset = self.view.context_layout.slots_byte as i32 + i32::from(coord.slot) * 8;
        let value = self.add(Kind::LoadTaggedField(offset), &[context], Repr::Tagged);
        self.write(instruction.writes[0], value);
    }

    fn visit_store_context_slot(&mut self, instruction: &Instruction) {
        let Some(coord) = instruction.imm32(2).and_then(ContextCoord::from_imm32) else {
            self.generic(instruction);
            return;
        };
        let value = self.read(instruction.reads[0]);
        let value = self.tagged(value);
        let context = self.read(instruction.reads[1]);
        let context = self.tagged(context);
        let context = self.context_at_depth(context, coord.depth);
        let offset = self.view.context_layout.slots_byte as i32 + i32::from(coord.slot) * 8;
        self.add(
            Kind::StoreTaggedField(offset),
            &[context, value],
            Repr::None,
        );
        self.add(Kind::WriteBarrier, &[context, value], Repr::None);
    }
}
