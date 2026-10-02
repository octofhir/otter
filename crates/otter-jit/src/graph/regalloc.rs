//! Straight-forward linear register allocation over the graph's emission
//! order.
//!
//! # Contents
//! - [`Location`] — a general register, a floating-point register, a frame
//!   slot, or a constant rematerialized at the use.
//! - [`allocate`] — one forward walk assigning every input, result,
//!   temporary and deopt value a location, recording the moves that make the
//!   walk's register state real.
//! - [`Allocation`] — the per-node and per-edge result consumed by code
//!   generation.
//!
//! # Invariants
//! - Every value is stored to its spill slot right after its definition (a
//!   phi at its block's start), once it has one; a slot is therefore valid at
//!   every point its value is live, on every path.
//! - Tagged values spill to tagged slots, which are part of the frame window
//!   the collector traces; unboxed values spill to untagged slots below it.
//! - A call spills every value live past it and leaves no value in a
//!   register.
//! - A value used by a node's eager deopt stays live, and so keeps its
//!   register, until after the node's result is assigned: a result never
//!   overwrites a register an exit still reads.
//! - A block's entry register state is the state of its first predecessor in
//!   emission order, restricted to values live at the block; every other
//!   predecessor ends with moves that recreate it ([`Allocation::edges`]).
//!
//! # See also
//! - [`super::ir::Constraints`] — the per-node contract.
//! - [`super::arm64`] — emits the moves and nodes with these locations.

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use super::ir::{BlockId, Graph, InputPolicy, Kind, NodeId, Repr, ResultPolicy};

/// Where a value lives at one point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Location {
    Gp(u8),
    Fp(u8),
    /// A tagged slot of the window tail (index past the interpreter
    /// registers).
    TaggedSlot(u32),
    /// An untagged slot below the window.
    UntaggedSlot(u32),
    /// A constant node, rematerialized where it is used.
    Constant(NodeId),
}

impl Location {
    pub(crate) fn is_register(self) -> bool {
        matches!(self, Self::Gp(_) | Self::Fp(_))
    }
}

/// A move inserted before a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Move {
    pub(crate) from: Location,
    pub(crate) to: Location,
}

/// Locations of one node.
#[derive(Debug, Clone, Default)]
pub(crate) struct NodeAllocation {
    /// Sequential moves to run before the node.
    pub(crate) moves: Vec<Move>,
    pub(crate) inputs: SmallVec<[Location; 4]>,
    pub(crate) result: Option<Location>,
    pub(crate) gp_temps: SmallVec<[u8; 2]>,
    pub(crate) fp_temps: SmallVec<[u8; 1]>,
    /// Locations of the eager frame state's registers, in state order.
    pub(crate) eager: Vec<Location>,
    /// Locations of the lazy frame state's registers, in state order.
    pub(crate) lazy: Vec<Location>,
    /// The node's value is dead and nothing is emitted for it.
    pub(crate) skipped: bool,
    /// Registers holding live tagged and untagged values across the node,
    /// for nodes whose slow path saves them itself.
    pub(crate) live_registers: SmallVec<[(Location, Repr); 8]>,
}

/// Moves on one control edge, resolved in parallel at the end of the
/// predecessor.
#[derive(Debug, Clone, Default)]
pub(crate) struct EdgeMoves {
    /// Values the successor expects in other registers.
    pub(crate) moves: Vec<Move>,
    /// `(phi, input location)`: the phi's own location is final only once
    /// the successor has been allocated, so code generation pairs them.
    pub(crate) phi_inputs: Vec<(NodeId, Location)>,
}

/// The finished allocation.
#[derive(Debug, Default)]
pub(crate) struct Allocation {
    pub(crate) nodes: Vec<NodeAllocation>,
    /// Spill slot of each value that has one.
    pub(crate) spill: FxHashMap<NodeId, Location>,
    /// Moves per `(predecessor, successor)` edge.
    pub(crate) edges: FxHashMap<(BlockId, BlockId), EdgeMoves>,
    pub(crate) tagged_slots: u32,
    pub(crate) untagged_slots: u32,
}

impl Allocation {
    pub(crate) fn node(&self, id: NodeId) -> &NodeAllocation {
        &self.nodes[id.0 as usize]
    }
}

/// Allocatable general registers.
pub(crate) const GP_REGISTERS: &[u8] = &[
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
];
/// Allocatable floating-point registers.
pub(crate) const FP_REGISTERS: &[u8] = &[
    0, 1, 2, 3, 4, 5, 6, 7, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30,
];

/// Register file state of one class.
#[derive(Debug, Clone)]
struct RegisterFile {
    values: [Option<NodeId>; 32],
    blocked: u32,
    allocatable: u32,
}

impl RegisterFile {
    fn new(registers: &[u8]) -> Self {
        Self {
            values: [None; 32],
            blocked: 0,
            allocatable: registers.iter().fold(0, |mask, &r| mask | (1 << r)),
        }
    }

    fn free_unblocked(&self) -> impl Iterator<Item = u8> + '_ {
        (0..32u8).filter(move |&r| {
            self.allocatable & (1 << r) != 0
                && self.values[r as usize].is_none()
                && self.blocked & (1 << r) == 0
        })
    }

    fn holder(&self, register: u8) -> Option<NodeId> {
        self.values[register as usize]
    }
}

struct ValueState {
    gp: u32,
    fp: u32,
    /// Sorted use positions.
    uses: Vec<u32>,
}

struct Allocator<'g> {
    graph: &'g Graph,
    layout: &'g [BlockId],
    position: Vec<u32>,
    block_start: FxHashMap<BlockId, u32>,
    values: FxHashMap<NodeId, ValueState>,
    gp: RegisterFile,
    fp: RegisterFile,
    out: Allocation,
    /// Released slots with the position their last value died at.
    free_tagged: Vec<(u32, u32)>,
    free_untagged: Vec<(u32, u32)>,
    released: rustc_hash::FxHashSet<NodeId>,
    entry_states: FxHashMap<BlockId, Vec<(NodeId, Location)>>,
    exit_states: FxHashMap<BlockId, Vec<(NodeId, Location)>>,
    current: NodeId,
}

const NO_POSITION: u32 = u32::MAX;

/// Allocate `graph` in `layout` order.
pub(crate) fn allocate(graph: &Graph, layout: &[BlockId]) -> Allocation {
    let mut allocator = Allocator {
        graph,
        layout,
        position: vec![NO_POSITION; graph.nodes.len()],
        block_start: FxHashMap::default(),
        values: FxHashMap::default(),
        gp: RegisterFile::new(GP_REGISTERS),
        fp: RegisterFile::new(FP_REGISTERS),
        out: Allocation {
            nodes: vec![NodeAllocation::default(); graph.nodes.len()],
            ..Allocation::default()
        },
        free_tagged: Vec::new(),
        free_untagged: Vec::new(),
        released: rustc_hash::FxHashSet::default(),
        entry_states: FxHashMap::default(),
        exit_states: FxHashMap::default(),
        current: NodeId(0),
    };
    allocator.number();
    allocator.collect_uses();
    allocator.walk();
    allocator.out
}

impl<'g> Allocator<'g> {
    // ------------------------------------------------------------------
    // Positions and uses
    // ------------------------------------------------------------------

    fn number(&mut self) {
        let mut next = 1u32;
        for &block in self.layout {
            let data = self.graph.block(block);
            self.block_start.insert(block, next);
            for &phi in &data.phis {
                self.position[phi.0 as usize] = next;
            }
            next += 1;
            for &node in &data.body {
                self.position[node.0 as usize] = next;
                next += 1;
            }
            if let Some(control) = data.control {
                self.position[control.0 as usize] = next;
                next += 1;
            }
        }
    }

    fn pos(&self, node: NodeId) -> u32 {
        self.position[node.0 as usize]
    }

    fn add_use(&mut self, value: NodeId, at: u32) {
        self.values
            .entry(value)
            .or_insert_with(|| ValueState {
                gp: 0,
                fp: 0,
                uses: Vec::new(),
            })
            .uses
            .push(at);
    }

    fn collect_uses(&mut self) {
        let graph = self.graph;
        for &block in self.layout {
            let data = graph.block(block);
            for (index, &predecessor) in data.predecessors.iter().enumerate() {
                let Some(control) = graph.block(predecessor).control else {
                    continue;
                };
                let at = self.pos(control);
                for &phi in &data.phis {
                    if let Some(&input) = graph.node(phi).inputs.get(index) {
                        self.add_use(input, at);
                    }
                }
            }
            for &node in data.body.iter().chain(data.control.iter()) {
                let at = self.pos(node);
                let data = graph.node(node);
                for &input in &data.inputs {
                    self.add_use(input, at);
                }
                for state in data.eager.iter().chain(data.lazy.iter()) {
                    for &(_, value) in &graph.frame_state(*state).registers {
                        if value != node {
                            self.add_use(value, at);
                        }
                    }
                }
            }
        }
        // A value used inside a loop but defined before it stays live until
        // every back edge of the loop.
        let mut loops = Vec::new();
        for &block in self.layout {
            let data = graph.block(block);
            if !data.is_loop {
                continue;
            }
            let start = self.block_start[&block];
            let end = data
                .predecessors
                .iter()
                .filter_map(|&predecessor| graph.block(predecessor).control)
                .map(|control| self.pos(control))
                .filter(|&at| at != NO_POSITION && at >= start)
                .max();
            if let Some(end) = end {
                loops.push((start, end));
            }
        }
        let definitions: Vec<(NodeId, u32)> = self
            .values
            .keys()
            .map(|&value| (value, self.pos(value)))
            .collect();
        loop {
            let mut changed = false;
            for &(start, end) in &loops {
                for &(value, defined) in &definitions {
                    let state = self.values.get_mut(&value).expect("a used value");
                    let defined_before = defined == NO_POSITION || defined < start;
                    if !defined_before {
                        continue;
                    }
                    let used_inside = state.uses.iter().any(|&at| at >= start && at <= end);
                    let last = state.uses.iter().copied().max().unwrap_or(0);
                    if used_inside && last < end {
                        state.uses.push(end);
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        for state in self.values.values_mut() {
            state.uses.sort_unstable();
            state.uses.dedup();
        }
    }

    fn last_use(&self, value: NodeId) -> u32 {
        self.values
            .get(&value)
            .and_then(|state| state.uses.last().copied())
            .unwrap_or(0)
    }

    /// Next use of `value` strictly after `at`, if any.
    fn next_use_after(&self, value: NodeId, at: u32) -> Option<u32> {
        let state = self.values.get(&value)?;
        let index = state.uses.partition_point(|&use_at| use_at <= at);
        state.uses.get(index).copied()
    }

    fn is_live_after(&self, value: NodeId, at: u32) -> bool {
        self.next_use_after(value, at).is_some()
    }

    // ------------------------------------------------------------------
    // Register state
    // ------------------------------------------------------------------

    fn is_float(&self, value: NodeId) -> bool {
        self.graph.node(value).repr.is_float()
    }

    fn file(&mut self, float: bool) -> &mut RegisterFile {
        if float { &mut self.fp } else { &mut self.gp }
    }

    fn registers_of(&self, value: NodeId) -> u32 {
        self.values.get(&value).map_or(0, |state| {
            if self.graph.node(value).repr.is_float() {
                state.fp
            } else {
                state.gp
            }
        })
    }

    fn register_location(float: bool, register: u8) -> Location {
        if float {
            Location::Fp(register)
        } else {
            Location::Gp(register)
        }
    }

    /// Current location of `value` (register preferred).
    fn location_of(&self, value: NodeId) -> Option<Location> {
        if self.graph.node(value).kind.is_constant() {
            return Some(Location::Constant(value));
        }
        let registers = self.registers_of(value);
        if registers != 0 {
            return Some(Self::register_location(
                self.is_float(value),
                registers.trailing_zeros() as u8,
            ));
        }
        self.out.spill.get(&value).copied()
    }

    fn bind(&mut self, value: NodeId, register: u8) {
        let float = self.is_float(value);
        self.file(float).values[register as usize] = Some(value);
        let state = self.values.get_mut(&value).expect("a tracked value");
        if float {
            state.fp |= 1 << register;
        } else {
            state.gp |= 1 << register;
        }
    }

    fn unbind(&mut self, float: bool, register: u8) {
        let Some(value) = self.file(float).values[register as usize].take() else {
            return;
        };
        if let Some(state) = self.values.get_mut(&value) {
            if float {
                state.fp &= !(1 << register);
            } else {
                state.gp &= !(1 << register);
            }
        }
    }

    fn ensure_spill_slot(&mut self, value: NodeId) -> Location {
        if let Some(&slot) = self.out.spill.get(&value) {
            return slot;
        }
        // The slot is written at the definition, so a released slot is
        // reusable only when its last value died before this one exists.
        let defined = match self.pos(value) {
            NO_POSITION => 0,
            at => at,
        };
        let take = |free: &mut Vec<(u32, u32)>| {
            let index = free.iter().position(|&(_, died)| died < defined)?;
            Some(free.swap_remove(index).0)
        };
        let slot = if self.graph.node(value).repr == Repr::Tagged {
            let index = take(&mut self.free_tagged).unwrap_or_else(|| {
                self.out.tagged_slots += 1;
                self.out.tagged_slots - 1
            });
            Location::TaggedSlot(index)
        } else {
            let index = take(&mut self.free_untagged).unwrap_or_else(|| {
                self.out.untagged_slots += 1;
                self.out.untagged_slots - 1
            });
            Location::UntaggedSlot(index)
        };
        self.out.spill.insert(value, slot);
        slot
    }

    /// Make `value` reachable without `register`: keep it in another
    /// register, its slot, or a constant.
    fn evict(&mut self, float: bool, register: u8) {
        let Some(value) = self.file(float).holder(register) else {
            return;
        };
        self.unbind(float, register);
        let at = self.pos(self.current);
        if !self.is_live_after(value, at.saturating_sub(1)) {
            return;
        }
        if self.graph.node(value).kind.is_constant() || self.registers_of(value) != 0 {
            return;
        }
        self.ensure_spill_slot(value);
    }

    /// A free unblocked register, evicting the value used furthest away.
    fn take_register(&mut self, float: bool) -> u8 {
        if let Some(register) = self.file(float).free_unblocked().next() {
            return register;
        }
        let at = self.pos(self.current);
        let file = if float { &self.fp } else { &self.gp };
        let mut best: Option<(u8, u32)> = None;
        for register in 0..32u8 {
            if file.allocatable & (1 << register) == 0 || file.blocked & (1 << register) != 0 {
                continue;
            }
            let Some(value) = file.values[register as usize] else {
                continue;
            };
            let distance = self.next_use_after(value, at.saturating_sub(1)).unwrap_or(u32::MAX);
            if best.is_none_or(|(_, best_distance)| distance > best_distance) {
                best = Some((register, distance));
            }
        }
        let (register, _) = best.expect("an unblocked allocatable register");
        self.evict(float, register);
        register
    }

    fn block_register(&mut self, float: bool, register: u8) {
        self.file(float).blocked |= 1 << register;
    }

    fn emit_move(&mut self, from: Location, to: Location) {
        if from != to {
            let current = self.current.0 as usize;
            self.out.nodes[current].moves.push(Move { from, to });
        }
    }

    /// Put `value` into a register of its class, preferring one it already
    /// occupies; the register is blocked for the current node.
    fn input_register(&mut self, value: NodeId) -> u8 {
        let float = self.is_float(value);
        let registers = self.registers_of(value);
        if registers != 0 {
            let register = registers.trailing_zeros() as u8;
            self.block_register(float, register);
            return register;
        }
        let from = self.location_of(value).expect("a live value has a location");
        let register = self.take_register(float);
        self.bind(value, register);
        self.block_register(float, register);
        self.emit_move(from, Self::register_location(float, register));
        register
    }

    /// Put `value` into exactly `register`.
    fn fixed_input(&mut self, value: NodeId, register: u8) {
        let float = self.is_float(value);
        if self.file(float).holder(register) == Some(value) {
            self.block_register(float, register);
            return;
        }
        let from = self.location_of(value).expect("a live value has a location");
        if self.file(float).holder(register).is_some() {
            // Move the occupant aside when it stays live.
            let occupant = self.file(float).holder(register).expect("an occupant");
            let at = self.pos(self.current);
            self.unbind(float, register);
            if self.is_live_after(occupant, at.saturating_sub(1))
                && !self.graph.node(occupant).kind.is_constant()
                && self.registers_of(occupant) == 0
            {
                let free = self.file(float).free_unblocked().find(|&r| r != register);
                if let Some(free) = free {
                    self.emit_move(
                        Self::register_location(float, register),
                        Self::register_location(float, free),
                    );
                    self.bind(occupant, free);
                } else {
                    self.ensure_spill_slot(occupant);
                }
            }
        }
        self.emit_move(from, Self::register_location(float, register));
        self.bind(value, register);
        self.block_register(float, register);
    }

    /// Release every register whose value has no use after `at`.
    fn release_dead(&mut self, at: u32) {
        for float in [false, true] {
            for register in 0..32u8 {
                let Some(value) = self.file(float).holder(register) else {
                    continue;
                };
                if !self.is_live_after(value, at) {
                    self.unbind(float, register);
                }
            }
        }
        let dead: Vec<(NodeId, Location)> = self
            .out
            .spill
            .iter()
            .filter(|(value, _)| !self.released.contains(*value) && self.last_use(**value) <= at)
            .map(|(&value, &slot)| (value, slot))
            .collect();
        for (value, slot) in dead {
            // A slot lives as long as its value, then is recycled.
            self.released.insert(value);
            match slot {
                Location::TaggedSlot(index) => self.free_tagged.push((index, at)),
                Location::UntaggedSlot(index) => self.free_untagged.push((index, at)),
                _ => {}
            }
        }
    }

    // ------------------------------------------------------------------
    // The walk
    // ------------------------------------------------------------------

    fn walk(&mut self) {
        let graph = self.graph;
        for (layout_index, &block) in self.layout.iter().enumerate() {
            let data = graph.block(block);
            let start = self.block_start[&block];
            if layout_index != 0 {
                self.restore_entry_state(block, start);
            }
            for &phi in &data.phis {
                self.current = phi;
                self.allocate_phi(phi, block);
            }
            self.gp.blocked = 0;
            self.fp.blocked = 0;
            for &node in &data.body {
                self.current = node;
                self.allocate_node(node);
            }
            if let Some(control) = data.control {
                self.current = control;
                self.allocate_control(control, block);
            }
        }
    }

    fn restore_entry_state(&mut self, block: BlockId, start: u32) {
        for register in 0..32u8 {
            self.unbind(false, register);
            self.unbind(true, register);
        }
        let Some(state) = self.entry_states.get(&block).cloned() else {
            return;
        };
        for (value, location) in state {
            if !self.is_live_after(value, start.saturating_sub(1)) {
                continue;
            }
            match location {
                Location::Gp(register) | Location::Fp(register) => self.bind(value, register),
                _ => {}
            }
        }
    }

    fn allocate_phi(&mut self, phi: NodeId, block: BlockId) {
        let float = self.is_float(phi);
        if !self.values.contains_key(&phi) {
            // A phi nothing reads.
            self.out.nodes[phi.0 as usize].skipped = true;
            return;
        }
        let first = self.graph.block(block).predecessors.first().copied();
        let preferred = first.and_then(|predecessor| {
            let input = *self.graph.node(phi).inputs.first()?;
            let exit = self.exit_states.get(&predecessor)?;
            exit.iter()
                .find(|(value, _)| *value == input)
                .and_then(|(_, location)| match *location {
                    Location::Gp(register) if !float => Some(register),
                    Location::Fp(register) if float => Some(register),
                    _ => None,
                })
        });
        let register = preferred
            .filter(|&register| {
                let file = if float { &self.fp } else { &self.gp };
                file.allocatable & (1 << register) != 0 && file.holder(register).is_none()
            })
            .or_else(|| self.file(float).free_unblocked().next());
        let location = match register {
            Some(register) => {
                self.bind(phi, register);
                Self::register_location(float, register)
            }
            None => self.ensure_spill_slot(phi),
        };
        self.out.nodes[phi.0 as usize].result = Some(location);
    }

    fn allocate_node(&mut self, node: NodeId) {
        let graph = self.graph;
        let data = graph.node(node);
        let at = self.pos(node);
        let properties = data.kind.properties();
        if data.repr != Repr::None
            && !properties.effectful
            && !properties.eager_deopt
            && !self.values.contains_key(&node)
        {
            self.out.nodes[node.0 as usize].skipped = true;
            return;
        }
        let constraints = data.kind.constraints(data.inputs.len());
        self.gp.blocked = 0;
        self.fp.blocked = 0;
        // Fixed inputs first, so arbitrary ones avoid their registers.
        let mut inputs: SmallVec<[Location; 4]> =
            data.inputs.iter().map(|_| Location::Gp(0)).collect();
        for (index, (&input, policy)) in data.inputs.iter().zip(&constraints.inputs).enumerate() {
            if let InputPolicy::FixedGp(register) = *policy {
                self.fixed_input(input, register);
                inputs[index] = Location::Gp(register);
            }
        }
        for (index, (&input, policy)) in data.inputs.iter().zip(&constraints.inputs).enumerate() {
            match *policy {
                InputPolicy::Register => {
                    let register = self.input_register(input);
                    inputs[index] = Self::register_location(self.is_float(input), register);
                }
                InputPolicy::Any => {
                    inputs[index] = self.location_of(input).expect("a live input");
                }
                InputPolicy::FixedGp(_) => {}
            }
        }
        self.out.nodes[node.0 as usize].inputs = inputs.clone();
        if properties.call {
            self.spill_all_live(at);
        }
        // Temporaries.
        let mut gp_temps = SmallVec::new();
        for _ in 0..constraints.gp_temps {
            let register = self.take_register(false);
            self.block_register(false, register);
            gp_temps.push(register);
        }
        let mut fp_temps = SmallVec::new();
        for _ in 0..constraints.fp_temps {
            let register = self.take_register(true);
            self.block_register(true, register);
            fp_temps.push(register);
        }
        self.out.nodes[node.0 as usize].gp_temps = gp_temps;
        self.out.nodes[node.0 as usize].fp_temps = fp_temps;
        // Inputs whose last use is this node die before the result unless an
        // eager deopt of this node still reads them.
        let eager_values: SmallVec<[NodeId; 8]> = data
            .eager
            .iter()
            .flat_map(|&state| graph.frame_state(state).registers.iter().map(|&(_, v)| v))
            .collect();
        for &input in &data.inputs {
            if !self.is_live_after(input, at) && !eager_values.contains(&input) {
                let float = self.is_float(input);
                let mut registers = self.registers_of(input);
                while registers != 0 {
                    let register = registers.trailing_zeros() as u8;
                    registers &= registers - 1;
                    self.unbind(float, register);
                    // A dying input's register may hold the result.
                    self.file(float).blocked &= !(1 << register);
                }
            }
        }
        if !properties.call {
            // Live registers a slow path must preserve.
            let mut live = SmallVec::new();
            for float in [false, true] {
                for register in 0..32u8 {
                    if let Some(value) = self.file(float).holder(register)
                        && self.is_live_after(value, at)
                    {
                        live.push((
                            Self::register_location(float, register),
                            self.graph.node(value).repr,
                        ));
                    }
                }
            }
            self.out.nodes[node.0 as usize].live_registers = live;
        }
        // Result.
        if data.repr != Repr::None && constraints.result != ResultPolicy::None {
            let float = data.repr.is_float();
            let register = match constraints.result {
                ResultPolicy::FixedGp(register) => {
                    self.evict(false, register);
                    register
                }
                ResultPolicy::SameAsInput(index) => match inputs[index as usize] {
                    Location::Gp(register) | Location::Fp(register) => {
                        self.evict(float, register);
                        register
                    }
                    _ => self.take_register(float),
                },
                ResultPolicy::Register | ResultPolicy::None => self.take_register(float),
            };
            if self.values.contains_key(&node) {
                self.bind(node, register);
            }
            self.block_register(float, register);
            self.out.nodes[node.0 as usize].result =
                Some(Self::register_location(float, register));
        }
        // Deopt locations.
        if let Some(state) = data.eager {
            let locations = graph
                .frame_state(state)
                .registers
                .iter()
                .map(|&(_, value)| self.location_of(value).expect("a deopt value is live"))
                .collect();
            self.out.nodes[node.0 as usize].eager = locations;
        }
        if let Some(state) = data.lazy {
            let locations = graph
                .frame_state(state)
                .registers
                .iter()
                .map(|&(_, value)| {
                    if value == node {
                        self.out.nodes[node.0 as usize].result.expect("a result")
                    } else {
                        // After a call nothing is in a register.
                        self.out
                            .spill
                            .get(&value)
                            .copied()
                            .or_else(|| self.location_of(value))
                            .expect("a lazy deopt value is spilled")
                    }
                })
                .collect();
            self.out.nodes[node.0 as usize].lazy = locations;
        }
        self.release_dead(at);
        self.gp.blocked = 0;
        self.fp.blocked = 0;
    }

    /// Give every value live past `at` a slot and empty both register
    /// files.
    fn spill_all_live(&mut self, at: u32) {
        for float in [false, true] {
            for register in 0..32u8 {
                let Some(value) = self.file(float).holder(register) else {
                    continue;
                };
                self.unbind(float, register);
                if self.is_live_after(value, at) && !self.graph.node(value).kind.is_constant() {
                    self.ensure_spill_slot(value);
                }
            }
        }
        // Values a frame state of this call reads keep a slot even when
        // nothing reads them after it: every register is gone at the exits.
        let current = self.graph.node(self.current);
        let state_values: Vec<NodeId> = current
            .eager
            .iter()
            .chain(current.lazy.iter())
            .flat_map(|&state| self.graph.frame_state(state).registers.iter().map(|&(_, v)| v))
            .collect();
        for value in state_values {
            if value != self.current && !self.graph.node(value).kind.is_constant() {
                self.ensure_spill_slot(value);
            }
        }
    }

    fn allocate_control(&mut self, control: NodeId, block: BlockId) {
        let graph = self.graph;
        let data = graph.node(control);
        let at = self.pos(control);
        let constraints = data.kind.constraints(data.inputs.len());
        self.gp.blocked = 0;
        self.fp.blocked = 0;
        let mut inputs: SmallVec<[Location; 4]> = SmallVec::new();
        for (&input, policy) in data.inputs.iter().zip(&constraints.inputs) {
            let location = match *policy {
                InputPolicy::FixedGp(register) => {
                    self.fixed_input(input, register);
                    Location::Gp(register)
                }
                InputPolicy::Register => {
                    let register = self.input_register(input);
                    Self::register_location(self.is_float(input), register)
                }
                InputPolicy::Any => self.location_of(input).expect("a live input"),
            };
            inputs.push(location);
        }
        self.out.nodes[control.0 as usize].inputs = inputs;
        if let Some(state) = data.eager {
            let locations = graph
                .frame_state(state)
                .registers
                .iter()
                .map(|&(_, value)| self.location_of(value).expect("a deopt value is live"))
                .collect();
            self.out.nodes[control.0 as usize].eager = locations;
        }
        let targets: SmallVec<[BlockId; 2]> = match data.kind {
            Kind::Jump(target) | Kind::JumpLoop(target) => smallvec::smallvec![target],
            Kind::Branch {
                if_true, if_false, ..
            } => smallvec::smallvec![if_true, if_false],
            _ => SmallVec::new(),
        };
        // Phi inputs are read at this edge: take their locations before the
        // values that die here are released.
        let mut phi_inputs: FxHashMap<BlockId, Vec<(NodeId, NodeId, Location)>> =
            FxHashMap::default();
        for &target in &targets {
            let Some(index) = graph
                .block(target)
                .predecessors
                .iter()
                .position(|&predecessor| predecessor == block)
            else {
                continue;
            };
            for &phi in &graph.block(target).phis {
                if let Some(&input) = graph.node(phi).inputs.get(index)
                    && let Some(location) = self.location_of(input)
                {
                    phi_inputs.entry(target).or_default().push((phi, input, location));
                }
            }
        }
        // A back edge's poll saves every occupied register around its call.
        let mut occupied = SmallVec::new();
        for (float, file) in [(false, &self.gp), (true, &self.fp)] {
            for register in 0..32u8 {
                if let Some(value) = file.holder(register) {
                    occupied.push((
                        Self::register_location(float, register),
                        self.graph.node(value).repr,
                    ));
                }
            }
        }
        self.out.nodes[control.0 as usize].live_registers = occupied;
        // The edge reads every value live at its target, including values
        // whose last use is this jump (a back edge extends the loop's
        // live-through values to here): take the state before releasing.
        let snapshot = self.snapshot();
        self.release_dead(at);
        for target in targets {
            let target_start = self.block_start[&target];
            let live: Vec<(NodeId, Location)> = snapshot
                .iter()
                .copied()
                .filter(|&(value, _)| self.is_live_after(value, target_start.saturating_sub(1)))
                .collect();
            let mut edge = EdgeMoves::default();
            let mut exit = live.clone();
            for &(phi, input, location) in phi_inputs.get(&target).into_iter().flatten() {
                exit.push((input, location));
                edge.phi_inputs.push((phi, location));
            }
            if let Some(entry) = self.entry_states.get(&target) {
                // A later predecessor recreates the target's entry state. A
                // value this path never defined cannot be read there.
                for &(value, to) in entry {
                    // Not in a register here: its slot, or the constant
                    // itself, recreates it.
                    let from = exit
                        .iter()
                        .find(|(candidate, _)| *candidate == value)
                        .map(|&(_, location)| location)
                        .or_else(|| self.out.spill.get(&value).copied())
                        .or_else(|| {
                            self.graph
                                .node(value)
                                .kind
                                .is_constant()
                                .then_some(Location::Constant(value))
                        });
                    if let Some(from) = from
                        && from != to
                    {
                        edge.moves.push(Move { from, to });
                    }
                }
            } else {
                self.entry_states.insert(target, live);
            }
            self.exit_states.insert(block, exit);
            self.out.edges.insert((block, target), edge);
        }
    }

    fn snapshot(&self) -> Vec<(NodeId, Location)> {
        let mut state = Vec::new();
        for (float, file) in [(false, &self.gp), (true, &self.fp)] {
            for register in 0..32u8 {
                if let Some(value) = file.holder(register) {
                    state.push((value, Self::register_location(float, register)));
                }
            }
        }
        state
    }
}
